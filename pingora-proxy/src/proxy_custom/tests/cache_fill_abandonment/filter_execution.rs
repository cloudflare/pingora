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

const ORIGINAL_BODY: Bytes = Bytes::from_static(b"original body");
const CACHED_BODY: Bytes = Bytes::from_static(b"upstream-filtered body");

/// How many times each response hook of a [`ResponseHookProxy`] ran.
#[derive(Default)]
struct ResponseHookCalls {
    upstream_header: AtomicUsize,
    upstream_body: AtomicUsize,
    upstream_trailer: AtomicUsize,
    downstream_header: AtomicUsize,
    downstream_body: AtomicUsize,
    downstream_trailer: AtomicUsize,
}

/// A [`CacheFillCallbackProxy`] that also counts its response hooks. Each response filter
/// transforms what it sees, as an application's would.
#[derive(Clone, Default)]
struct ResponseHookProxy {
    callback: CacheFillCallbackProxy,
    calls: Arc<ResponseHookCalls>,
}

#[async_trait]
impl ProxyHttp for ResponseHookProxy {
    type CTX = ();

    fn new_ctx(&self) -> Self::CTX {}

    async fn upstream_peer(
        &self,
        session: &mut Session,
        ctx: &mut Self::CTX,
    ) -> Result<Box<HttpPeer>> {
        self.callback.upstream_peer(session, ctx).await
    }

    fn response_cache_filter(
        &self,
        session: &Session,
        response: &ResponseHeader,
        ctx: &mut Self::CTX,
    ) -> Result<RespCacheable> {
        self.callback.response_cache_filter(session, response, ctx)
    }

    async fn upstream_response_filter(
        &self,
        _session: &mut Session,
        response: &mut ResponseHeader,
        _ctx: &mut Self::CTX,
    ) -> Result<()> {
        self.calls.upstream_header.fetch_add(1, Ordering::Relaxed);
        response.insert_header("x-upstream-filter", "true")
    }

    async fn upstream_response_body_filter(
        &self,
        _session: &mut Session,
        body: &mut Option<Bytes>,
        _end_of_stream: bool,
        _ctx: &mut Self::CTX,
    ) -> Result<Option<Duration>> {
        self.calls.upstream_body.fetch_add(1, Ordering::Relaxed);
        assert_eq!(body.as_deref(), Some(ORIGINAL_BODY.as_ref()));
        *body = Some(CACHED_BODY);
        Ok(None)
    }

    async fn upstream_response_trailer_filter(
        &self,
        _session: &mut Session,
        trailers: &mut http::HeaderMap,
        _ctx: &mut Self::CTX,
    ) -> Result<()> {
        self.calls.upstream_trailer.fetch_add(1, Ordering::Relaxed);
        trailers.insert("x-upstream-trailer", "true".parse().unwrap());
        Ok(())
    }

    async fn response_filter(
        &self,
        _session: &mut Session,
        response: &mut ResponseHeader,
        _ctx: &mut Self::CTX,
    ) -> Result<()> {
        self.calls.downstream_header.fetch_add(1, Ordering::Relaxed);
        response.insert_header("x-downstream-filter", "true")
    }

    async fn response_body_filter(
        &self,
        _session: &mut Session,
        body: &mut Option<Bytes>,
        _end_of_stream: bool,
        _ctx: &mut Self::CTX,
    ) -> Result<Option<Duration>> {
        self.calls.downstream_body.fetch_add(1, Ordering::Relaxed);
        assert_eq!(body.as_deref(), Some(CACHED_BODY.as_ref()));
        *body = Some(Bytes::from_static(b"downstream-filtered body"));
        Ok(None)
    }

    async fn response_trailer_filter(
        &self,
        _session: &mut Session,
        trailers: &mut http::HeaderMap,
        _ctx: &mut Self::CTX,
    ) -> Result<Option<Bytes>> {
        self.calls
            .downstream_trailer
            .fetch_add(1, Ordering::Relaxed);
        assert_eq!(trailers.get("x-upstream-trailer").unwrap(), "true");
        Ok(Some(Bytes::from_static(b"downstream trailer body")))
    }

    async fn downstream_custom_message_proxy_filter(
        &self,
        session: &mut Session,
        custom_message: Bytes,
        ctx: &mut Self::CTX,
        final_hop: bool,
    ) -> Result<Option<Bytes>> {
        self.callback
            .downstream_custom_message_proxy_filter(session, custom_message, ctx, final_hop)
            .await
    }

