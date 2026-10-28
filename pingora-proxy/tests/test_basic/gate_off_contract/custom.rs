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

//! A caching proxy with a custom downstream session in front of an HTTP/1 origin, and one with
//! an HTTP/1 downstream in front of a custom upstream, whose client leaves during a miss. Each
//! test runs its own proxy service; they share one in-memory cache, keyed by path.

use super::*;
use async_trait::async_trait;
use pingora_cache::{CacheKey, CacheMeta, CachePhase, MemCache, RespCacheable};
use pingora_core::apps::{HttpServerApp, HttpServerOptions};
use pingora_core::protocols::http::custom::server::Session as DownstreamSession;
use pingora_core::protocols::http::ServerSession;
use pingora_core::protocols::tls::{CustomALPN, ALPN};
use pingora_core::server::{configuration::ServerConf, Fds};
use pingora_core::services::Service as _;
use pingora_core::upstreams::peer::HttpPeer;
use pingora_error::{Error, Result};
use pingora_http::ResponseHeader;
use pingora_proxy::{
    http_proxy_service_with_name_custom_connector, ProcessCustomSession, ProxyHttp,
    ProxyServiceBuilder, ProxyWarnLogContext, Session,
};
use std::net::SocketAddr;
use std::os::fd::IntoRawFd;
use std::sync::{LazyLock, Mutex};
use std::time::SystemTime;
use tokio::sync::watch;

#[path = "../../utils/custom_downstream.rs"]
mod downstream;
#[path = "custom/upstream.rs"]
mod upstream;
use downstream::{CustomDownstream, CustomOutput};
use upstream::{CustomUpstream, OneSessionConnector};

/// The cache of every [`ContractProxy`]; each test uses its own paths.
static CACHE: LazyLock<MemCache> = LazyLock::new(MemCache::new);

/// How a request through a [`ContractProxy`] ended.
#[derive(Clone, Debug, PartialEq)]
struct Completion {
    method: String,
    /// The final error's type, and the context of the error it was caused by, if any.
    error: Option<(ErrorType, Option<String>)>,
    /// Why the cache was disabled when the request ended, if it was.
    disabled_cache: Option<NoCacheReason>,
}

/// What a [`ContractProxy`] observed, across all its requests.
#[derive(Default)]
struct Observed {
    upstream_attempts: AtomicUsize,
    ignored_downstream_errors: Mutex<Vec<ErrorType>>,
    completions: Mutex<Vec<Completion>>,
    /// The custom output of each custom downstream connection, in accept order.
    outputs: Mutex<Vec<Arc<Mutex<CustomOutput>>>>,
    /// The custom-message sender to the downstream, when the proxy keeps it for the test.
    custom_to_downstream: Mutex<Option<mpsc::Sender<Bytes>>>,
    /// The custom-message sender to the upstream, when the proxy keeps it for the test and the
    /// upstream takes custom messages.
    custom_to_upstream: Mutex<Option<mpsc::Sender<Bytes>>>,
    /// The custom messages from the upstream that the proxy passed through its filter.
    filtered_from_upstream: Mutex<Vec<Bytes>>,
}

impl Observed {
    fn upstream_attempts(&self) -> usize {
        self.upstream_attempts.load(Ordering::Relaxed)
    }

    fn completions(&self) -> Vec<Completion> {
        self.completions.lock().unwrap().clone()
    }

    /// Wait until the proxy has ignored `n` downstream errors, then return their types.
    async fn wait_for_ignored_errors(&self, n: usize) -> Vec<ErrorType> {
        wait_until("the proxy did not ignore a downstream error", || {
            self.ignored_downstream_errors.lock().unwrap().len() >= n
        })
        .await;
        self.ignored_downstream_errors.lock().unwrap().clone()
    }

    /// Wait until `n` requests have completed, then return how they ended.
    async fn wait_for_completions(&self, n: usize) -> Vec<Completion> {
        wait_until("the request did not complete", || {
            self.completions.lock().unwrap().len() >= n
        })
        .await;
        self.completions()
    }

