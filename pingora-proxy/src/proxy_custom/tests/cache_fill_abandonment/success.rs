// Copyright 2026 Cloudflare, Inc.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use super::*;

#[derive(Clone, Copy)]
enum CustomDownstreamWritePath {
    Direct,
    ProxyTasks,
}

const WRITE_FAILURE_BODY: Bytes = Bytes::from_static(&[b'x'; 8 * 1024]);

/// What a cache fill whose client stopped reading its body left behind.
struct WriteFailureOutcome {
    result: Result<bool>,
    abandonments: usize,
    abandonments_with_pending_tasks: usize,
    pipe_state: u8,
    admission_active: bool,
    released_writers: usize,
    cached_body: Option<Vec<u8>>,
}

/// Run a streaming cache miss whose client stops reading: writing the body to it on
/// `downstream_write_path` times out, then the client closes and the upstream ends its response.
async fn run_downstream_write_failure_during_cache_fill(
    path: &str,
    downstream_write_path: CustomDownstreamWritePath,
    handles_abandonment: bool,
) -> WriteFailureOutcome {
    let (mut session, mut client) = cache_miss_session_for_request(
        &format!("GET {path} HTTP/1.1\r\nHost: example.com\r\n\r\n"),
        handles_abandonment,
    )
    .await;
    session.set_write_timeout(Some(Duration::from_millis(100)));
    if matches!(downstream_write_path, CustomDownstreamWritePath::ProxyTasks) {
        session.set_proxy_tasks_enabled(true);
    }

    let observed = CacheFillCallbackProxy::default();
    let proxy = HttpProxy::new(observed.clone(), Arc::new(ServerConf::default()));
    let (
        channels,
        CustomLoopDriver {
            tasks: task_tx,
            pipe_state,
            ..
        },
    ) = CustomLoop::new();
    // An abandoned fill must end on the producer's terminal task while the producer still holds
    // its sender. Otherwise the loop ends once the producer drops its sender after that task.
    let producer_sender = handles_abandonment.then(|| task_tx.clone());

    let proxy_loop = channels.run(&proxy, &mut session, Box::new(NoopBodyWriter));
    let drive = async {
        task_tx.send(cacheable_response_header()).await.unwrap();
        read_response_head(&mut client).await;
        task_tx
            .send(HttpTask::Body(Some(WRITE_FAILURE_BODY), false))
            .await
            .unwrap();

        tokio::time::timeout(
            Duration::from_secs(5),
            observed.downstream_error_notify.notified(),
        )
        .await
        .expect("custom proxy did not ignore the failed downstream write for the cache fill");
        drop(client);
        task_tx
            .send(HttpTask::Body(Some(Bytes::new()), true))
            .await
            .unwrap();
        task_tx.send(HttpTask::Done).await.unwrap();
        drop(task_tx);
    };

    let (result, ()) = tokio::time::timeout(Duration::from_secs(10), async {
        tokio::join!(proxy_loop, drive)
    })
    .await
    .expect("custom proxy loop did not finish");
    drop(producer_sender);
    let admission_active = session.cache.is_cache_admission_active();
    drop(session);
    WriteFailureOutcome {
        result,
        abandonments: count(&observed.calls),
        abandonments_with_pending_tasks: count(&observed.calls_with_pending_tasks),
        pipe_state: pipe_state.load(Ordering::Acquire),
        admission_active,
        released_writers: WRITER_DEMAND_CACHE.released_writers(path),
        cached_body: cached_response(path).await.map(|(_, body)| body),
    }
}

/// Run a write failure on `downstream_write_path` with abandonment handling at `path`, then
/// without it at `{path}-ungated`, and check what each run left behind.
async fn assert_write_failure_abandons_only_with_handling(
    path: &str,
    downstream_write_path: CustomDownstreamWritePath,
) {
    let outcome =
        run_downstream_write_failure_during_cache_fill(path, downstream_write_path, true).await;
    assert!(!outcome
        .result
        .expect("custom cache fill returned a downstream write error"));
    assert_eq!(outcome.abandonments, 1);
    assert_eq!(
        outcome.abandonments_with_pending_tasks,
        usize::from(matches!(
            downstream_write_path,
            CustomDownstreamWritePath::ProxyTasks
        )),
        "the callback did not run from the selected downstream write path"
    );
    assert_eq!(outcome.pipe_state, PipeState::DownstreamComplete as u8);
    assert_eq!(outcome.released_writers, 1);
    assert!(!outcome.admission_active);
    assert_eq!(
        outcome.cached_body.as_deref(),
        Some(WRITE_FAILURE_BODY.as_ref())
    );

    let outcome = run_downstream_write_failure_during_cache_fill(
        &format!("{path}-ungated"),
        downstream_write_path,
        false,
    )
    .await;
    let error = outcome
        .result
        .expect_err("the failed downstream write was swallowed");
    assert_eq!(error.etype(), &WriteError);
    assert_eq!(outcome.abandonments, 0);
    assert_eq!(outcome.pipe_state, PipeState::Active as u8);
    assert_eq!(outcome.released_writers, 0);
    assert!(!outcome.admission_active);
    assert_eq!(
        outcome.cached_body.as_deref(),
        Some(WRITE_FAILURE_BODY.as_ref())
    );
}