    async fn upstream_custom_message_proxy_filter(
        &self,
        session: &mut Session,
        custom_message: Bytes,
        ctx: &mut Self::CTX,
        final_hop: bool,
    ) -> Result<Option<Bytes>> {
        self.callback
            .upstream_custom_message_proxy_filter(session, custom_message, ctx, final_hop)
            .await
    }

    fn downstream_abandoned_during_cache_fill(&self, session: &Session, ctx: &mut Self::CTX) {
        self.callback
            .downstream_abandoned_during_cache_fill(session, ctx);
    }

    fn suppress_proxy_warn_log(
        &self,
        session: &Session,
        ctx: &Self::CTX,
        error: &Error,
        context: ProxyWarnLogContext,
    ) -> bool {
        self.callback
            .suppress_proxy_warn_log(session, ctx, error, context)
    }
}

struct RejectRequestBodyWrites;

#[async_trait]
impl BodyWrite for RejectRequestBodyWrites {
    async fn write_all_buf(&mut self, _data: &mut Bytes) -> Result<()> {
        Error::e_explain(
            InternalError,
            "retry attempt read from the abandoned downstream",
        )
    }

    async fn finish(&mut self) -> Result<()> {
        Error::e_explain(
            InternalError,
            "retry attempt finished the abandoned downstream body",
        )
    }

    async fn cleanup(&mut self) -> Result<()> {
        Ok(())
    }

    fn upgrade_body_writer(&mut self) {}
}

/// Process an informational response, then a cacheable response with a trailer, for a client
/// that left before its response header. The departure is recorded the way the proxy loop
/// records a failed downstream read. Stops at the first failing step, as the loop does.
async fn process_response_after_client_left(
    path: &str,
    handles_abandonment: bool,
) -> (Result<()>, ResponseHookProxy) {
    let (mut session, client) = cache_miss_session_for_request(
        &format!("GET {path} HTTP/1.1\r\nHost: example.com\r\n\r\n"),
        handles_abandonment,
    )
    .await;

    let observed = ResponseHookProxy::default();
    let proxy = HttpProxy::new(observed.clone(), Arc::new(ServerConf::default()));
    let (_task_tx, mut task_rx) = mpsc::channel(TASK_BUFFER_SIZE);
    let mut serve_from_cache = ServeFromCache::new();
    let mut range_body_filter = RangeBodyFilter::new();
    let mut response_state = ResponseStateMachine::new();

    drop(client);
    proxy.abandon_downstream_for_cache_fill(
        &serve_from_cache,
        &mut DownstreamStateMachine::new(true),
        &mut response_state,
        &mut session,
        &mut (),
    );

    let mut response = ResponseHeader::build(200, None).unwrap();
    response
        .insert_header(http::header::CACHE_CONTROL, "max-age=60")
        .unwrap();
    let tasks = [
        HttpTask::Header(Box::new(ResponseHeader::build(103, None).unwrap()), false),
        HttpTask::Header(Box::new(response), false),
        HttpTask::Body(Some(ORIGINAL_BODY), false),
        HttpTask::Trailer(Some(Box::new(http::HeaderMap::new()))),
        HttpTask::Done,
    ];
    let result: Result<()> = async {
        for task in tasks {
            // Until the producer ends, the fill stays admitted and the response work is not done.
            if matches!(task, HttpTask::Done) {
                assert!(session.cache.is_cache_admission_active());
                assert!(!response_state.upstream_producer_done());
                assert!(!session.response_work_done(&response_state));
            }
            let response_done = proxy
                .process_upstream_tasks_custom(
                    &mut session,
                    &mut (),
                    task,
                    &mut task_rx,
                    &mut serve_from_cache,
                    &mut range_body_filter,
                    &mut response_state,
                    &mut None,
                )
                .await?;
            assert_eq!(response_done, None);
        }
        assert!(response_state.upstream_producer_done());
        assert!(response_state.upstream_response_complete());
        assert!(!session.cache.is_cache_admission_active());
        assert!(!response_state.is_done());
        assert!(session.response_work_done(&response_state));
        Ok(())
    }
    .await;
    drop(session);
    (result, observed)
}