    /// The custom output of the `n`th custom downstream connection.
    fn output(&self, n: usize) -> CustomOutput {
        self.outputs.lock().unwrap()[n].lock().unwrap().clone()
    }

    /// Wait until the proxy has filtered `n` custom messages from the upstream, then return them.
    async fn wait_for_filtered_from_upstream(&self, n: usize) -> Vec<Bytes> {
        wait_until(
            "the proxy did not filter a custom message from the upstream",
            || self.filtered_from_upstream.lock().unwrap().len() >= n,
        )
        .await;
        self.filtered_from_upstream.lock().unwrap().clone()
    }

    /// The custom-message senders the proxy kept for the test, to the downstream and to the
    /// upstream.
    fn take_custom_senders(&self) -> (mpsc::Sender<Bytes>, Option<mpsc::Sender<Bytes>>) {
        let to_downstream = self
            .custom_to_downstream
            .lock()
            .unwrap()
            .take()
            .expect("the proxy did not start custom forwarding");
        (
            to_downstream,
            self.custom_to_upstream.lock().unwrap().take(),
        )
    }
}

enum Upstream {
    /// An HTTP/1 origin on this local port.
    Http(u16),
    /// The service's custom connector.
    Custom,
}

/// A caching proxy for any downstream session type, with default settings apart from the
/// cache, and a record of what it observed.
struct ContractProxy {
    upstream: Upstream,
    /// Keep the custom-message senders for the test, instead of dropping them.
    keep_custom_senders: bool,
    observed: Arc<Observed>,
}

#[async_trait]
impl<DS: DownstreamSession> ProxyHttp<DS> for ContractProxy {
    type CTX = ();

    fn new_ctx(&self) -> Self::CTX {}

    async fn upstream_peer(
        &self,
        _session: &mut Session<DS>,
        _ctx: &mut (),
    ) -> Result<Box<HttpPeer>> {
        self.observed
            .upstream_attempts
            .fetch_add(1, Ordering::Relaxed);
        let peer = match self.upstream {
            Upstream::Http(port) => HttpPeer::new(("127.0.0.1", port), false, String::new()),
            Upstream::Custom => {
                let mut peer = HttpPeer::new(("127.0.0.1", 1), false, String::new());
                peer.options.alpn = ALPN::Custom(CustomALPN::new(b"test/1".to_vec()));
                peer
            }
        };
        Ok(Box::new(peer))
    }

