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

//! An H2 upstream attempt replaying a complete request body after the H2 client reset its
//! stream. The body exceeds the upstream window, so the replay waits on flow control while the
//! closed client is observable.

use super::*;
use crate::test_support::{AbandonmentSignalProxy, WRITER_DEMAND_CACHE};
use pingora_cache::NoCacheReason;
use pingora_core::modules::http::HttpModules;
use pingora_core::protocols::http::v2::server::{self as h2_server, H2Accept};
use pingora_core::protocols::http::ServerSession;
use pingora_core::protocols::Digest;
use pingora_core::server::configuration::ServerConf;
use std::sync::atomic::{AtomicBool, AtomicU8};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

/// One byte over the default H2 window, and within the retry buffer.
const RETRY_BODY_LEN: usize = 64 * 1024;
const BUFFERED_BODY_LEN: usize = 1 << 20;

#[derive(Clone, Copy)]
enum Body {
    /// Read from the client into the ordinary retry buffer.
    RetryBuffer,
    /// Left for the transport loop to read, ending with a chunk the upstream window cannot take.
    Streamed,
    EarlyBuffered,
}

impl Body {
    fn len(self) -> usize {
        match self {
            Body::RetryBuffer | Body::Streamed => RETRY_BODY_LEN,
            Body::EarlyBuffered => BUFFERED_BODY_LEN,
        }
    }

    /// The `content-length` the client declares.
    fn declared_len(self) -> usize {
        match self {
            Body::RetryBuffer | Body::Streamed => RETRY_BODY_LEN,
            Body::EarlyBuffered => 0,
        }
    }
}

#[derive(Clone, Copy)]
enum CacheState {
    Disabled,
    /// A partial-write miss whose client is not yet known gone.
    PartialWriteMiss,
    /// The client was recorded gone during a partial-write miss that is still filling.
    AbandonedPartialWriteMiss,
    /// The client was recorded gone during a fill whose cache was then disabled.
    AbandonedWithoutFill,
}

/// An H2 downstream session for a PUT carrying `body`, and the client's request stream.
async fn downstream(
    body: Body,
    cache: CacheState,
) -> (Session, h2::SendStream<Bytes>, Vec<JoinHandle<()>>) {
    let (client_io, server_io) = tokio::io::duplex(1 << 20);
    let (mut requests, connection) = h2::client::handshake(client_io).await.unwrap();
    let client_task = tokio::spawn(async move {
        let _ = connection.await;
    });
    let mut server_connection = h2_server::handshake(Box::new(server_io), None)
        .await
        .unwrap();
    let request = http::Request::builder()
        .method("PUT")
        .uri("https://test/")
        .header("content-length", body.declared_len())
        .body(())
        .unwrap();
    let (_response, mut downstream_body) = requests
        .send_request(request, body.declared_len() == 0)
        .unwrap();
    let bytes = |len| Bytes::from(vec![b'x'; len]);
    match body {
        Body::RetryBuffer => downstream_body.send_data(bytes(body.len()), true).unwrap(),
        Body::Streamed => {
            downstream_body
                .send_data(bytes(body.len() - 1), false)
                .unwrap();
            downstream_body.send_data(bytes(1), true).unwrap();
        }
        Body::EarlyBuffered => {}
    }
    let Some(H2Accept::Session(http)) =
        h2_server::HttpSession::from_h2_conn(&mut server_connection, Arc::new(Digest::default()))
            .await
            .unwrap()
    else {
        panic!("expected an H2 session");
    };
    let server_task = tokio::spawn(async move {
        while let Ok(Some(_)) = h2_server::HttpSession::from_h2_conn(
            &mut server_connection,
            Arc::new(Digest::default()),
        )
        .await
        {}
    });

    let mut session = Session::new(
        Box::new(ServerSession::new_http2(http)),
        &HttpModules::new(),
        #[cfg(feature = "upstream_modules")]
        &HttpModules::new(),
        Arc::new(AtomicBool::new(false)),
    );
    session.set_handles_downstream_abandonment(true);
    match body {
        Body::RetryBuffer => {
            session.as_mut().enable_retry_buffering();
            let mut read = 0;
            while read < body.len() {
                read += session.read_request_body().await.unwrap().unwrap().len();
            }
            assert!(session.as_mut().is_body_done());
            assert_eq!(
                session.as_ref().get_retry_buffer().map(|b| b.len()),
                Some(body.len())
            );
        }
        Body::Streamed => {}
        Body::EarlyBuffered => session
            .set_buffered_body(Some(Bytes::from(vec![b'x'; body.len()])))
            .unwrap(),
    }
    if !matches!(cache, CacheState::Disabled) {
        session
            .cache
            .enable(&*WRITER_DEMAND_CACHE, None, None, None, None);
        session
            .cache
            .set_cache_key(pingora_cache::CacheKey::new("/h2-replay", ""));
        session.cache.cache_miss();
        assert_eq!(session.cache.support_streaming_partial_write(), Some(true));
    }
    if matches!(
        cache,
        CacheState::AbandonedPartialWriteMiss | CacheState::AbandonedWithoutFill
    ) {
        assert!(session.mark_downstream_abandoned_during_cache_fill());
    }
    if matches!(cache, CacheState::AbandonedWithoutFill) {
        session.cache.disable(NoCacheReason::Custom("test"));
        assert!(session.downstream_abandoned_during_cache_fill());
    }
    (session, downstream_body, vec![client_task, server_task])
}