// Without abandonment handling the fill completes, then the request fails; no demand release.
#[tokio::test]
async fn custom_downstream_abandonment_during_cache_fill_after_direct_write_failure() {
    assert_write_failure_abandons_only_with_handling(
        "/custom-fill-direct",
        CustomDownstreamWritePath::Direct,
    )
    .await;
}

// Without abandonment handling the fill completes, then the request fails; no demand release.
#[tokio::test]
async fn custom_downstream_abandonment_during_cache_fill_after_proxy_task_failure() {
    assert_write_failure_abandons_only_with_handling(
        "/custom-fill-proxy-tasks",
        CustomDownstreamWritePath::ProxyTasks,
    )
    .await;
}

const EARLY_DEPARTURE_BODY: Bytes = Bytes::from_static(b"cached body");

/// What a cache fill whose client left before its response header left behind.
struct EarlyDepartureOutcome {
    result: Result<bool>,
    /// Abandonments the hook saw once the proxy had handled the departure, before any response.
    abandonments_before_response: usize,
    abandonments: usize,
    abandonments_with_pending_tasks: usize,
    downstream_informational_responses: usize,
    downstream_abandoned: bool,
    admission_active: bool,
    released_writers: usize,
    cached_body: Option<Vec<u8>>,
}

/// Run a cache miss for a POST whose client leaves before sending its body. Once the proxy has
/// handled that, the upstream sends an informational response, then the cacheable response.
async fn run_client_leaves_before_response_header(
    path: &str,
    handles_abandonment: bool,
) -> EarlyDepartureOutcome {
    let (mut session, client) = cache_miss_session_for_request(
        &format!("POST {path} HTTP/1.1\r\nHost: example.com\r\nContent-Length: 1\r\n\r\n"),
        handles_abandonment,
    )
    .await;

    let observed = CacheFillCallbackProxy::default();
    let proxy = HttpProxy::new(observed.clone(), Arc::new(ServerConf::default()));
    let (channels, CustomLoopDriver { tasks: task_tx, .. }) = CustomLoop::new();
    let mut abandonments_before_response = None;

    let proxy_loop = channels.run(&proxy, &mut session, Box::new(NoopBodyWriter));
    let drive = async {
        drop(client);
        tokio::time::timeout(
            Duration::from_secs(5),
            observed.downstream_error_notify.notified(),
        )
        .await
        .expect("custom proxy did not observe the downstream close");
        abandonments_before_response = Some(count(&observed.calls));

        let early_hints = ResponseHeader::build(103, None).unwrap();
        task_tx
            .send(HttpTask::Header(Box::new(early_hints), false))
            .await
            .unwrap();
        tokio::time::timeout(
            Duration::from_secs(5),
            observed.upstream_informational_response_notify.notified(),
        )
        .await
        .expect("custom proxy did not run the upstream informational response hook");

        // A loop that failed the request has dropped its task receiver; its result says why.
        let _ = task_tx.send(cacheable_response_header()).await;
        let _ = task_tx
            .send(HttpTask::Body(Some(EARLY_DEPARTURE_BODY), true))
            .await;
        let _ = task_tx.send(HttpTask::Done).await;
        drop(task_tx);
    };

    let (result, ()) = tokio::time::timeout(Duration::from_secs(10), async {
        tokio::join!(proxy_loop, drive)
    })
    .await
    .expect("custom proxy loop did not finish");
    let downstream_abandoned = session.downstream_abandoned_during_cache_fill();
    let admission_active = session.cache.is_cache_admission_active();
    drop(session);
    EarlyDepartureOutcome {
        result,
        abandonments_before_response: abandonments_before_response.unwrap(),
        abandonments: count(&observed.calls),
        abandonments_with_pending_tasks: count(&observed.calls_with_pending_tasks),
        downstream_informational_responses: count(
            &observed.downstream_informational_response_calls,
        ),
        downstream_abandoned,
        admission_active,
        released_writers: WRITER_DEMAND_CACHE.released_writers(path),
        cached_body: cached_response(path).await.map(|(_, body)| body),
    }
}