    fn request_cache_filter(&self, session: &mut Session<DS>, _ctx: &mut ()) -> Result<()> {
        session.cache.enable(&*CACHE, None, None, None, None);
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

    async fn response_filter(
        &self,
        session: &mut Session<DS>,
        response: &mut ResponseHeader,
        _ctx: &mut (),
    ) -> Result<()> {
        let status = match session.cache.phase() {
            CachePhase::Hit => "hit",
            CachePhase::Miss => "miss",
            _ => "other",
        };
        response.insert_header("x-cache-status", status)?;
        Ok(())
    }

    async fn custom_forwarding(
        &self,
        _session: &mut Session<DS>,
        _ctx: &mut (),
        custom_message_to_upstream: Option<mpsc::Sender<Bytes>>,
        custom_message_to_downstream: mpsc::Sender<Bytes>,
    ) -> Result<()> {
        if self.keep_custom_senders {
            *self.observed.custom_to_downstream.lock().unwrap() =
                Some(custom_message_to_downstream);
            *self.observed.custom_to_upstream.lock().unwrap() = custom_message_to_upstream;
        }
        Ok(())
    }

    async fn upstream_custom_message_proxy_filter(
        &self,
        _session: &mut Session<DS>,
        custom_message: Bytes,
        _ctx: &mut (),
        _final_hop: bool,
    ) -> Result<Option<Bytes>> {
        self.observed
            .filtered_from_upstream
            .lock()
            .unwrap()
            .push(custom_message.clone());
        Ok(Some(custom_message))
    }

    fn suppress_proxy_warn_log(
        &self,
        _session: &Session<DS>,
        _ctx: &(),
        error: &Error,
        context: ProxyWarnLogContext,
    ) -> bool {
        if context == ProxyWarnLogContext::DownstreamCache {
            self.observed
                .ignored_downstream_errors
                .lock()
                .unwrap()
                .push(error.etype().clone());
        }
        false
    }

    async fn logging(&self, session: &mut Session<DS>, error: Option<&Error>, _ctx: &mut ()) {
        let completion = Completion {
            method: session.req_header().method.to_string(),
            error: error.map(|e| (e.etype().clone(), innermost_context(e))),
            disabled_cache: match session.cache.phase() {
                CachePhase::Disabled(reason) => Some(reason),
                _ => None,
            },
        };
        self.observed.completions.lock().unwrap().push(completion);
    }
}

/// The context of the innermost error in `error`'s chain of causes.
fn innermost_context(error: &Error) -> Option<String> {
    let cause = error.cause.as_ref().and_then(|cause| {
        cause
            .downcast_ref::<Error>()
            .or_else(|| cause.downcast_ref::<Box<Error>>().map(|cause| &**cause))
    });
    match cause {
        Some(cause) => innermost_context(cause),
        None => error.context.as_ref().map(|context| context.to_string()),
    }
}

/// A proxy service running on a local port for one test.
struct RunningProxy {
    address: SocketAddr,
    observed: Arc<Observed>,
    shutdown: watch::Sender<bool>,
}

impl Drop for RunningProxy {
    fn drop(&mut self) {
        let _ = self.shutdown.send(true);
    }
}

/// A listener on a free local port, for a service to take over.
fn bind_listener() -> (SocketAddr, Fds) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let mut fds = Fds::new();
    fds.add(address.to_string(), listener.into_raw_fd());
    (address, fds)
}

impl RunningProxy {
    /// A proxy whose downstream sessions are [`CustomDownstream`]s, in front of the HTTP/1 origin
    /// on `origin_port`, that keeps the custom-message senders for the test.
    fn custom_downstream(origin_port: u16) -> Self {
        let observed = Arc::new(Observed::default());
        let proxy = ContractProxy {
            upstream: Upstream::Http(origin_port),
            keep_custom_senders: true,
            observed: observed.clone(),
        };
        let on_custom: ProcessCustomSession<ContractProxy, (), CustomDownstream> = {
            let observed = observed.clone();
            Arc::new(move |proxy, stream, shutdown| {
                let output = Arc::new(Mutex::new(CustomOutput::default()));
                observed.outputs.lock().unwrap().push(output.clone());
                let shutdown = shutdown.clone();
                Box::pin(async move {
                    let mut http = ServerSession::new_http1(stream);
                    if !http.read_request().await.unwrap_or(false) {
                        return None;
                    }
                    let downstream = CustomDownstream::new(http, output);
                    proxy
                        .process_new_http(ServerSession::new_custom(downstream), &shutdown)
                        .await;
                    None
                })
            })
        };
        let mut options = HttpServerOptions::default();
        options.force_custom = true;
        let mut service = ProxyServiceBuilder::new(&Arc::new(ServerConf::default()), proxy)
            .custom((), on_custom)
            .server_options(options)
            .build();
        let (address, fds) = bind_listener();
        service.add_tcp(&address.to_string());
        let (shutdown, shutdown_rx) = watch::channel(false);
        tokio::spawn(async move {
            service
                .start_service(Some(Arc::new(fds.into())), shutdown_rx, 1)
                .await;
        });
        Self {
            address,
            observed,
            shutdown,
        }
    }