/// Like [`downstream`], once the client has reset its stream.
async fn reset_downstream(body: Body, cache: CacheState) -> (Session, Vec<JoinHandle<()>>) {
    let (mut session, mut downstream_body, tasks) = downstream(body, cache).await;
    downstream_body.send_reset(h2::Reason::CANCEL);
    tokio::time::timeout(
        Duration::from_secs(1),
        session.downstream_session.watch_h2_stream_close().unwrap(),
    )
    .await
    .expect("downstream reset was not observed")
    .unwrap();
    (session, tasks)
}

/// An H2 upstream that reports how many request body bytes arrived by its end. Unless it
/// `reads`, it never grants more flow-control capacity, so only client closure can end a replay.
async fn upstream(
    reads: bool,
) -> (
    h2::SendStream<Bytes>,
    oneshot::Receiver<usize>,
    Vec<JoinHandle<()>>,
) {
    let (client_io, server_io) = tokio::io::duplex(65536);
    let (received_tx, received_rx) = oneshot::channel();
    let server_task = tokio::spawn(async move {
        let mut connection = h2::server::handshake(server_io).await.unwrap();
        let Some(Ok((request, _respond))) = connection.accept().await else {
            return;
        };
        let connection_task =
            tokio::spawn(async move { while connection.accept().await.is_some() {} });
        let mut body = request.into_body();
        if !reads {
            std::future::pending::<()>().await;
        }
        let mut received = 0;
        while let Some(Ok(chunk)) = body.data().await {
            received += chunk.len();
            let _ = body.flow_control().release_capacity(chunk.len());
        }
        let _ = received_tx.send(received);
        connection_task.abort();
    });
    let (mut upstream, connection) = h2::client::handshake(client_io).await.unwrap();
    let connection_task = tokio::spawn(async move {
        let _ = connection.await;
    });
    upstream = upstream.ready().await.unwrap();
    let request = http::Request::builder()
        .method("PUT")
        .uri("https://origin/")
        .body(())
        .unwrap();
    let (_response, body_writer) = upstream.send_request(request, false).unwrap();
    (body_writer, received_rx, vec![server_task, connection_task])
}

/// Disables caching from the request body filter, as an application may.
#[derive(Default)]
struct DisableCacheInBodyFilter;

#[async_trait]
impl ProxyHttp for DisableCacheInBodyFilter {
    type CTX = ();

    fn new_ctx(&self) -> Self::CTX {}

    async fn upstream_peer(
        &self,
        _session: &mut Session,
        _ctx: &mut Self::CTX,
    ) -> Result<Box<HttpPeer>> {
        unreachable!("test calls the transport loop directly")
    }

    async fn request_body_filter(
        &self,
        session: &mut Session,
        _body: &mut Option<Bytes>,
        _end_of_stream: bool,
        _ctx: &mut Self::CTX,
    ) -> Result<()> {
        session.cache.disable(NoCacheReason::Custom("test"));
        Ok(())
    }
}

/// Runs the attempt until it fails or the upstream has read the whole body.
async fn replay<SV>(
    app: SV,
    body: Body,
    cache: CacheState,
    upstream_reads: bool,
) -> std::result::Result<usize, Box<Error>>
where
    SV: ProxyHttp<CTX = ()> + Send + Sync,
{
    let (mut session, mut tasks) = reset_downstream(body, cache).await;
    let (mut body_writer, received, upstream_tasks) = upstream(upstream_reads).await;
    tasks.extend(upstream_tasks);
    let (_response_tx, response_rx) = mpsc::channel(TASK_BUFFER_SIZE);
    let proxy = HttpProxy::new(app, Arc::new(ServerConf::default()));
    let (mut custom_writer, mut custom_reader) = (None, None);
    let pipe_state = Arc::new(AtomicU8::new(PipeState::Active as u8));
    let mut ctx = ();

    let outcome = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::select! {
            result = proxy.bidirection_down_to_up(
                &mut session,
                &mut body_writer,
                response_rx,
                &mut ctx,
                None,
                false,
                &mut custom_writer,
                &mut custom_reader,
                pipe_state,
            ) => match result {
                Err(e) => Err(e),
                Ok(_) => panic!("the attempt ended without an upstream response"),
            },
            received = received => Ok(received.expect("upstream did not finish reading")),
        }
    })
    .await
    .expect("attempt neither failed nor delivered the body");
    for task in tasks {
        task.abort();
    }
    outcome
}

async fn assert_replayed(body: Body, cache: CacheState) {
    let received = replay(AbandonmentSignalProxy::default(), body, cache, true)
        .await
        .unwrap_or_else(|e| panic!("closed client cut short a replay the fill can use: {e}"));
    assert_eq!(received, body.len());
}