// Without abandonment handling the 1xx write to the departed client fails the request.
#[tokio::test]
async fn custom_downstream_abandonment_before_cache_admission_notifies_immediately() {
    let outcome =
        run_client_leaves_before_response_header("/custom-fill-abandoned-before-admission", true)
            .await;
    assert!(!outcome
        .result
        .expect("custom cache fill returned a downstream error"));
    assert_eq!(outcome.abandonments_before_response, 1);
    assert_eq!(outcome.abandonments, 1);
    assert_eq!(outcome.abandonments_with_pending_tasks, 0);
    assert_eq!(
        outcome.downstream_informational_responses, 0,
        "custom proxy ran the downstream response hook after abandonment",
    );
    assert!(outcome.downstream_abandoned);
    assert!(!outcome.admission_active);
    // No writer existed when the hook ran, and the hook does not reach writers created later.
    assert_eq!(outcome.released_writers, 0);
    assert_eq!(
        outcome.cached_body.as_deref(),
        Some(EARLY_DEPARTURE_BODY.as_ref())
    );

    let outcome = run_client_leaves_before_response_header(
        "/custom-fill-abandoned-before-admission-ungated",
        false,
    )
    .await;
    let error = outcome
        .result
        .expect_err("the failed informational response write was swallowed");
    assert_eq!(error.etype(), &WriteError);
    assert_eq!(outcome.abandonments_before_response, 0);
    assert_eq!(outcome.abandonments, 0);
    assert_eq!(outcome.downstream_informational_responses, 1);
    assert!(!outcome.downstream_abandoned);
    assert!(!outcome.admission_active);
    assert_eq!(outcome.released_writers, 0);
    assert_eq!(outcome.cached_body, None);
}

/// What the custom-message directions of a fill whose client left carried.
struct CustomMessageOutcome {
    result: Result<bool>,
    abandonments: usize,
    upstream_reader_cancelled: bool,
    downstream_reader_cancelled: bool,
    reply_to_upstream_message: Option<Bytes>,
    reply_to_downstream_message: Option<Bytes>,
    upstream_messages_filtered: usize,
    downstream_messages_filtered: usize,
    admission_active: bool,
    released_writers: usize,
}