    /// A proxy with HTTP/1 downstream sessions in front of `connector`.
    fn custom_upstream(connector: OneSessionConnector, keep_custom_senders: bool) -> Self {
        let observed = Arc::new(Observed::default());
        let proxy = ContractProxy {
            upstream: Upstream::Custom,
            keep_custom_senders,
            observed: observed.clone(),
        };
        let mut service = http_proxy_service_with_name_custom_connector(
            &Arc::new(ServerConf::default()),
            proxy,
            "gate-off contract custom upstream",
            connector,
        );
        let (address, fds) = bind_listener();
        service.add_tcp(&address.to_string());
        let (shutdown, shutdown_rx) = watch::channel(false);
        tokio::spawn(async move {
            service
                .start_service(Some(Arc::new(fds.into())), shutdown_rx, 1)
                .await;
        });
        Self {
            address,
            observed,
            shutdown,
        }
    }

    async fn connect(&self) -> TcpStream {
        tokio::time::timeout(TEST_TIMEOUT, TcpStream::connect(self.address))
            .await
            .expect("the proxy did not accept the connection")
            .unwrap()
    }

    /// Send a request on a new connection.
    async fn send(&self, method: &str, path: &str) -> TcpStream {
        let mut client = self.connect().await;
        client
            .write_all(format!("{method} {path} HTTP/1.1\r\nHost: {HOST}\r\n\r\n").as_bytes())
            .await
            .unwrap();
        client
    }

    /// A `GET` for `path` on a new connection, with the decoded body.
    async fn fetch(&self, path: &str) -> (String, Vec<u8>) {
        let mut client = self.connect().await;
        client
            .write_all(
                format!("GET {path} HTTP/1.1\r\nHost: {HOST}\r\nConnection: close\r\n\r\n")
                    .as_bytes(),
            )
            .await
            .unwrap();
        let head = read_head(&mut client).await;
        let body = if is_chunked(&head) {
            read_chunked_body(&mut client).await
        } else {
            read_rest(&mut client).await
        };
        (head, body)
    }

    /// A follow-up `GET` is a hit with `body`: the origin no longer accepts anything, so a miss
    /// would fail.
    async fn assert_cached(&self, path: &str, body: &[u8]) {
        let (head, cached) = self.fetch(path).await;
        assert!(head.starts_with("HTTP/1.1 200"), "{head}");
        assert_eq!(header_value(&head, "x-cache-status"), Some("hit"), "{head}");
        assert_eq!(cached, body);
    }
}

/// A request that succeeded, with the cache enabled throughout.
fn completed(method: &str) -> Completion {
    Completion {
        method: method.to_owned(),
        error: None,
        disabled_cache: None,
    }
}

/// A custom-upstream request that failed once its transport loop ended with the downstream
/// errored; the proxy then disables the cache.
fn failed_with_errored_downstream(method: &str) -> Completion {
    Completion {
        method: method.to_owned(),
        error: Some((
            ErrorType::WriteError,
            Some("downstream_state is_errored".to_owned()),
        )),
        disabled_cache: Some(NoCacheReason::InternalError),
    }
}

/// When the application keeps the custom-message sender, a custom downstream client that closes
/// mid-body does not stop the custom output: a message sent afterwards is still written to the
/// session, the output is finished once the sender is dropped, and the fill completes.
#[tokio::test]
async fn custom_downstream_messages_after_client_close_are_still_written() {
    let path = "/gate_off/custom_downstream_messages_after_client_close";
    let mut origin = ScriptedOrigin::start().await;
    let proxy = RunningProxy::custom_downstream(origin.port);
    let mut client = proxy.send("GET", path).await;
    origin.wait_for_request().await;
    origin.send(CHUNKED_HEADER).await;
    origin.send(HELLO_CHUNK).await;
    let head = read_head(&mut client).await;
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    assert_eq!(read_chunk(&mut client).await, b"hello ");
    drop(client);
    assert_eq!(
        proxy.observed.wait_for_ignored_errors(1).await,
        [ErrorType::ConnectionClosed]
    );
    assert_eq!(proxy.observed.output(0), CustomOutput::default());
    let (sender, _) = proxy.observed.take_custom_senders();
    sender
        .send(Bytes::from_static(b"after close"))
        .await
        .expect("the proxy stopped reading custom messages");
    drop(sender);
    let expected = CustomOutput {
        messages: vec![Bytes::from_static(b"after close")],
        finishes: 1,
    };
    wait_until("the custom output was not finished", || {
        proxy.observed.output(0).finishes > 0
    })
    .await;
    assert_eq!(proxy.observed.output(0), expected);
    assert!(proxy.observed.completions().is_empty());
    origin.send(WORLD_CHUNK_AND_END).await;
    assert_eq!(origin.finish().await, OriginEnd::Released);
    assert_eq!(
        proxy.observed.wait_for_completions(1).await,
        [completed("GET")]
    );
    assert_eq!(proxy.observed.output(0), expected);
    proxy.assert_cached(path, b"hello world!").await;
}