// Without abandonment handling the 1xx runs the downstream filter and fails to reach the client.
#[tokio::test]
async fn abandoned_custom_fill_runs_upstream_hooks_and_skips_downstream_filters() {
    const PATH: &str = "/custom-fill-filter-execution";

    let (result, observed) = process_response_after_client_left(PATH, true).await;
    // Writing to the departed client fails, so a fill that completes wrote nothing to it.
    result.expect("the abandoned cache fill failed");
    let calls = &observed.calls;
    assert_eq!(count(&calls.upstream_header), 2);
    assert_eq!(count(&calls.upstream_body), 1);
    assert_eq!(count(&calls.upstream_trailer), 1);
    assert_eq!(count(&calls.downstream_header), 0);
    assert_eq!(count(&calls.downstream_body), 0);
    assert_eq!(count(&calls.downstream_trailer), 0);
    assert_eq!(count(&observed.callback.downstream_custom_message_calls), 0);
    assert_eq!(count(&observed.callback.upstream_custom_message_calls), 0);
    assert_eq!(count(&observed.callback.calls), 1);
    // No writer existed when the hook ran, and the hook does not reach writers created later.
    assert_eq!(WRITER_DEMAND_CACHE.released_writers(PATH), 0);

    let (meta, body) = cached_response(PATH)
        .await
        .expect("completed custom fill should be readable");
    assert_eq!(meta.headers().get("x-upstream-filter").unwrap(), "true");
    assert!(meta.headers().get("x-downstream-filter").is_none());
    assert_eq!(body.as_slice(), CACHED_BODY.as_ref());

    const UNGATED_PATH: &str = "/custom-fill-filter-execution-ungated";
    let (result, observed) = process_response_after_client_left(UNGATED_PATH, false).await;
    let error =
        result.expect_err("the informational response write to the departed client was swallowed");
    assert_eq!(error.etype(), &WriteError);
    let calls = &observed.calls;
    assert_eq!(count(&calls.upstream_header), 1);
    assert_eq!(count(&calls.upstream_body), 0);
    assert_eq!(count(&calls.upstream_trailer), 0);
    assert_eq!(count(&calls.downstream_header), 1);
    assert_eq!(count(&calls.downstream_body), 0);
    assert_eq!(count(&calls.downstream_trailer), 0);
    assert_eq!(count(&observed.callback.calls), 0);
    assert_eq!(WRITER_DEMAND_CACHE.released_writers(UNGATED_PATH), 0);
    assert!(cached_response(UNGATED_PATH).await.is_none());
}

/// What a retry attempt of a request whose client left did.
struct RetryOutcome {
    result: Result<bool>,
    upstream_reader_cancelled: bool,
    reply_to_upstream_message: Option<Bytes>,
    reply_to_downstream_message: Option<Bytes>,
    pipe_state: u8,
    admission_active: bool,
    released_writers: usize,
    cached_body: Option<Vec<u8>>,
    observed: ResponseHookProxy,
}

/// Run a retry attempt of a POST whose client left, before sending its body, during an earlier
/// attempt that recorded the departure the way the proxy loop records a failed downstream read.
/// Each custom-message direction carries one message, then the upstream sends a cacheable
/// response.
///
/// The forwarder of messages to the downstream stays open with the handling. Without it the loop
/// serves that forwarder until it ends, so the forwarder ends once its message was sent.
async fn run_retry_after_client_left(path: &str, handles_abandonment: bool) -> RetryOutcome {
    let (mut session, client) = cache_miss_session_for_request(
        &format!("POST {path} HTTP/1.1\r\nHost: example.com\r\nContent-Length: 1\r\n\r\n"),
        handles_abandonment,
    )
    .await;

    let observed = ResponseHookProxy::default();
    let proxy = HttpProxy::new(observed.clone(), Arc::new(ServerConf::default()));
    drop(client);
    proxy.abandon_downstream_for_cache_fill(
        &ServeFromCache::new(),
        &mut DownstreamStateMachine::new(false),
        &mut ResponseStateMachine::new(),
        &mut session,
        &mut (),
    );

    let (
        channels,
        CustomLoopDriver {
            tasks: task_tx,
            upstream_messages: upstream_custom_tx,
            downstream_messages: downstream_custom_tx,
            downstream_reader_cancelled: _,
            upstream_reader_cancelled: cancel_upstream_reader_rx,
            pipe_state,
        },
    ) = CustomLoop::new();
    let (upstream_reply_tx, upstream_reply_rx) = oneshot::channel();
    let (downstream_reply_tx, downstream_reply_rx) = oneshot::channel();
    upstream_custom_tx
        .send((
            Bytes::from_static(b"message to the upstream"),
            upstream_reply_tx,
        ))
        .await
        .unwrap();
    // The forwarder of messages to the upstream finishes once its inputs end.
    drop(upstream_custom_tx);
    downstream_custom_tx
        .send((
            Bytes::from_static(b"message to the downstream"),
            downstream_reply_tx,
        ))
        .await
        .unwrap();
    let held_downstream_custom_tx = handles_abandonment.then_some(downstream_custom_tx);

    let proxy_loop = channels.run(&proxy, &mut session, Box::new(RejectRequestBodyWrites));
    let drive = async {
        if !handles_abandonment {
            // Without the handling this attempt reads the departed client again and ignores the
            // failure for the fill; the upstream responds after that.
            tokio::time::timeout(
                Duration::from_secs(5),
                observed.callback.downstream_error_notify.notified(),
            )
            .await
            .expect("the retry attempt did not ignore the departed client for the fill");
        }
        let mut response = ResponseHeader::build(200, None).unwrap();
        response
            .insert_header(http::header::CACHE_CONTROL, "max-age=60")
            .unwrap();
        task_tx
            .send(HttpTask::Header(Box::new(response), false))
            .await
            .unwrap();
        task_tx
            .send(HttpTask::Body(Some(ORIGINAL_BODY), true))
            .await
            .unwrap();
        task_tx.send(HttpTask::Done).await.unwrap();
        drop(task_tx);
    };

    let (result, ()) = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(proxy_loop, drive)
    })
    .await
    .expect("custom retry did not finish");
    drop(held_downstream_custom_tx);
    let admission_active = session.cache.is_cache_admission_active();
    drop(session);
    RetryOutcome {
        result,
        upstream_reader_cancelled: cancel_upstream_reader_rx.await.is_ok(),
        reply_to_upstream_message: upstream_reply_rx.await.ok().flatten(),
        reply_to_downstream_message: downstream_reply_rx.await.ok().flatten(),
        pipe_state: pipe_state.load(Ordering::Acquire),
        admission_active,
        released_writers: WRITER_DEMAND_CACHE.released_writers(path),
        cached_body: cached_response(path).await.map(|(_, body)| body),
        observed,
    }
}