async fn assert_fails_fast<SV>(app: SV, body: Body, cache: CacheState)
where
    SV: ProxyHttp<CTX = ()> + Send + Sync,
{
    let error = replay(app, body, cache, false)
        .await
        .expect_err("a replay no fill can use should stop once the client resets");
    assert_eq!(error.esource, ErrorSource::Downstream);
    // The loop's own downstream read would fail the same way, but only once the replay finished.
    assert_eq!(error.etype(), &H2Error);
    assert!(
        error.to_string().contains("while writing body to upstream"),
        "not stopped by the replay's closure watch: {error}"
    );
}

/// Ordinary retry buffers keep upstream's closure watch with early buffering enabled.
#[tokio::test]
async fn retry_buffer_replay_fails_when_client_resets_during_partial_write_fill() {
    assert_fails_fast(
        AbandonmentSignalProxy::default(),
        Body::RetryBuffer,
        CacheState::PartialWriteMiss,
    )
    .await;
}

#[tokio::test]
async fn uncached_retry_buffer_replay_fails_when_client_resets() {
    assert_fails_fast(
        AbandonmentSignalProxy::default(),
        Body::RetryBuffer,
        CacheState::Disabled,
    )
    .await;
}

#[tokio::test]
async fn buffered_replay_completes_for_partial_write_fill() {
    assert_replayed(Body::EarlyBuffered, CacheState::PartialWriteMiss).await;
}

#[tokio::test]
async fn abandoned_buffered_replay_completes_for_partial_write_fill() {
    assert_replayed(Body::EarlyBuffered, CacheState::AbandonedPartialWriteMiss).await;
}

#[tokio::test]
async fn abandoned_buffered_replay_without_fill_fails_when_client_resets() {
    assert_fails_fast(
        AbandonmentSignalProxy::default(),
        Body::EarlyBuffered,
        CacheState::AbandonedWithoutFill,
    )
    .await;
}

/// Signals when the final request body chunk reaches the body filter, and when the proxy reports
/// downstream abandonment.
#[derive(Default)]
struct ObserveStreamedBody {
    final_chunk: Arc<tokio::sync::Notify>,
    abandoned: Arc<tokio::sync::Notify>,
}

#[async_trait]
impl ProxyHttp for ObserveStreamedBody {
    type CTX = ();

    fn new_ctx(&self) -> Self::CTX {}

    async fn upstream_peer(
        &self,
        _session: &mut Session,
        _ctx: &mut Self::CTX,
    ) -> Result<Box<HttpPeer>> {
        unreachable!("test calls the transport loop directly")
    }

    async fn request_body_filter(
        &self,
        _session: &mut Session,
        _body: &mut Option<Bytes>,
        end_of_stream: bool,
        _ctx: &mut Self::CTX,
    ) -> Result<()> {
        if end_of_stream {
            self.final_chunk.notify_one();
        }
        Ok(())
    }

    fn downstream_abandoned_during_cache_fill(&self, _session: &Session, _ctx: &mut ()) {
        self.abandoned.notify_one();
    }
}

/// The loop's final streamed chunk keeps its closure watch even though it completes the body, so
/// a client that resets while that write waits on the upstream is still noticed.
#[tokio::test]
async fn streamed_final_chunk_still_watches_client_closure() {
    let (mut session, mut downstream_body, mut tasks) =
        downstream(Body::Streamed, CacheState::PartialWriteMiss).await;
    let (mut body_writer, _received, upstream_tasks) = upstream(false).await;
    tasks.extend(upstream_tasks);
    let app = ObserveStreamedBody::default();
    let (final_chunk, abandoned) = (app.final_chunk.clone(), app.abandoned.clone());
    let (_response_tx, response_rx) = mpsc::channel(TASK_BUFFER_SIZE);
    let proxy = HttpProxy::new(app, Arc::new(ServerConf::default()));
    let (mut custom_writer, mut custom_reader) = (None, None);
    let pipe_state = Arc::new(AtomicU8::new(PipeState::Active as u8));
    let mut ctx = ();

    tokio::time::timeout(Duration::from_secs(5), async {
        tokio::select! {
            result = proxy.bidirection_down_to_up(
                &mut session,
                &mut body_writer,
                response_rx,
                &mut ctx,
                None,
                false,
                &mut custom_writer,
                &mut custom_reader,
                pipe_state,
            ) => panic!("the attempt ended before the client was noticed: {result:?}"),
            _ = async {
                final_chunk.notified().await;
                downstream_body.send_reset(h2::Reason::CANCEL);
                abandoned.notified().await;
            } => {}
        }
    })
    .await
    .expect("a reset during the final streamed write went unnoticed");
    for task in tasks {
        task.abort();
    }
}

#[tokio::test]
async fn buffered_replay_fails_when_body_filter_disables_caching() {
    assert_fails_fast(
        DisableCacheInBodyFilter,
        Body::EarlyBuffered,
        CacheState::PartialWriteMiss,
    )
    .await;
}

#[tokio::test]
async fn uncached_buffered_replay_fails_when_client_resets() {
    assert_fails_fast(
        AbandonmentSignalProxy::default(),
        Body::EarlyBuffered,
        CacheState::Disabled,
    )
    .await;
}
