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

#[tokio::test]
async fn failed_upload_keeps_active_fill_until_terminal_response_after_framed_delivery() {
    const PATH: &str = "/failed-upload-framed-delivery-active-fill";
    let (mut session, mut client) = cache_miss_session_for_request(
        &format!("POST {PATH} HTTP/1.1\r\nHost: example.com\r\nContent-Length: 4\r\n\r\n"),
        true,
    )
    .await;
    let proxy = HttpProxy::new(
        CacheFillCallbackProxy::default(),
        Arc::new(ServerConf::default()),
    );
    let (channels, CustomLoopDriver { tasks, .. }) = CustomLoop::new();
    let failed = Arc::new(tokio::sync::Notify::new());
    let writer = Box::new(FailingRequestBodyWriter {
        failure: RequestBodyFailure::Write,
        write_attempted: failed.clone(),
    });
    let drive = async {
        let mut header = ResponseHeader::build(200, None).unwrap();
        header.insert_header("content-length", "4").unwrap();
        tasks
            .send(HttpTask::Header(Box::new(header), false))
            .await
            .unwrap();
        read_response_head(&mut client).await;
        client.write_all(b"body").await.unwrap();
        failed.notified().await;
        tasks
            .send(HttpTask::Body(Some(Bytes::from_static(b"body")), false))
            .await
            .unwrap();
        let mut body = [0; 4];
        client.read_exact(&mut body).await.unwrap();
        assert_eq!(&body, b"body");
        // The client has its entire framed body, but the cache producer has not ended.
        tasks
            .send(HttpTask::Failed(
                Error::explain(ReadError, "late fill failure").into_up(),
            ))
            .await
            .unwrap();
    };
    let (result, ()) = tokio::time::timeout(Duration::from_secs(2), async {
        tokio::join!(channels.run(&proxy, &mut session, writer), drive)
    })
    .await
    .expect("the active fill must consume the terminal error");
    assert_eq!(
        result
            .expect_err("failed fill must fail the attempt")
            .etype(),
        &WriteError
    );
    assert!(session.downstream_session.is_response_complete());
    drop(session);
    assert!(
        cached_response(PATH).await.is_none(),
        "failed fill must not be published"
    );
}

#[derive(Clone, Copy)]
enum RequestBodyFailure {
    Write,
    Finish,
}

impl RequestBodyFailure {
    fn context(self) -> &'static str {
        match self {
            Self::Write => "injected upstream request body write failure",
            Self::Finish => "injected upstream request body finish failure",
        }
    }
}

struct FailingRequestBodyWriter {
    failure: RequestBodyFailure,
    write_attempted: Arc<tokio::sync::Notify>,
}

impl FailingRequestBodyWriter {
    fn fail(&self) -> Result<()> {
        self.write_attempted.notify_one();
        Error::e_explain(WriteError, self.failure.context())
    }
}

#[async_trait]
impl BodyWrite for FailingRequestBodyWriter {
    async fn write_all_buf(&mut self, body: &mut Bytes) -> Result<()> {
        if matches!(self.failure, RequestBodyFailure::Write) {
            return self.fail();
        }
        body.advance(body.len());
        Ok(())
    }

    async fn finish(&mut self) -> Result<()> {
        self.fail()
    }

    async fn cleanup(&mut self) -> Result<()> {
        Ok(())
    }

    fn upgrade_body_writer(&mut self) {}
}

// The second run is the same upload failure without abandonment handling, and ends the same way.
#[tokio::test]
async fn custom_loop_preserves_response_after_request_body_write_failure_during_cache_miss() {
    for (path, handles_abandonment) in [
        ("/custom-fill-request-body-write-error", true),
        ("/custom-fill-request-body-write-error-ungated", false),
    ] {
        assert_request_body_failure_keeps_custom_output(
            path,
            RequestBodyFailure::Write,
            handles_abandonment,
        )
        .await;
    }
}

