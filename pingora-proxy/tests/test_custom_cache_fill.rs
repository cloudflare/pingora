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

#![cfg(unix)]

use async_trait::async_trait;
use bytes::Bytes;
use pingora_cache::{
    trace::Span, CacheFillCancellation, CacheKey, CacheMeta, MemCache, RespCacheable, Storage,
};
use pingora_core::listeners::{L4BufferSettings, ListenerConfig};
use pingora_core::protocols::http::custom::server::Session as DownstreamSession;
use pingora_core::protocols::tls::{CustomALPN, ALPN};
use pingora_core::protocols::Digest;
use pingora_core::server::{configuration::ServerConf, Fds, ListenFds};
use pingora_core::services::Service;
use pingora_core::upstreams::peer::HttpPeer;
use pingora_error::{Error, ErrorType, Result};
use pingora_http::{RequestHeader, ResponseHeader};
use pingora_proxy::{
    http_proxy_service_with_name_custom_connector, ProxyHttp, ProxyWarnLogContext, Session,
};
use std::future::Future;
use std::net::{SocketAddr, TcpListener};
use std::os::fd::IntoRawFd;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, SystemTime};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::sync::{oneshot, watch, Notify};
use tokio::task::JoinHandle;

#[path = "custom_cache_fill/upstream.rs"]
mod upstream;
use upstream::{CustomUpstream, StreamingConnector, FIRST_BODY, LAST_BODY, LATE_MESSAGE_COUNT};

#[path = "custom_cache_fill/cancellation.rs"]
mod cancellation;

#[path = "custom_cache_fill/failed_attempt.rs"]
mod failed_attempt;

#[path = "custom_cache_fill/final_error_log.rs"]
mod final_error_log;

#[path = "custom_cache_fill/fill_cancellation.rs"]
mod fill_cancellation;

/// Bound for each wait in these tests, which only a hang can exceed. It costs nothing while the
/// suite passes, so it is generous.
const TEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Shared by every test in this binary that doesn't need its own storage. Each test requests its
/// own path, which is its cache key.
static CACHE: LazyLock<MemCache> = LazyLock::new(MemCache::new);

#[derive(Default)]
struct Observations {
    callback_calls: AtomicUsize,
    custom_messages: AtomicUsize,
    body_bytes: AtomicUsize,
    initial_admission_active: AtomicBool,
    admitted_at_abandonment: AtomicBool,
    completed: AtomicBool,
    expected_error: Option<ErrorType>,
    write_failure: Option<Arc<cancellation::WriteFailure>>,
    /// Cancellation the abandonment callback requests, when a scenario cancels the fill from there.
    cancel_in_hook: Option<Arc<CacheFillCancellation>>,
    /// Hook that fails before the proxy writes the request to the custom upstream, if any.
    pre_write_failure: Option<PreWriteFailure>,
    first_custom_message: Notify,
    abandoned: Notify,
    downstream_error_ignored: Notify,
    finished: Notify,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PreWriteFailure {
    ConnectedHook,
    RequestFilter,
}

struct CacheFillProxy {
    cache: &'static (dyn Storage + Sync),
    observed: Arc<Observations>,
    handles_abandonment: bool,
}

#[async_trait]
impl<DS: DownstreamSession> ProxyHttp<DS> for CacheFillProxy {
    type CTX = ();

    fn new_ctx(&self) -> Self::CTX {}

    fn should_handle_downstream_abandonment(&self, _session: &Session<DS>, _ctx: &()) -> bool {
        self.handles_abandonment
    }

    async fn connected_to_upstream(
        &self,
        _session: &mut Session<DS>,
        _reused: bool,
        _peer: &HttpPeer,
        _fd: std::os::fd::RawFd,
        _digest: Option<&Digest>,
        _ctx: &mut (),
    ) -> Result<()> {
        if self.observed.pre_write_failure == Some(PreWriteFailure::ConnectedHook) {
            return Error::e_explain(ErrorType::InternalError, "injected connection hook error");
        }
        Ok(())
    }