/// The custom upstream's request, which must be a `GET` for `path`; the cache also sends one for
/// a `HEAD`.
async fn assert_upstream_get(upstream: &mut CustomUpstream, path: &str) {
    let request = tokio::time::timeout(TEST_TIMEOUT, &mut upstream.requested)
        .await
        .expect("the custom upstream did not get the request")
        .unwrap();
    assert_eq!(request.method, http::Method::GET);
    assert_eq!(request.uri.path(), path);
}

/// When the client of a `HEAD` miss sends its next request early.
#[derive(Clone, Copy)]
enum EarlyRequest {
    /// Once the fill has completed.
    AfterFill,
    /// While the fill still reads the body.
    DuringFill,
}

/// Sends the next request on `client` before its response has ended, which the proxy ignores as
/// a downstream error, then closes the client.
async fn send_next_request_early_then_close(proxy: &RunningProxy, client: &mut TcpStream) {
    client
        .write_all(format!("GET /next HTTP/1.1\r\nHost: {HOST}\r\n\r\n").as_bytes())
        .await
        .unwrap();
    assert_eq!(
        proxy.observed.wait_for_ignored_errors(1).await,
        [ErrorType::ConnectError]
    );
    client.shutdown().await.unwrap();
}

/// Sends the `rest` of a custom upstream's body and ends it, which completes the fill of `path`.
async fn finish_fill(
    proxy: &RunningProxy,
    path: &str,
    body: mpsc::Sender<Bytes>,
    rest: &'static [u8],
) {
    body.send(Bytes::from_static(rest)).await.unwrap();
    drop(body);
    // A follow-up reads the entry as it is written, so it ends only once the fill completes.
    proxy.assert_cached(path, b"hello world!").await;
    assert_eq!(
        proxy.observed.wait_for_completions(1).await,
        [completed("GET")]
    );
}

/// A `HEAD` miss from a custom upstream whose custom messages stay open, whose client sends its
/// next request early, either once the fill has completed or while it still reads the body, and
/// then closes. The proxy ignores the early request as a downstream error, completes the fill,
/// and keeps the request running until the custom messages end, then fails it with a write
/// error; the client gets nothing more, and the entry stays cached.
#[tokio::test]
async fn custom_upstream_early_next_request_then_close_fails_request() {
    for (path, early_request) in [
        (
            "/gate_off/custom_upstream_early_next_request_after_fill_then_close",
            EarlyRequest::AfterFill,
        ),
        (
            "/gate_off/custom_upstream_early_next_request_during_fill_then_close",
            EarlyRequest::DuringFill,
        ),
    ] {
        let (connector, mut upstream) = CustomUpstream::new();
        let proxy = RunningProxy::custom_upstream(connector, false);
        let mut client = proxy.send("HEAD", path).await;
        assert_upstream_get(&mut upstream, path).await;
        let head = read_head(&mut client).await;
        assert!(head.starts_with("HTTP/1.1 200"), "{head}");
        assert_eq!(
            header_value(&head, "x-cache-status"),
            Some("miss"),
            "{head}"
        );
        match early_request {
            EarlyRequest::AfterFill => {
                finish_fill(&proxy, path, upstream.body, b"hello world!").await;
                send_next_request_early_then_close(&proxy, &mut client).await;
            }
            EarlyRequest::DuringFill => {
                upstream
                    .body
                    .send(Bytes::from_static(b"hello "))
                    .await
                    .unwrap();
                send_next_request_early_then_close(&proxy, &mut client).await;
                finish_fill(&proxy, path, upstream.body, b"world!").await;
            }
        }
        // The custom messages are still open, so the request is still running.
        assert_eq!(proxy.observed.completions(), [completed("GET")]);
        drop(upstream.custom);
        let completions = proxy.observed.wait_for_completions(2).await;
        assert_eq!(completions[1], failed_with_errored_downstream("HEAD"));
        assert_eq!(read_rest(&mut client).await, b"");
        assert_eq!(proxy.observed.upstream_attempts(), 1);
        proxy.assert_cached(path, b"hello world!").await;
    }
}