// Without abandonment handling the retry re-reads the client and keeps both message directions.
#[tokio::test]
async fn abandoned_custom_retry_only_processes_upstream_cache_fill() {
    let outcome = run_retry_after_client_left("/custom-fill-abandoned-retry", true).await;
    let reusable = outcome
        .result
        .expect("custom retry failed while completing the cache fill");
    assert!(!reusable, "abandoned downstream was marked reusable");
    assert!(outcome.upstream_reader_cancelled);
    assert_eq!(
        outcome.reply_to_upstream_message,
        Some(Bytes::from_static(b"message to the upstream"))
    );
    assert_eq!(outcome.reply_to_downstream_message, None);
    let (calls, callback) = (&outcome.observed.calls, &outcome.observed.callback);
    assert_eq!(count(&calls.upstream_header), 1);
    assert_eq!(count(&calls.upstream_body), 1);
    assert_eq!(count(&calls.downstream_header), 0);
    assert_eq!(count(&calls.downstream_body), 0);
    assert_eq!(count(&callback.downstream_custom_message_calls), 1);
    assert_eq!(count(&callback.upstream_custom_message_calls), 0);
    assert_eq!(count(&callback.calls), 1);
    assert_eq!(outcome.pipe_state, PipeState::DownstreamComplete as u8);
    assert!(!outcome.admission_active);
    // No writer existed when the hook ran, and the hook does not reach writers created later.
    assert_eq!(outcome.released_writers, 0);
    assert_eq!(
        outcome.cached_body.as_deref(),
        Some(CACHED_BODY.as_ref()),
        "custom retry did not publish the cache fill"
    );

    let outcome = run_retry_after_client_left("/custom-fill-abandoned-retry-ungated", false).await;
    let error = outcome
        .result
        .expect_err("the retry attempt's downstream failure was swallowed");
    assert_eq!(error.etype(), &WriteError);
    assert!(!outcome.upstream_reader_cancelled);
    assert_eq!(
        outcome.reply_to_upstream_message,
        Some(Bytes::from_static(b"message to the upstream"))
    );
    assert_eq!(
        outcome.reply_to_downstream_message,
        Some(Bytes::from_static(b"message to the downstream"))
    );
    let (calls, callback) = (&outcome.observed.calls, &outcome.observed.callback);
    assert_eq!(count(&calls.upstream_header), 1);
    assert_eq!(count(&calls.upstream_body), 1);
    assert_eq!(count(&calls.downstream_header), 0);
    assert_eq!(count(&calls.downstream_body), 0);
    assert_eq!(count(&callback.downstream_custom_message_calls), 1);
    assert_eq!(count(&callback.upstream_custom_message_calls), 1);
    assert_eq!(count(&callback.calls), 0);
    assert_eq!(outcome.pipe_state, PipeState::Active as u8);
    assert!(!outcome.admission_active);
    assert_eq!(outcome.released_writers, 0);
    assert_eq!(outcome.cached_body.as_deref(), Some(CACHED_BODY.as_ref()));
}