/// Run a cache miss for a POST whose client leaves before sending its body. Once the proxy has
/// handled that, each custom-message direction carries one message, then the upstream sends a
/// cacheable response.
///
/// The forwarder of messages to the downstream stays open with the handling. Without it the loop
/// serves that forwarder until it ends, so the forwarder ends once its message was sent.
async fn run_custom_messages_after_client_leaves(
    path: &str,
    handles_abandonment: bool,
) -> CustomMessageOutcome {
    const BODY: Bytes = Bytes::from_static(b"cached body");

    let (mut session, client) = cache_miss_session_for_request(
        &format!("POST {path} HTTP/1.1\r\nHost: example.com\r\nContent-Length: 1\r\n\r\n"),
        handles_abandonment,
    )
    .await;

    let observed = CacheFillCallbackProxy::default();
    let proxy = HttpProxy::new(observed.clone(), Arc::new(ServerConf::default()));
    let (
        channels,
        CustomLoopDriver {
            tasks: task_tx,
            upstream_messages: upstream_custom_tx,
            downstream_messages: downstream_custom_tx,
            downstream_reader_cancelled: mut cancel_downstream_reader_rx,
            upstream_reader_cancelled: mut cancel_upstream_reader_rx,
            ..
        },
    ) = CustomLoop::new();
    let mut downstream_custom_tx = Some(downstream_custom_tx);

    let proxy_loop = channels.run(&proxy, &mut session, Box::new(NoopBodyWriter));
    let drive = async {
        drop(client);
        tokio::time::timeout(
            Duration::from_secs(5),
            observed.downstream_error_notify.notified(),
        )
        .await
        .expect("custom proxy did not observe the downstream close");
        let upstream_reader_cancelled = match cancel_upstream_reader_rx.try_recv() {
            Ok(()) => true,
            Err(oneshot::error::TryRecvError::Empty) => false,
            Err(oneshot::error::TryRecvError::Closed) => {
                panic!("upstream custom reader cancel guard was dropped")
            }
        };
        let downstream_reader_cancelled = !matches!(
            cancel_downstream_reader_rx.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        );

        let (reply_tx, reply_rx) = oneshot::channel();
        upstream_custom_tx
            .send((Bytes::from_static(b"control message"), reply_tx))
            .await
            .expect("messages to the upstream are no longer filtered");
        let reply_to_upstream_message = tokio::time::timeout(Duration::from_secs(1), reply_rx)
            .await
            .expect("message to the upstream was not filtered")
            .unwrap();
        // The forwarder of messages to the upstream finishes, as it does once its inputs end.
        drop(upstream_custom_tx);

        // A stopped direction refuses this message or never filters it.
        let (reply_tx, reply_rx) = oneshot::channel();
        let _ = downstream_custom_tx
            .as_ref()
            .unwrap()
            .send((Bytes::from_static(b"late message"), reply_tx))
            .await;
        let reply_to_downstream_message = reply_rx.await.ok().flatten();
        if !handles_abandonment {
            downstream_custom_tx = None;
        }

        task_tx.send(cacheable_response_header()).await.unwrap();
        task_tx
            .send(HttpTask::Body(Some(BODY), true))
            .await
            .unwrap();
        task_tx.send(HttpTask::Done).await.unwrap();
        drop(task_tx);
        (
            upstream_reader_cancelled,
            downstream_reader_cancelled,
            reply_to_upstream_message,
            reply_to_downstream_message,
        )
    };

    let (
        result,
        (
            upstream_reader_cancelled,
            downstream_reader_cancelled,
            reply_to_upstream_message,
            reply_to_downstream_message,
        ),
    ) = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(proxy_loop, drive)
    })
    .await
    .expect("custom proxy kept waiting for custom messages");
    drop(downstream_custom_tx);
    CustomMessageOutcome {
        result,
        abandonments: count(&observed.calls),
        upstream_reader_cancelled,
        downstream_reader_cancelled,
        reply_to_upstream_message,
        reply_to_downstream_message,
        upstream_messages_filtered: count(&observed.downstream_custom_message_calls),
        downstream_messages_filtered: count(&observed.upstream_custom_message_calls),
        admission_active: session.cache.is_cache_admission_active(),
        released_writers: WRITER_DEMAND_CACHE.released_writers(path),
    }
}

// Without abandonment handling messages to the downstream keep flowing after the client left.
#[tokio::test]
async fn custom_downstream_abandonment_stops_only_custom_messages_to_the_downstream() {
    let outcome =
        run_custom_messages_after_client_leaves("/custom-fill-stops-custom-messages", true).await;
    assert!(!outcome
        .result
        .expect("custom cache fill returned a downstream error"));
    assert_eq!(outcome.abandonments, 1);
    assert!(
        outcome.upstream_reader_cancelled,
        "abandonment did not stop the custom messages to the downstream"
    );
    // The upstream is still in use for the fill, so messages to it keep flowing.
    assert!(
        !outcome.downstream_reader_cancelled,
        "abandonment stopped the custom messages to the upstream"
    );
    assert_eq!(
        outcome.reply_to_upstream_message,
        Some(Bytes::from_static(b"control message"))
    );
    assert_eq!(outcome.reply_to_downstream_message, None);
    assert_eq!(outcome.upstream_messages_filtered, 1);
    assert_eq!(outcome.downstream_messages_filtered, 0);
    assert!(!outcome.admission_active);
    assert_eq!(outcome.released_writers, 0);

    let outcome = run_custom_messages_after_client_leaves(
        "/custom-fill-stops-custom-messages-ungated",
        false,
    )
    .await;
    let error = outcome
        .result
        .expect_err("the downstream failure was swallowed");
    assert_eq!(error.etype(), &WriteError);
    assert_eq!(outcome.abandonments, 0);
    assert!(!outcome.upstream_reader_cancelled);
    assert!(!outcome.downstream_reader_cancelled);
    assert_eq!(
        outcome.reply_to_upstream_message,
        Some(Bytes::from_static(b"control message"))
    );
    assert_eq!(
        outcome.reply_to_downstream_message,
        Some(Bytes::from_static(b"late message"))
    );
    assert_eq!(outcome.upstream_messages_filtered, 1);
    assert_eq!(outcome.downstream_messages_filtered, 1);
    assert!(!outcome.admission_active);
    assert_eq!(outcome.released_writers, 0);
}