/// A `POST` miss (the test proxy caches every method) whose custom upstream fails the request
/// body upload while it holds the response header. The proxy cancels only the forwarding of
/// custom messages to the upstream: the application's sender to the upstream is closed with
/// nothing written to or finished on the upstream, while messages from the upstream still go
/// through the proxy's filter. The complete response that follows fills the cache and reaches
/// the client; once the custom messages from the upstream end, the request succeeds.
#[tokio::test]
async fn custom_upstream_upload_failure_stops_custom_messages_to_upstream_only() {
    let path = "/gate_off/custom_upstream_upload_failure";
    let (connector, mut upstream) = CustomUpstream::failing_upload();
    let proxy = RunningProxy::custom_upstream(connector, true);
    let mut client = proxy.connect().await;
    client
        .write_all(
            format!("POST {path} HTTP/1.1\r\nHost: {HOST}\r\nContent-Length: 5\r\n\r\nhello")
                .as_bytes(),
        )
        .await
        .unwrap();
    let request = tokio::time::timeout(TEST_TIMEOUT, &mut upstream.requested)
        .await
        .expect("the custom upstream did not get the request")
        .unwrap();
    assert_eq!(request.method, http::Method::POST);
    let failed = upstream.failed_request_body.take().unwrap();
    let failed = tokio::time::timeout(TEST_TIMEOUT, failed)
        .await
        .expect("the proxy did not upload the request body")
        .unwrap();
    assert_eq!(failed, "hello");
    let (to_downstream, to_upstream) = proxy.observed.take_custom_senders();
    let to_upstream = to_upstream.expect("a custom upstream takes custom messages");
    tokio::time::timeout(TEST_TIMEOUT, to_upstream.closed())
        .await
        .expect("the proxy kept reading custom messages to the upstream");
    assert_eq!(
        *upstream.custom_output.lock().unwrap(),
        CustomOutput::default()
    );
    upstream
        .custom
        .send(Ok(Bytes::from_static(b"from upstream")))
        .await
        .unwrap();
    assert_eq!(
        proxy.observed.wait_for_filtered_from_upstream(1).await,
        [Bytes::from_static(b"from upstream")]
    );
    drop(upstream.header.take());
    upstream
        .body
        .send(Bytes::from_static(b"hello world!"))
        .await
        .unwrap();
    drop(upstream.body);
    proxy.assert_cached(path, b"hello world!").await;
    assert_eq!(
        proxy.observed.wait_for_completions(1).await,
        [completed("GET")]
    );
    drop(upstream.custom);
    drop(to_downstream);
    let completions = proxy.observed.wait_for_completions(2).await;
    assert_eq!(completions[1], completed("POST"));
    let head = read_head(&mut client).await;
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    assert_eq!(
        header_value(&head, "x-cache-status"),
        Some("miss"),
        "{head}"
    );
    assert_eq!(read_n(&mut client, 12).await, b"hello world!");
    assert_eq!(proxy.observed.upstream_attempts(), 1);
    // The proxy hands the session back to its connector before the request completes.
    assert!(
        upstream.released.try_recv().is_ok(),
        "the proxy did not hand the custom upstream's session back"
    );
    assert_eq!(
        *upstream.custom_output.lock().unwrap(),
        CustomOutput::default()
    );
}