    async fn upstream_request_filter(
        &self,
        _session: &mut Session<DS>,
        _upstream_request: &mut RequestHeader,
        _ctx: &mut (),
    ) -> Result<()> {
        if self.observed.pre_write_failure == Some(PreWriteFailure::RequestFilter) {
            return Error::e_explain(ErrorType::InternalError, "injected request filter error");
        }
        Ok(())
    }

    async fn upstream_peer(
        &self,
        _session: &mut Session<DS>,
        _ctx: &mut (),
    ) -> Result<Box<HttpPeer>> {
        let mut peer = HttpPeer::new(("127.0.0.1", 1), false, String::new());
        peer.options.alpn = ALPN::Custom(CustomALPN::new(b"test/1".to_vec()));
        Ok(Box::new(peer))
    }

    fn request_cache_filter(&self, session: &mut Session<DS>, _ctx: &mut ()) -> Result<()> {
        session.cache.enable(self.cache, None, None, None, None);
        Ok(())
    }

    fn cache_key_callback(&self, session: &Session<DS>, _ctx: &mut ()) -> Result<CacheKey> {
        Ok(CacheKey::new(session.req_header().uri.path(), ""))
    }

    fn response_cache_filter(
        &self,
        _session: &Session<DS>,
        response: &ResponseHeader,
        _ctx: &mut (),
    ) -> Result<RespCacheable> {
        let now = SystemTime::now();
        Ok(RespCacheable::Cacheable(CacheMeta::new(
            now + Duration::from_secs(60),
            now,
            0,
            0,
            response.clone(),
        )))
    }

    async fn upstream_response_body_filter(
        &self,
        session: &mut Session<DS>,
        body: &mut Option<Bytes>,
        _end_of_stream: bool,
        _ctx: &mut (),
    ) -> Result<Option<Duration>> {
        if let Some(body) = body {
            self.observed
                .body_bytes
                .fetch_add(body.len(), Ordering::Relaxed);
            if body.as_ref() == FIRST_BODY {
                self.observed
                    .initial_admission_active
                    .store(session.cache.is_cache_admission_active(), Ordering::Relaxed);
            }
        }
        Ok(None)
    }

    async fn upstream_custom_message_proxy_filter(
        &self,
        _session: &mut Session<DS>,
        message: Bytes,
        _ctx: &mut (),
        _final_hop: bool,
    ) -> Result<Option<Bytes>> {
        if self
            .observed
            .custom_messages
            .fetch_add(1, Ordering::Relaxed)
            == 0
        {
            self.observed.first_custom_message.notify_one();
        }
        Ok(Some(message))
    }

    fn downstream_abandoned_during_cache_fill(&self, session: &Session<DS>, _ctx: &mut ()) {
        self.observed
            .admitted_at_abandonment
            .store(session.cache.is_cache_admission_active(), Ordering::Relaxed);
        self.observed.callback_calls.fetch_add(1, Ordering::Relaxed);
        if let Some(cancellation) = &self.observed.cancel_in_hook {
            cancellation.cancel();
        }
        self.observed.abandoned.notify_one();
    }

    fn suppress_proxy_warn_log(
        &self,
        _session: &Session<DS>,
        _ctx: &(),
        _error: &Error,
        context: ProxyWarnLogContext,
    ) -> bool {
        // Logged once the proxy has handled the client leaving, with or without abandonment
        // handling, and after any abandonment callback.
        if context == ProxyWarnLogContext::DownstreamCache {
            if let Some(failure) = &self.observed.write_failure {
                failure.release();
            }
            self.observed.downstream_error_ignored.notify_one();
        }
        false
    }