/// What an HTTP/1 client does once its complete HEAD response has arrived, while the fill for
/// that response is still running.
#[derive(Clone, Copy)]
enum AfterCompleteResponse {
    /// It closes the connection.
    Close,
    /// It sends its next request, then closes the connection.
    EarlyRequestThenClose,
}

/// Run a HEAD miss whose fill outlives its complete response. The client does `action`; once the
/// loop has read it, the upstream finishes the fill and the custom-message channels close.
/// Returns the loop's result, how many abandonments the hook saw and how many cache writers had
/// their demand released.
async fn run_after_complete_head_response(
    path: &str,
    action: AfterCompleteResponse,
    handles_abandonment: bool,
) -> (
    std::result::Result<bool, pingora_error::ErrorType>,
    usize,
    usize,
) {
    let (mut session, mut client) = cache_miss_session_for_request(
        &format!("HEAD {path} HTTP/1.1\r\nHost: example.com\r\n\r\n"),
        handles_abandonment,
    )
    .await;
    let observed = CacheFillCallbackProxy::default();
    let proxy = HttpProxy::new(observed.clone(), Arc::new(ServerConf::default()));
    let (
        channels,
        CustomLoopDriver {
            tasks: task_tx,
            upstream_messages,
            downstream_messages,
            ..
        },
    ) = CustomLoop::new();

    let proxy_loop = channels.run(&proxy, &mut session, Box::new(NoopBodyWriter));
    let drive = async {
        task_tx.send(cacheable_response_header()).await.unwrap();
        let mut head = Vec::new();
        while !head.ends_with(b"\r\n\r\n") {
            head.push(client.read_u8().await.unwrap());
        }
        if matches!(action, AfterCompleteResponse::EarlyRequestThenClose) {
            client
                .write_all(b"GET /next HTTP/1.1\r\nHost: example.com\r\n\r\n")
                .await
                .unwrap();
        }
        drop(client);
        // Both runs read the client here and ignore the resulting error for the fill.
        observed.downstream_error_notify.notified().await;
        task_tx
            .send(HttpTask::Body(
                Some(Bytes::from_static(b"cached body")),
                true,
            ))
            .await
            .unwrap();
        task_tx.send(HttpTask::Done).await.unwrap();
        drop(task_tx);
        drop(upstream_messages);
        drop(downstream_messages);
    };
    let (result, ()) = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(proxy_loop, drive)
    })
    .await
    .expect("custom proxy loop did not finish");
    (
        result.map_err(|e| e.etype().clone()),
        count(&observed.calls),
        WRITER_DEMAND_CACHE.released_writers(path),
    )
}

async fn assert_after_complete_head_response_matches_opt_out(
    path: &str,
    action: AfterCompleteResponse,
) {
    let opted_out =
        run_after_complete_head_response(&format!("{path}-opted-out"), action, false).await;
    let opted_in =
        run_after_complete_head_response(&format!("{path}-opted-in"), action, true).await;
    assert_eq!(opted_in, opted_out);
    assert_eq!(opted_out.1, 0);
    assert_eq!(opted_out.2, 0);
}

// A client that closes once its HEAD response is complete ends the loop exactly as without the
// opt-in, and does not abandon the fill still running for that response.
#[tokio::test]
async fn client_close_after_complete_response_ends_custom_loop_like_opt_out() {
    assert_after_complete_head_response_matches_opt_out(
        "/custom-fill-client-close-after-head",
        AfterCompleteResponse::Close,
    )
    .await;
}

// The same for a client that first sends its next request.
#[tokio::test]
async fn early_request_then_close_after_complete_response_ends_custom_loop_like_opt_out() {
    assert_after_complete_head_response_matches_opt_out(
        "/custom-fill-early-request-after-head",
        AfterCompleteResponse::EarlyRequestThenClose,
    )
    .await;
}

// A connected client opted in to abandonment handling: the loop handles the trailer that follows
// the body end and ends only once the upstream task channel closes. The second run, without that
// handling, does the same.
#[tokio::test]
async fn connected_opted_in_miss_completes_like_opted_out_through_trailers() {
    for (path, handles_abandonment) in [
        ("/custom-fill-connected-trailers-gated", true),
        ("/custom-fill-connected-trailers-ungated", false),
    ] {
        let (result, trailer_filter_calls) =
            run_connected_miss(path, handles_abandonment, AfterBodyEnd::TrailerThenDone).await;
        result.expect("a connected miss with trailers failed");
        assert_eq!(trailer_filter_calls, 1);
    }
}