// The second run is the same upload failure without abandonment handling, and ends the same way.
#[tokio::test]
async fn custom_loop_preserves_response_after_request_body_finish_failure_during_cache_miss() {
    for (path, handles_abandonment) in [
        ("/custom-fill-request-body-finish-error", true),
        ("/custom-fill-request-body-finish-error-ungated", false),
    ] {
        assert_request_body_failure_keeps_custom_output(
            path,
            RequestBodyFailure::Finish,
            handles_abandonment,
        )
        .await;
    }
}

async fn assert_request_body_failure_keeps_custom_output(
    path: &str,
    failure: RequestBodyFailure,
    handles_abandonment: bool,
) {
    let (mut session, mut client) = cache_miss_session_for_request(
        &format!("POST {path} HTTP/1.1\r\nHost: example.com\r\nContent-Length: 4\r\n\r\n"),
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
            downstream_reader_cancelled,
            ..
        },
    ) = CustomLoop::new();
    let write_attempted = Arc::new(tokio::sync::Notify::new());
    let request_body = Box::new(FailingRequestBodyWriter {
        failure,
        write_attempted: Arc::clone(&write_attempted),
    });
    let proxy_loop = channels.run(&proxy, &mut session, request_body);
    let drive = async {
        task_tx.send(cacheable_response_header()).await.unwrap();
        read_response_head(&mut client).await;
        client.write_all(b"body").await.unwrap();
        write_attempted.notified().await;
        downstream_reader_cancelled.await.unwrap();
        // The cancelled forwarder that read the downstream's messages drops its filter sender.
        drop(upstream_custom_tx);

        let (downstream_reply, downstream_reply_rx) = oneshot::channel();
        downstream_custom_tx
            .send((
                Bytes::from_static(b"late upstream message"),
                downstream_reply,
            ))
            .await
            .unwrap();
        task_tx
            .send(HttpTask::Body(Some(Bytes::from_static(b"body")), true))
            .await
            .unwrap();
        task_tx.send(HttpTask::Done).await.unwrap();
        drop(task_tx);
        let downstream_reply = downstream_reply_rx.await;
        // Open custom output keeps the loop running.
        drop(downstream_custom_tx);
        downstream_reply
    };
    let (result, downstream_reply) = tokio::time::timeout(Duration::from_secs(2), async {
        tokio::join!(proxy_loop, drive)
    })
    .await
    .expect("custom proxy did not finish once its custom output ended");
    assert_eq!(
        downstream_reply.ok().flatten().as_deref(),
        Some(b"late upstream message".as_slice()),
        "custom output must keep flowing after the upstream request body failed"
    );
    // An upload failure is not downstream abandonment. A complete response
    // survives it, independently of the cache-abandonment opt-in.
    assert!(
        matches!(result, Ok(true)),
        "complete response was lost: {result:?}"
    );
    assert_eq!(count(&observed.calls), 0);
    assert_eq!(WRITER_DEMAND_CACHE.released_writers(path), 0);
    assert_eq!(count(&observed.upstream_custom_message_calls), 1);
    assert_eq!(count(&observed.downstream_custom_message_calls), 0);
    assert!(!session.downstream_abandoned_during_cache_fill());
    assert!(!session.cache.is_cache_admission_active());
    let (_, body) = cached_response(path)
        .await
        .expect("the fill of a failed upload was not published");
    assert_eq!(body, b"body");
}

/// What a cache fill whose client stopped reading, and whose upstream then failed, left behind.
struct UpstreamFailureOutcome {
    result: Result<bool>,
    abandonments: usize,
    downstream_abandoned: bool,
    admission_active: bool,
    released_writers: usize,
    cached_body: Option<Vec<u8>>,
}