    async fn logging(&self, session: &mut Session<DS>, error: Option<&Error>, _ctx: &mut ()) {
        assert_eq!(
            error.map(Error::etype),
            self.observed.expected_error.as_ref(),
            "unexpected cache fill result: {error:?}"
        );
        if error.is_none() {
            assert!(!session.cache.is_cache_admission_active());
        }
        self.observed.completed.store(true, Ordering::Release);
        self.observed.finished.notify_one();
    }
}

struct RunningProxy {
    address: SocketAddr,
    shutdown: watch::Sender<bool>,
    task: JoinHandle<()>,
}

impl RunningProxy {
    fn start<A>(app: A, connector: StreamingConnector) -> Self
    where
        A: ProxyHttp + Send + Sync + 'static,
        A::CTX: Send + Sync + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let mut service = http_proxy_service_with_name_custom_connector(
            &Arc::new(ServerConf::default()),
            app,
            "custom cache fill test",
            connector,
        );
        // The client must receive the prefix before the producer sends the remaining body.
        service.add_listener(
            ListenerConfig::tcp(address.to_string())
                .l4_buffer(L4BufferSettings::default().write(0)),
        );
        let fds: ListenFds = Arc::new(Fds::new().into());
        fds.lock().add(address.to_string(), listener.into_raw_fd());
        let (shutdown, watch) = watch::channel(false);
        let task = tokio::spawn(async move { service.start_service(Some(fds), watch, 1).await });
        Self {
            address,
            shutdown,
            task,
        }
    }

    async fn stop(mut self) {
        self.shutdown.send(true).unwrap();
        within("proxy service did not shut down", &mut self.task)
            .await
            .unwrap();
    }
}

impl Drop for RunningProxy {
    fn drop(&mut self) {
        let _ = self.shutdown.send(true);
    }
}

async fn within<F: Future>(message: &str, future: F) -> F::Output {
    tokio::time::timeout(TEST_TIMEOUT, future)
        .await
        .expect(message)
}

/// Connects to the proxy and sends a GET for `path`.
async fn send_request(address: SocketAddr, path: &str) -> BufReader<TcpStream> {
    let mut client = within("HTTP client did not connect", TcpStream::connect(address))
        .await
        .unwrap();
    let request = format!("GET {path} HTTP/1.1\r\nHost: example.com\r\nConnection: close\r\n\r\n");
    within(
        "HTTP request was not written",
        client.write_all(request.as_bytes()),
    )
    .await
    .unwrap();
    BufReader::new(client)
}

async fn read_initial_response(client: &mut BufReader<TcpStream>) {
    let mut line = String::new();
    client.read_line(&mut line).await.unwrap();
    assert!(
        line.starts_with("HTTP/1.1 200"),
        "unexpected response: {line}"
    );
    loop {
        line.clear();
        assert!(client.read_line(&mut line).await.unwrap() > 0);
        if line == "\r\n" {
            break;
        }
    }
    let mut body = vec![0; FIRST_BODY.len()];
    client.read_exact(&mut body).await.unwrap();
    assert_eq!(body, FIRST_BODY);
}

async fn read_cached_body(cache: &'static MemCache, path: &str) -> Option<Vec<u8>> {
    let key = CacheKey::new(path, "");
    let span = Span::inactive();
    let (_meta, mut hit) = within(
        "cache lookup did not finish",
        cache.lookup(&key, &span.handle()),
    )
    .await
    .unwrap()?;
    let mut body = Vec::new();
    while let Some(chunk) = within("cache body did not finish", hit.read_body())
        .await
        .unwrap()
    {
        body.extend_from_slice(&chunk);
    }
    within(
        "cache hit did not finish",
        hit.finish(cache, &key, &span.handle()),
    )
    .await
    .unwrap();
    Some(body)
}

enum Downstream {
    /// Leaves while the upstream holds the response header.
    LeavesBeforeHeader,
    /// Leaves after receiving the header and the first part of the body.
    LeavesMidBody,
    Connected,
}