/// Run a streaming cache miss whose client stops reading: writing the body to it times out, then
/// the client closes and the upstream fails.
async fn run_upstream_failure_after_client_stops_reading(
    path: &str,
    handles_abandonment: bool,
) -> UpstreamFailureOutcome {
    const PARTIAL_BODY: Bytes = Bytes::from_static(&[b'x'; 8 * 1024]);

    let (mut session, mut client) = cache_miss_session_for_request(
        &format!("GET {path} HTTP/1.1\r\nHost: example.com\r\n\r\n"),
        handles_abandonment,
    )
    .await;
    session.set_write_timeout(Some(Duration::from_millis(100)));

    let observed = CacheFillCallbackProxy::default();
    let proxy = HttpProxy::new(observed.clone(), Arc::new(ServerConf::default()));
    let (channels, CustomLoopDriver { tasks: task_tx, .. }) = CustomLoop::new();
    let proxy_loop = channels.run(&proxy, &mut session, Box::new(NoopBodyWriter));
    let drive = async {
        task_tx.send(cacheable_response_header()).await.unwrap();
        read_response_head(&mut client).await;
        task_tx
            .send(HttpTask::Body(Some(PARTIAL_BODY), false))
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
            .send(HttpTask::Failed(Error::explain(
                ReadError,
                "injected upstream read failure",
            )))
            .await
            .unwrap();
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
    UpstreamFailureOutcome {
        result,
        abandonments: count(&observed.calls),
        downstream_abandoned,
        admission_active,
        released_writers: WRITER_DEMAND_CACHE.released_writers(path),
        cached_body: cached_response(path).await.map(|(_, body)| body),
    }
}

// Without abandonment handling, the client that stopped reading is not abandoned, and the
// upstream failure fails the request the same way.
#[tokio::test]
async fn custom_loop_propagates_upstream_failure_after_cache_fill_abandonment() {
    let outcome =
        run_upstream_failure_after_client_stops_reading("/custom-fill-upstream-error", true).await;
    let error = outcome
        .result
        .expect_err("upstream failure was swallowed after abandonment");
    assert_eq!(error.etype(), &ReadError);
    assert_eq!(outcome.abandonments, 1);
    assert!(outcome.downstream_abandoned);
    assert!(outcome.admission_active);
    assert_eq!(outcome.released_writers, 1);
    assert_eq!(
        outcome.cached_body, None,
        "failed custom fill published a partial cache entry"
    );

    let outcome = run_upstream_failure_after_client_stops_reading(
        "/custom-fill-upstream-error-ungated",
        false,
    )
    .await;
    let error = outcome
        .result
        .expect_err("upstream failure was swallowed after the client stopped reading");
    assert_eq!(error.etype(), &ReadError);
    assert_eq!(outcome.abandonments, 0);
    assert!(!outcome.downstream_abandoned);
    assert!(outcome.admission_active);
    assert_eq!(outcome.released_writers, 0);
    assert_eq!(
        outcome.cached_body, None,
        "failed custom fill published a partial cache entry"
    );
}

// A connected client opted in to abandonment handling: an upstream failure after the body end
// still fails the request. The second run, without that handling, fails the same way.
#[tokio::test]
async fn connected_opted_in_miss_fails_like_opted_out_on_failure_after_body_end() {
    for (path, handles_abandonment) in [
        ("/custom-fill-connected-failure-gated", true),
        ("/custom-fill-connected-failure-ungated", false),
    ] {
        let (result, _) = run_connected_miss(path, handles_abandonment, AfterBodyEnd::Failed).await;
        let error = result.expect_err("an upstream failure after the body end was swallowed");
        assert_eq!(error.etype(), &ReadError);
    }
}

/// What a streaming miss whose connected client's request body read timed out left behind.
struct BodyReadTimeoutOutcome {
    result: Result<bool>,
    abandonments: usize,
    downstream_abandoned: bool,
    released_writers: usize,
    cached_body: Option<Vec<u8>>,
}

/// Run a streaming cache miss whose client sends 2 of its 10 body bytes and then nothing, past a
/// 100 ms read timeout, while staying connected. Once the loop has ignored that error to keep the
/// fill going, the upstream sends a complete response.
async fn run_request_body_read_timeout(
    path: &str,
    handles_abandonment: bool,
) -> BodyReadTimeoutOutcome {
    const BODY: &[u8] = b"cached body";

    let (mut session, _client) = cache_miss_session_for_request(
        &format!("GET {path} HTTP/1.1\r\nHost: example.com\r\nContent-Length: 10\r\n\r\nup"),
        handles_abandonment,
    )
    .await;
    session
        .downstream_session
        .set_read_timeout(Some(Duration::from_millis(100)));

    let observed = CacheFillCallbackProxy::default();
    let proxy = HttpProxy::new(observed.clone(), Arc::new(ServerConf::default()));
    let (channels, CustomLoopDriver { tasks: task_tx, .. }) = CustomLoop::new();
    let proxy_loop = channels.run(&proxy, &mut session, Box::new(NoopBodyWriter));
    let drive = async {
        tokio::time::timeout(
            Duration::from_secs(5),
            observed.downstream_error_notify.notified(),
        )
        .await
        .expect("custom proxy did not ignore the timed-out request body read for the cache fill");
        let mut header = ResponseHeader::build(200, None).unwrap();
        header
            .insert_header(http::header::CACHE_CONTROL, "max-age=60")
            .unwrap();
        header
            .insert_header(http::header::CONTENT_LENGTH, BODY.len())
            .unwrap();
        task_tx
            .send(HttpTask::Header(Box::new(header), false))
            .await
            .unwrap();
        task_tx
            .send(HttpTask::Body(Some(Bytes::from_static(BODY)), true))
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
    let downstream_abandoned = session.downstream_abandoned_during_cache_fill();
    drop(session);
    BodyReadTimeoutOutcome {
        result,
        abandonments: count(&observed.calls),
        downstream_abandoned,
        released_writers: WRITER_DEMAND_CACHE.released_writers(path),
        cached_body: cached_response(path).await.map(|(_, body)| body),
    }
}

/// A connected client whose request body read timed out has not left. The loop records no
/// abandonment and keeps the writer's demand, the fill completes, and the request fails once the
/// loop ends with the downstream errored, as without abandonment handling, so the proxy can
/// still answer that client with an error response. The second run, without that handling,
/// ends the same way.
#[tokio::test]
async fn custom_loop_request_body_read_timeout_is_not_abandonment() {
    for (path, handles_abandonment) in [
        ("/custom-fill-request-body-read-timeout-gated", true),
        ("/custom-fill-request-body-read-timeout-ungated", false),
    ] {
        let outcome = run_request_body_read_timeout(path, handles_abandonment).await;
        assert_eq!(
            outcome.abandonments, 0,
            "a timed-out request body read from a connected client was reported as an abandonment"
        );
        assert!(!outcome.downstream_abandoned);
        assert_eq!(outcome.released_writers, 0);
        assert_eq!(
            outcome.cached_body.as_deref(),
            Some(b"cached body".as_slice())
        );
        let error = outcome
            .result
            .expect_err("the request succeeded without delivering its response");
        assert_eq!(error.etype(), &WriteError);
    }
}

/// A session for a request whose client left during an earlier attempt's cache fill, with the
/// request body that attempt buffered for retries. The departure is recorded the way the proxy
/// loop records a failed downstream read.
async fn departed_client_with_retry_buffer<SV>(proxy: &HttpProxy<SV>, path: &str) -> Session
where
    SV: ProxyHttp<CTX = ()>,
{
    let (mut session, mut client) = cache_miss_session_for_request(
        &format!("POST {path} HTTP/1.1\r\nHost: example.com\r\nContent-Length: 4\r\n\r\n"),
        true,
    )
    .await;
    session.as_mut().enable_retry_buffering();
    client.write_all(b"body").await.unwrap();
    let buffered = session
        .downstream_session
        .read_request_body()
        .await
        .unwrap();
    assert_eq!(buffered.as_deref(), Some(b"body".as_slice()));
    drop(client);
    proxy.abandon_downstream_for_cache_fill(
        &ServeFromCache::new(),
        &mut DownstreamStateMachine::new(true),
        &mut ResponseStateMachine::new(),
        &mut session,
        &mut (),
    );
    assert!(session.downstream_abandoned_during_cache_fill());
    session
}

async fn h1_retry_upload_failure_after_client_left(path: &str) -> Option<Box<Error>> {
    let proxy = HttpProxy::new(
        CacheFillCallbackProxy::default(),
        Arc::new(ServerConf::default()),
    );
    let mut session = departed_client_with_retry_buffer(&proxy, path).await;
    let mut request = session.req_header().clone();
    pingora_cache::filters::upstream::request_filter(
        &mut request,
        session.cache.maybe_cache_meta(),
    );
    crate::proxy_common::finalize_h1_upstream_request_framing(&mut request, true).unwrap();
    let request_wire =
        pingora_core::protocols::http::v1::client::http_req_header_to_wire(&request).unwrap();
    let upstream_io = tokio_test::io::Builder::new()
        .write(&request_wire)
        .write_error(std::io::Error::from(std::io::ErrorKind::BrokenPipe))
        .read(b"HTTP/1.1 200 OK\r\nCache-Control: max-age=60\r\nContent-Length: 4\r\n\r\nbody")
        .build();
    let mut upstream = HttpSessionV1::new(Box::new(upstream_io));
    let peer = HttpPeer::new(("127.0.0.1", 1), false, String::new());
    let (_, _, error) = proxy
        .proxy_1to1(&mut session, &mut upstream, &peer, &mut ())
        .await;
    error
}

async fn custom_retry_upload_failure_after_client_left(path: &str) -> Result<bool> {
    let proxy = HttpProxy::new(
        CacheFillCallbackProxy::default(),
        Arc::new(ServerConf::default()),
    );
    let mut session = departed_client_with_retry_buffer(&proxy, path).await;
    let (channels, CustomLoopDriver { tasks: task_tx, .. }) = CustomLoop::new();
    let write_attempted = Arc::new(tokio::sync::Notify::new());
    let request_body = Box::new(FailingRequestBodyWriter {
        failure: RequestBodyFailure::Write,
        write_attempted: Arc::clone(&write_attempted),
    });
    let proxy_loop = channels.run(&proxy, &mut session, request_body);
    let drive = async {
        write_attempted.notified().await;
        task_tx.send(cacheable_response_header()).await.unwrap();
        task_tx
            .send(HttpTask::Body(Some(Bytes::from_static(b"body")), true))
            .await
            .unwrap();
        task_tx.send(HttpTask::Done).await.unwrap();
        drop(task_tx);
    };
    let (result, ()) = tokio::join!(proxy_loop, drive);
    result
}

/// A retry attempt resends the buffered request body. When that upload fails after the client
/// left, the complete response still fills the cache and the attempt succeeds, on both paths.
#[tokio::test]
async fn retry_upload_failure_after_client_left_matches_h1() {
    const H1_PATH: &str = "/h1-fill-retry-upload-failure";
    const CUSTOM_PATH: &str = "/custom-fill-retry-upload-failure";
    let h1_error = tokio::time::timeout(
        Duration::from_secs(3),
        h1_retry_upload_failure_after_client_left(H1_PATH),
    )
    .await
    .expect("the HTTP/1 retry attempt did not finish");
    assert!(h1_error.is_none(), "HTTP/1: {h1_error:?}");
    assert_eq!(
        cached_response(H1_PATH)
            .await
            .map(|(_, body)| body)
            .as_deref(),
        Some(b"body".as_slice())
    );

    let custom_result = tokio::time::timeout(
        Duration::from_secs(3),
        custom_retry_upload_failure_after_client_left(CUSTOM_PATH),
    )
    .await
    .expect("the custom retry attempt did not finish");
    assert!(
        matches!(custom_result, Ok(false)),
        "custom: {custom_result:?}"
    );
    assert_eq!(
        cached_response(CUSTOM_PATH)
            .await
            .map(|(_, body)| body)
            .as_deref(),
        Some(b"body".as_slice())
    );
}