async fn custom_cache_fill(path: &str, downstream: Downstream, handles_abandonment: bool) {
    let before_header = matches!(downstream, Downstream::LeavesBeforeHeader);
    let leaves = !matches!(downstream, Downstream::Connected);
    // Only abandonment handling stops forwarding to the downstream that left.
    let abandoned = leaves && handles_abandonment;
    let observed = Arc::new(Observations {
        // Without abandonment handling, the loop ends in the error the downstream left.
        expected_error: (leaves && !handles_abandonment).then_some(ErrorType::WriteError),
        ..Default::default()
    });
    let upstream = if before_header {
        CustomUpstream::with_held_response_header()
    } else {
        CustomUpstream::new()
    };
    let server = RunningProxy::start(
        CacheFillProxy {
            cache: &*CACHE,
            observed: observed.clone(),
            handles_abandonment,
        },
        upstream.connector,
    );
    let (release, released) = oneshot::channel();
    let producer = tokio::spawn(upstream.producer.run(released));
    let mut client = send_request(server.address, path).await;
    if !before_header {
        within(
            "initial response was not delivered",
            read_initial_response(&mut client),
        )
        .await;
    }
    // The transport loop filters custom messages while it polls the downstream.
    within(
        "initial custom message was not filtered",
        observed.first_custom_message.notified(),
    )
    .await;
    assert_eq!(
        observed.body_bytes.load(Ordering::Relaxed),
        if before_header { 0 } else { FIRST_BODY.len() }
    );
    assert_eq!(observed.custom_messages.load(Ordering::Relaxed), 1);

    if leaves {
        drop(client);
        within(
            "the proxy did not ignore the downstream error for the cache fill",
            observed.downstream_error_ignored.notified(),
        )
        .await;
        assert_eq!(
            observed.callback_calls.load(Ordering::Relaxed),
            usize::from(abandoned)
        );
        release.send(()).unwrap();
    } else {
        release.send(()).unwrap();
        let mut body = Vec::new();
        within(
            "connected client did not receive EOF",
            client.read_to_end(&mut body),
        )
        .await
        .unwrap();
        assert_eq!(body, LAST_BODY);
    }

    let reader_closed = within(
        "custom producer is blocked on a live receiver before the final HTTP body",
        producer,
    )
    .await
    .unwrap();
    assert_eq!(reader_closed, abandoned);
    within(
        "custom cache fill did not complete",
        observed.finished.notified(),
    )
    .await;
    assert_eq!(
        observed.callback_calls.load(Ordering::Relaxed),
        usize::from(abandoned)
    );
    if abandoned {
        assert_eq!(
            observed.admitted_at_abandonment.load(Ordering::Relaxed),
            !before_header
        );
    }
    assert!(observed.initial_admission_active.load(Ordering::Relaxed));
    assert_eq!(
        observed.custom_messages.load(Ordering::Relaxed),
        if abandoned { 1 } else { 1 + LATE_MESSAGE_COUNT },
    );
    assert_eq!(
        observed.body_bytes.load(Ordering::Relaxed),
        FIRST_BODY.len() + LAST_BODY.len()
    );

    let cached_body = read_cached_body(&CACHE, path)
        .await
        .expect("completed custom response was not cached");
    assert_eq!(cached_body, [FIRST_BODY, LAST_BODY].concat());
    server.stop().await;
}

#[tokio::test]
async fn custom_reader_is_released_when_downstream_abandons_cache_fill() {
    custom_cache_fill("/leaves-mid-body-gated", Downstream::LeavesMidBody, true).await;
    // Opted out, nothing is stopped and the request fails with the downstream's error.
    custom_cache_fill("/leaves-mid-body-ungated", Downstream::LeavesMidBody, false).await;
}

#[tokio::test]
async fn custom_reader_is_released_when_downstream_leaves_before_response_header() {
    custom_cache_fill(
        "/leaves-before-header-gated",
        Downstream::LeavesBeforeHeader,
        true,
    )
    .await;
    // Opted out, nothing is stopped and the request fails with the downstream's error.
    custom_cache_fill(
        "/leaves-before-header-ungated",
        Downstream::LeavesBeforeHeader,
        false,
    )
    .await;
}

#[tokio::test]
async fn connected_downstream_completes_custom_cache_fill() {
    custom_cache_fill("/connected-gated", Downstream::Connected, true).await;
    // Opted out, a connected client gets the same response, custom messages and cache entry.
    custom_cache_fill("/connected-ungated", Downstream::Connected, false).await;
}
