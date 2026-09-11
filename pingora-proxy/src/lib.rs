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

//! # pingora-proxy
//!
//! Programmable HTTP proxy built on top of [pingora_core].
//!
//! # Features
//! - HTTP/1.x and HTTP/2 for both downstream and upstream
//! - Connection pooling
//! - TLSv1.3, mutual TLS, customizable CA
//! - Request/Response scanning, modification or rejection
//! - Dynamic upstream selection
//! - Configurable retry and failover
//! - Fully programmable and customizable at any stage of a HTTP request
//!
//! # How to use
//!
//! Users of this crate defines their proxy by implementing [ProxyHttp] trait, which contains the
//! callbacks to be invoked at each stage of a HTTP request.
//!
//! Then the service can be passed into [`http_proxy_service()`] for a [pingora_core::server::Server] to
//! run it.
//!
//! See `examples/load_balancer.rs` for a detailed example.

use async_trait::async_trait;
use bytes::Bytes;
use futures::future::BoxFuture;
use futures::future::FutureExt;
use http::{header, version::Version, Method};
use log::{debug, error, trace, warn};
use once_cell::sync::Lazy;
use pingora_http::{RequestHeader, ResponseHeader};
use std::fmt::Debug;
use std::future::{poll_fn, Future};
use std::str;
use std::sync::{
    atomic::{AtomicBool, AtomicU64, AtomicU8, AtomicUsize, Ordering},
    Arc,
};
use std::task::Poll;
use std::time::Duration;
use tokio::sync::{mpsc, Notify};
use tokio::time;

use pingora_cache::NoCacheReason;
use pingora_core::apps::{
    HttpPersistentSettings, HttpServerApp, HttpServerOptions, ReusedHttpStream,
};
use pingora_core::connectors::http::custom;
use pingora_core::connectors::{http::Connector, ConnectorOptions};
use pingora_core::modules::http::compression::ResponseCompressionBuilder;
use pingora_core::modules::http::{HttpModuleCtx, HttpModules};
use pingora_core::protocols::http::client::HttpSession as ClientSession;
use pingora_core::protocols::http::custom::server::Session as DownstreamSession;
use pingora_core::protocols::http::custom::CustomMessageWrite;
use pingora_core::protocols::http::subrequest::server::SubrequestHandle;
use pingora_core::protocols::http::v1::client::HttpSession as HttpSessionV1;
#[cfg(feature = "early_body_buffer")]
use pingora_core::protocols::http::v1::common::{
    header_value_content_length, is_expect_continue_req,
};
use pingora_core::protocols::http::v2::server::H2Options;
use pingora_core::protocols::http::HttpTask;
use pingora_core::protocols::http::ServerSession as HttpSession;
use pingora_core::protocols::http::SERVER_NAME;
use pingora_core::protocols::Stream;
use pingora_core::protocols::{Digest, UniqueID};
use pingora_core::server::configuration::ServerConf;
use pingora_core::server::{RuntimeOpts, ShutdownWatch};
use pingora_core::upstreams::peer::{HttpPeer, Peer};
use pingora_error::{Error, ErrorSource, ErrorType::*, OrErr, Result};

const TASK_BUFFER_SIZE: usize = 4;

/// Caps per-proxy padding and one-time shutdown fan-out on very large hosts.
const MAX_SHUTDOWN_NOTIFY_SHARDS: usize = 256;

type DownstreamCustomMessageReader =
    Box<dyn futures::Stream<Item = Result<Bytes>> + Unpin + Send + Sync + 'static>;

mod proxy_cache;
mod proxy_common;
mod proxy_custom;
mod proxy_h1;
mod proxy_h2;
mod proxy_purge;
mod proxy_trait;
pub mod subrequest;

use subrequest::{BodyMode, Ctx as SubrequestCtx};

pub use proxy_cache::range_filter::{range_header_filter, MultiRangeInfo, RangeType};
pub use proxy_purge::PurgeStatus;
pub use proxy_trait::{FailToProxy, ProxyHttp, ProxyWarnLogContext};

pub mod prelude {
    pub use crate::{http_proxy, http_proxy_service, ProxyHttp, ProxyWarnLogContext, Session};
}

/// Type-erased custom-connection callback.
///
/// The callback remains boxed once per accepted custom connection; the
/// downstream session operations themselves use concrete futures.
pub type ProcessCustomSession<SV, C, DS = ()> = Arc<
    dyn Fn(Arc<HttpProxy<SV, C, DS>>, Stream, &ShutdownWatch) -> BoxFuture<'static, Option<Stream>>
        + Send
        + Sync
        + Unpin
        + 'static,
>;

/// Shutdown [`Notify`] sharded by worker thread.
///
/// Every request that parks in `read_request()` registers a shutdown waiter and
/// unregisters it when the read completes. Both operations lock the `Notify`'s
/// internal mutex, so a single `Notify` shared across the whole proxy becomes a
/// contention hot spot on many-core machines. Sharding keeps waiter
/// registration on a (mostly) thread-local shard while shutdown notifies every
/// shard.
struct ShardedNotify {
    shards: Box<[NotifyShard]>,
}

/// Align each shard so its [`Notify`] state and waiter-list mutex do not share a
/// cache line with an adjacent shard. Without padding, writes made while adding
/// or removing waiters can falsely share a cache line with an independent shard,
/// forcing cache-coherence protocols such as MESI to transfer or invalidate that
/// line between cores. These transfers are especially expensive when they cross
/// the interconnect between sockets on a NUMA system.
///
/// The 128-byte alignment separates adjacent shards on systems with common
/// 64- or 128-byte cache lines. This trades bounded padding for avoiding false
/// sharing between shards; waiters assigned to the same shard can still contend.
#[repr(align(128))]
struct NotifyShard(Notify);

impl ShardedNotify {
    /// Create enough shards for the configured worker threads, rounded up
    /// to preserve mask-based indexing and bounded by [`MAX_SHUTDOWN_NOTIFY_SHARDS`].
    fn new(worker_threads: usize) -> Self {
        let shards = worker_threads
            .max(1)
            .checked_next_power_of_two()
            .unwrap_or(MAX_SHUTDOWN_NOTIFY_SHARDS)
            .min(MAX_SHUTDOWN_NOTIFY_SHARDS);
        ShardedNotify {
            shards: (0..shards).map(|_| NotifyShard(Notify::new())).collect(),
        }
    }

    /// Return the shard assigned to the current thread.
    ///
    /// A task can migrate after registering, but its [`Notified`](tokio::sync::futures::Notified)
    /// future remains bound to this shard and shutdown notifies every shard.
    fn local(&self) -> &Notify {
        static NEXT_THREAD_ID: AtomicUsize = AtomicUsize::new(0);
        thread_local! {
            static THREAD_ID: usize = NEXT_THREAD_ID.fetch_add(1, Ordering::Relaxed);
        }
        let id = THREAD_ID.with(|id| *id);
        // the shard count is a power of two
        &self.shards[id & (self.shards.len() - 1)].0
    }

    /// Notify waiters on every shard, including tasks polled by a different
    /// worker after registering.
    fn notify_waiters(&self) {
        for shard in self.shards.iter() {
            shard.0.notify_waiters();
        }
    }
}

/// The concrete type that holds the user defined HTTP proxy.
///
/// Users don't need to interact with this object directly.
pub struct HttpProxy<SV, C = (), DS = ()>
where
    C: custom::Connector, // Upstream custom connector
    DS: DownstreamSession,
{
    inner: SV, // TODO: name it better than inner
    client_upstream: Connector<C>,
    shutdown: ShardedNotify,
    shutdown_flag: Arc<AtomicBool>,
    pub server_options: Option<HttpServerOptions>,
    pub h2_options: Option<H2Options>,
    pub downstream_modules: HttpModules,
    #[cfg(feature = "upstream_modules")]
    pub upstream_modules: HttpModules,
    max_retries: usize,
    process_custom_session: Option<ProcessCustomSession<SV, C, DS>>,
}

impl<SV> HttpProxy<SV, (), ()> {
    /// Create a new [`HttpProxy`] with the given [`ProxyHttp`] implementation and [`ServerConf`].
    ///
    /// After creating an `HttpProxy`, you should call [`HttpProxy::handle_init_modules()`] to
    /// initialize the downstream modules before processing requests.
    ///
    /// For most use cases, prefer using [`http_proxy_service()`] which wraps the `HttpProxy` in a
    /// [`Service`]. This constructor is useful when you need to integrate `HttpProxy` into a custom
    /// accept loop (e.g., for SNI-based routing decisions before TLS termination).
    ///
    /// # Example
    ///
    /// ```ignore
    /// use pingora_proxy::HttpProxy;
    /// use std::sync::Arc;
    ///
    /// let mut proxy = HttpProxy::new(my_proxy_app, server_conf);
    /// proxy.handle_init_modules();
    /// let proxy = Arc::new(proxy);
    /// // Use proxy.process_new_http() in your custom accept loop
    /// ```
    pub fn new(inner: SV, conf: Arc<ServerConf>) -> Self {
        HttpProxy {
            inner,
            client_upstream: Connector::new(Some(ConnectorOptions::from_server_conf(&conf))),
            shutdown: ShardedNotify::new(conf.threads),
            shutdown_flag: Arc::new(AtomicBool::new(false)),
            server_options: None,
            h2_options: None,
            downstream_modules: HttpModules::new(),
            #[cfg(feature = "upstream_modules")]
            upstream_modules: HttpModules::new(),
            max_retries: conf.max_retries,
            process_custom_session: None,
        }
    }
}

impl<SV, C, DS> HttpProxy<SV, C, DS>
where
    C: custom::Connector,
    DS: DownstreamSession,
{
    fn new_custom(
        inner: SV,
        conf: Arc<ServerConf>,
        connector: C,
        on_custom: Option<ProcessCustomSession<SV, C, DS>>,
        server_options: Option<HttpServerOptions>,
        client_options: Option<ConnectorOptions>,
    ) -> Self
    where
        SV: ProxyHttp<DS> + Send + Sync + 'static,
        <SV as ProxyHttp<DS>>::CTX: Send + Sync,
    {
        let client_options =
            client_options.unwrap_or_else(|| ConnectorOptions::from_server_conf(&conf));
        let client_upstream = Connector::new_custom(Some(client_options), connector);

        HttpProxy {
            inner,
            client_upstream,
            shutdown: ShardedNotify::new(conf.threads),
            shutdown_flag: Arc::new(AtomicBool::new(false)),
            server_options,
            downstream_modules: HttpModules::new(),
            #[cfg(feature = "upstream_modules")]
            upstream_modules: HttpModules::new(),
            max_retries: conf.max_retries,
            process_custom_session: on_custom,
            h2_options: None,
        }
    }

    /// Return the number of times a pooled upstream connection was found to contain
    /// unexpected data from the server.
    pub fn unexpected_data_connection_count(&self) -> u64 {
        self.client_upstream.unexpected_data_connection_count()
    }

    /// Return a shared reference to the unexpected data connection counter for periodic metric reporting.
    pub fn unexpected_data_connection_counter(&self) -> Arc<AtomicU64> {
        self.client_upstream.unexpected_data_connection_counter()
    }

    /// Initialize the downstream modules for this proxy.
    ///
    /// This method must be called after creating an [`HttpProxy`] with [`HttpProxy::new()`]
    /// and before processing any requests. It invokes [`ProxyHttp::init_downstream_modules()`]
    /// to set up any HTTP modules configured by the user's proxy implementation.
    ///
    /// Note: When using [`http_proxy_service()`] or [`http_proxy_service_with_name()`],
    /// this method is called automatically.
    pub fn handle_init_modules(&mut self)
    where
        SV: ProxyHttp<DS>,
    {
        self.inner
            .init_downstream_modules(&mut self.downstream_modules);
        #[cfg(feature = "upstream_modules")]
        self.inner.init_upstream_modules(&mut self.upstream_modules);
    }

    /// Resolve when `http_cleanup()` has been called.
    ///
    /// The waiter is registered on the current thread's shard before
    /// `shutdown_flag` is checked, so a shutdown firing in between cannot be
    /// missed: either the flag load sees the store, or the registered waiter
    /// receives the notification.
    async fn await_shutdown(&self) {
        let notified = self.shutdown.local().notified();
        tokio::pin!(notified);

        poll_fn(|context| {
            if notified.as_mut().poll(context).is_ready()
                || self.shutdown_flag.load(Ordering::Acquire)
            {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        })
        .await;
    }

    async fn handle_new_request(
        &self,
        mut downstream_session: Box<HttpSession<DS>>,
    ) -> Option<Box<HttpSession<DS>>>
    where
        SV: ProxyHttp<DS> + Send + Sync,
        <SV as ProxyHttp<DS>>::CTX: Send + Sync,
    {
        // phase 1 read request header

        let res = tokio::select! {
            biased; // biased select is cheaper, and we don't want to drop already buffered requests
            res = downstream_session.read_request() => { res }
            _ = self.await_shutdown() => {
                // service shutting down, dropping the connection to stop more req from coming in
                return None;
            }
        };
        match res {
            Ok(true) => {
                // TODO: check n==0
                debug!("Successfully get a new request");
            }
            Ok(false) => {
                return None; // TODO: close connection?
            }
            Err(mut e) => {
                e.as_down();
                if matches!(e.etype, InvalidHTTPHeader) {
                    debug!(
                        "Fail to proxy: {e}, downstream session type: {}",
                        downstream_session.session_type()
                    );
                    downstream_session
                        .respond_error(400)
                        .await
                        .unwrap_or_else(|e| {
                            error!("failed to send error response to downstream: {e}");
                        });
                } else {
                    // otherwise the connection must be broken, no need to send anything
                    error!(
                        "Fail to proxy: {e}, downstream session type: {}",
                        downstream_session.session_type()
                    );
                }
                downstream_session.shutdown().await;
                return None;
            }
        }
        trace!(
            "Request header: {:?}",
            downstream_session.req_header().as_ref()
        );
        // CONNECT method proxying is not default supported by the proxy http logic itself,
        // since the tunneling process changes the request-response flow.
        // https://datatracker.ietf.org/doc/html/rfc9110#name-connect
        // Also because the method impacts message framing in a way is currently unaccounted for
        // (https://datatracker.ietf.org/doc/html/rfc9112#section-6.3-2.2)
        // it is safest to disallow use of the method by default.
        if !self
            .server_options
            .as_ref()
            .is_some_and(|opts| opts.allow_connect_method_proxying)
            && downstream_session.req_header().method == Method::CONNECT
        {
            downstream_session
                .respond_error(405)
                .await
                .unwrap_or_else(|e| {
                    error!("failed to send error response to downstream: {e}");
                });
            downstream_session.shutdown().await;
            return None;
        }
        Some(downstream_session)
    }

    // return bool: server_session can be reused, and error if any
    async fn proxy_to_upstream(
        &self,
        session: &mut Session<DS>,
        ctx: &mut <SV as ProxyHttp<DS>>::CTX,
    ) -> (bool, Option<Box<Error>>)
    where
        SV: ProxyHttp<DS> + Send + Sync,
        <SV as ProxyHttp<DS>>::CTX: Send + Sync,
    {
        let peer = match self.inner.upstream_peer(session, ctx).await {
            Ok(p) => p,
            Err(e) => return (false, Some(e)),
        };

        let client_session = self.client_upstream.get_http_session(&*peer).await;
        match client_session {
            Ok((client_session, client_reused)) => {
                let (server_reused, error) = match client_session {
                    ClientSession::H1(mut h1) => {
                        let (server_reused, client_reuse, error) = self
                            .proxy_to_h1_upstream(session, &mut h1, client_reused, &peer, ctx)
                            .await;
                        if client_reuse {
                            let session = ClientSession::H1(h1);
                            self.client_upstream
                                .release_http_session(session, &*peer, peer.idle_timeout())
                                .await;
                        }
                        (server_reused, error)
                    }
                    ClientSession::H2(mut h2) => {
                        let (server_reused, mut error) = self
                            .proxy_to_h2_upstream(session, &mut h2, client_reused, &peer, ctx)
                            .await;
                        let session = ClientSession::H2(h2);
                        self.client_upstream
                            .release_http_session(session, &*peer, peer.idle_timeout())
                            .await;

                        if let Some(e) = error.as_mut() {
                            // try to downgrade if A. origin says so or B. origin sends an invalid
                            // response, which usually means origin h2 is not production ready
                            if matches!(e.etype, H2Downgrade | InvalidH2) {
                                if peer
                                    .get_alpn()
                                    .is_none_or(|alpn| alpn.get_min_http_version() == 1)
                                {
                                    // Add the peer to prefer h1 so that all following requests
                                    // will use h1
                                    self.client_upstream.prefer_h1(&*peer);
                                } else {
                                    // the peer doesn't allow downgrading to h1 (e.g. gRPC)
                                    e.retry = false.into();
                                }
                            }
                        }

                        (server_reused, error)
                    }
                    ClientSession::Custom(mut c) => {
                        let (server_reused, error) = self
                            .proxy_to_custom_upstream(session, &mut c, client_reused, &peer, ctx)
                            .await;
                        let session = ClientSession::Custom(c);
                        self.client_upstream
                            .release_http_session(session, &*peer, peer.idle_timeout())
                            .await;
                        (server_reused, error)
                    }
                };
                (
                    server_reused,
                    error.map(|e| {
                        self.inner
                            .error_while_proxy(&peer, session, e, ctx, client_reused)
                    }),
                )
            }
            Err(mut e) => {
                e.as_up();
                let new_err = self.inner.fail_to_connect(session, &peer, ctx, e);
                (false, Some(new_err.into_up()))
            }
        }
    }

    async fn upstream_filter(
        &self,
        session: &mut Session<DS>,
        task: &mut HttpTask,
        ctx: &mut <SV as ProxyHttp<DS>>::CTX,
    ) -> Result<Option<Duration>>
    where
        SV: ProxyHttp<DS> + Send + Sync,
        <SV as ProxyHttp<DS>>::CTX: Send + Sync,
    {
        let duration = match task {
            HttpTask::Header(header, _eos) => {
                self.inner
                    .upstream_response_filter(session, header, ctx)
                    .await?;
                None
            }
            HttpTask::Body(data, eos) | HttpTask::UpgradedBody(data, eos) => {
                self.inner
                    .upstream_response_body_filter(session, data, *eos, ctx)
                    .await?
            }
            HttpTask::Trailer(Some(trailers)) => {
                self.inner
                    .upstream_response_trailer_filter(session, trailers, ctx)
                    .await?;
                None
            }
            _ => {
                // task does not support a filter
                None
            }
        };

        Ok(duration)
    }

    async fn finish(
        &self,
        mut session: Session<DS>,
        ctx: &mut <SV as ProxyHttp<DS>>::CTX,
        reuse: bool,
        error: Option<Box<Error>>,
    ) -> Option<ReusedHttpStream>
    where
        SV: ProxyHttp<DS> + Send + Sync,
        <SV as ProxyHttp<DS>>::CTX: Send + Sync,
    {
        self.inner
            .logging(&mut session, error.as_deref(), ctx)
            .await;

        if let Some(e) = error {
            session.downstream_session.on_proxy_failure(e);
        }

        if reuse {
            // TODO: log error
            let mut persistent_settings = HttpPersistentSettings::for_session(&session);
            if let Some(uc) = self.inner.persist_connection_context(&session, ctx) {
                persistent_settings.set_user_context(uc);
            }
            session
                .downstream_session
                .finish()
                .await
                .ok()
                .flatten()
                .map(|s| ReusedHttpStream::from_reusable_stream(s, persistent_settings))
        } else {
            None
        }
    }

    fn cleanup_sub_req(&self, session: &mut Session<DS>) {
        if let Some(ctx) = session.subrequest_ctx.as_mut() {
            ctx.release_write_lock();
        }
    }
}

use pingora_cache::HttpCache;
use pingora_core::protocols::http::compression::ResponseCompressionCtx;

/// The established HTTP session
///
/// This object is what users interact with in order to access the request itself or change the proxy
/// behavior.
pub struct Session<DS = ()>
where
    DS: DownstreamSession,
{
    /// the HTTP session to downstream (the client)
    pub downstream_session: Box<HttpSession<DS>>,
    /// The interface to control HTTP caching
    pub cache: HttpCache,
    /// (de)compress responses coming into the proxy (from upstream)
    pub upstream_compression: ResponseCompressionCtx,
    /// ignore downstream range (skip downstream range filters)
    pub ignore_downstream_range: bool,
    /// Were the upstream request headers modified?
    pub upstream_headers_mutated_for_cache: bool,
    /// Upstream predicate for whether this HTTP/1 request is an upgrade.
    h1_upgrade_request_status: H1UpgradeRequestStatus,
    /// The context from parent request, if this is a subrequest.
    pub subrequest_ctx: Option<Box<SubrequestCtx>>,
    /// Handle to allow spawning subrequests, assigned by the `Subrequest` app logic.
    pub subrequest_spawner: Option<SubrequestSpawner<DS>>,
    // Downstream filter modules
    pub downstream_modules_ctx: HttpModuleCtx,
    /// Upstream filter modules. These run before `upstream_compression` and see the raw
    /// (pre-compression) upstream response body.
    #[cfg(feature = "upstream_modules")]
    pub upstream_modules_ctx: HttpModuleCtx,
    /// Upstream response body bytes received (payload only). Set by proxy layer.
    /// TODO: move this into an upstream session digest for future fields.
    upstream_body_bytes_received: usize,
    /// Request body bytes written to the upstream (payload only). Set by proxy layer.
    ///
    /// `None` when the proxy layer does not track it (HTTP/2 and custom upstreams), which is
    /// deliberately distinct from `Some(0)` so that "not measured" cannot be mistaken for
    /// "a request body was dropped".
    upstream_body_bytes_sent: Option<usize>,
    /// Whether proxy task filtering has seen a downstream 101 upgrade header.
    downstream_task_seen_upgraded: bool,
    /// Upstream write pending time. Set by proxy layer (HTTP/1.x only).
    upstream_write_pending_time: Duration,
    /// Flag that is set when the shutdown process has begun.
    shutdown_flag: Arc<AtomicBool>,
    /// Request body buffered early (before upstream connection) for auth/routing decisions.
    /// When set, body forwarding will use this instead of re-reading from downstream. It must
    /// outlive the first attempt so a retry can replay it.
    /// Use accessor methods: `get_buffered_body()`, `set_buffered_body()`.
    #[cfg(feature = "early_body_buffer")]
    buffered_request_body: Option<Bytes>,
    /// Whether body has been fully consumed for buffering.
    /// Use accessor: `is_body_buffered()`.
    #[cfg(feature = "early_body_buffer")]
    body_buffered: bool,
}

impl<DS> Session<DS>
where
    DS: DownstreamSession,
{
    fn new(
        downstream_session: impl Into<Box<HttpSession<DS>>>,
        downstream_modules: &HttpModules,
        #[cfg(feature = "upstream_modules")] upstream_modules: &HttpModules,
        shutdown_flag: Arc<AtomicBool>,
    ) -> Self {
        Session {
            downstream_session: downstream_session.into(),
            cache: HttpCache::new(),
            // disable both upstream and downstream compression
            upstream_compression: ResponseCompressionCtx::new(0, false, false),
            ignore_downstream_range: false,
            upstream_headers_mutated_for_cache: false,
            h1_upgrade_request_status: H1UpgradeRequestStatus::default(),
            subrequest_ctx: None,
            subrequest_spawner: None, // optionally set later on
            downstream_modules_ctx: downstream_modules.build_ctx(),
            #[cfg(feature = "upstream_modules")]
            upstream_modules_ctx: upstream_modules.build_ctx(),
            upstream_body_bytes_received: 0,
            upstream_body_bytes_sent: None,
            downstream_task_seen_upgraded: false,
            upstream_write_pending_time: Duration::ZERO,
            shutdown_flag,
            #[cfg(feature = "early_body_buffer")]
            buffered_request_body: None,
            #[cfg(feature = "early_body_buffer")]
            body_buffered: false,
        }
    }

    /// Run upstream module filters on the given [`HttpTask`].
    ///
    /// Upstream modules process each task **before** `upstream_compression` and
    /// see the raw (pre-compression) upstream response. Like the downstream
    /// module path, `response_trailer_filter` and `response_done_filter` return
    /// values are converted to body tasks when present.
    #[cfg(feature = "upstream_modules")]
    pub async fn upstream_modules_filter_task(&mut self, t: &mut HttpTask) -> Result<()> {
        match t {
            HttpTask::Header(header, eos) => {
                self.upstream_modules_ctx
                    .response_header_filter(header, *eos)
                    .await?;
            }
            HttpTask::Body(body, eos) | HttpTask::UpgradedBody(body, eos) => {
                self.upstream_modules_ctx.response_body_filter(body, *eos)?;
            }
            HttpTask::Trailer(trailers) => {
                if let Some(buf) = self
                    .upstream_modules_ctx
                    .response_trailer_filter(trailers)?
                {
                    *t = HttpTask::Body(Some(buf), true);
                }
            }
            HttpTask::Done => {
                if let Some(buf) = self.upstream_modules_ctx.response_done_filter()? {
                    *t = HttpTask::Body(Some(buf), true);
                }
            }
            HttpTask::Failed(_) => {}
        }
        Ok(())
    }

    pub fn as_downstream_mut(&mut self) -> &mut HttpSession<DS> {
        &mut self.downstream_session
    }

    pub fn as_downstream(&self) -> &HttpSession<DS> {
        &self.downstream_session
    }

    /// Write HTTP response with the given error code to the downstream.
    pub async fn respond_error(&mut self, error: u16) -> Result<()> {
        self.as_downstream_mut().respond_error(error).await
    }

    /// Write HTTP response with the given error code to the downstream with a body.
    pub async fn respond_error_with_body(&mut self, error: u16, body: Bytes) -> Result<()> {
        self.as_downstream_mut()
            .respond_error_with_body(error, body)
            .await
    }

    /// Write the given HTTP response header to the downstream
    ///
    /// Different from directly calling [HttpSession::write_response_header], this function also
    /// invokes the filter modules.
    pub async fn write_response_header(
        &mut self,
        mut resp: Box<ResponseHeader>,
        end_of_stream: bool,
    ) -> Result<()> {
        self.downstream_modules_ctx
            .response_header_filter(&mut resp, end_of_stream)
            .await?;
        self.downstream_session.write_response_header(resp).await
    }

    /// Similar to `write_response_header()`, this fn will clone the `resp` internally
    pub async fn write_response_header_ref(
        &mut self,
        resp: &ResponseHeader,
        end_of_stream: bool,
    ) -> Result<(), Box<Error>> {
        self.write_response_header(Box::new(resp.clone()), end_of_stream)
            .await
    }

    /// Write the given HTTP response body chunk to the downstream
    ///
    /// Different from directly calling [HttpSession::write_response_body], this function also
    /// invokes the filter modules.
    pub async fn write_response_body(
        &mut self,
        mut body: Option<Bytes>,
        end_of_stream: bool,
    ) -> Result<()> {
        self.downstream_modules_ctx
            .response_body_filter(&mut body, end_of_stream)?;

        if body.is_none() && !end_of_stream {
            return Ok(());
        }

        let data = body.unwrap_or_default();
        self.downstream_session
            .write_response_body(data, end_of_stream)
            .await
    }

    // Run downstream module response filters on a single task, updating
    // `seen_upgraded` to track whether an upgrade has been seen. Used by both
    // `send_downstream_proxy_task` and `write_response_tasks`.
    async fn downstream_response_task_filter(
        &mut self,
        task: &mut HttpTask,
        seen_upgraded: &mut bool,
    ) -> Result<()> {
        match task {
            HttpTask::Header(resp, end) => {
                if *seen_upgraded {
                    return reject_unexpected_task_after_h1_upgrade(self, "header", *seen_upgraded);
                }
                self.downstream_modules_ctx
                    .response_header_filter(resp, *end)
                    .await?;
                reject_mismatched_h1_upgrade_101(self, resp, "downstream_module_header_filter")
                    .map_err(|e| e.into_in())?;
                if resp.status == http::StatusCode::SWITCHING_PROTOCOLS
                    && self.downstream_session.is_upgrade(resp) == Some(true)
                {
                    *seen_upgraded = true;
                }
            }
            HttpTask::Body(data, end) => {
                if *seen_upgraded {
                    return reject_unexpected_task_after_h1_upgrade(self, "body", *seen_upgraded);
                }
                self.downstream_modules_ctx
                    .response_body_filter(data, *end)?;
            }
            HttpTask::UpgradedBody(data, end) => {
                if !*seen_upgraded {
                    return reject_unexpected_upgraded_body_before_h1_upgrade(self, *seen_upgraded);
                }
                self.downstream_modules_ctx
                    .response_body_filter(data, *end)?;
            }
            HttpTask::Trailer(trailers) => {
                if *seen_upgraded {
                    return reject_unexpected_task_after_h1_upgrade(
                        self,
                        "trailer",
                        *seen_upgraded,
                    );
                }
                if let Some(buf) = self
                    .downstream_modules_ctx
                    .response_trailer_filter(trailers)?
                {
                    // Write the trailers into the body if the filter
                    // returns a buffer.
                    //
                    // Note, this will not work if end of stream has already
                    // been seen or we've written content-length bytes.
                    // (Trailers should never come after upgraded body)
                    *task = HttpTask::Body(Some(buf), true);
                }
            }
            HttpTask::Done => {
                // `Done` can be sent in certain response paths to mark end
                // of response if not already done via trailers or body with
                // end flag set.
                // If the filter returns body bytes on Done,
                // write them into the response. After a 101, those bytes are
                // already in the upgraded protocol and must not be HTTP-framed.
                //
                // Note, this will not work if end of stream has already
                // been seen or we've written content-length bytes.
                if let Some(buf) = self.downstream_modules_ctx.response_done_filter()? {
                    *task = if *seen_upgraded {
                        HttpTask::UpgradedBody(Some(buf), true)
                    } else {
                        HttpTask::Body(Some(buf), true)
                    };
                }
            }
            _ => { /* Failed */ }
        }
        Ok(())
    }

    /// Queue a downstream proxy task for cancel-safe writing after running
    /// downstream module filters. This allows decoupling cache writes from
    /// downstream writes.
    ///
    /// Only works with sessions that support the proxy task API.
    ///
    /// # Panics
    /// Panics if the session doesn't support the proxy task API.
    /// Use `write_response_tasks()` for sessions that don't support the proxy task API.
    pub async fn send_downstream_proxy_task(&mut self, mut task: HttpTask) -> Result<()> {
        let mut seen_upgraded = self.downstream_task_seen_upgraded || self.was_upgraded();
        self.downstream_response_task_filter(&mut task, &mut seen_upgraded)
            .await?;
        self.downstream_task_seen_upgraded = seen_upgraded;
        self.downstream_session.send_downstream_proxy_task(task);
        Ok(())
    }

    /// Enable or disable the cancel-safe proxy task API for this session.
    ///
    /// When disabled, the proxy falls back to the blocking `write_response_tasks`
    /// path. This can be called from request filters to opt out on a per-request
    /// basis.
    pub fn set_proxy_tasks_enabled(&mut self, enabled: bool) {
        self.downstream_session.set_proxy_tasks_enabled(enabled);
    }

    /// Check if there are pending downstream tasks queued for writing.
    /// Used for backpressure - don't queue more cache tasks if we have pending writes.
    /// Returns false for sessions that don't support the proxy task API.
    pub fn has_pending_downstream_tasks(&self) -> bool {
        self.downstream_session.supports_proxy_task_api()
            && self.downstream_session.has_pending_downstream_proxy_tasks()
    }

    /// Write all queued downstream proxy tasks. This is cancel-safe and can be called
    /// in a select! loop while waiting for upstream tasks.
    /// For sessions that don't support the proxy task API, this is a no-op.
    pub async fn write_downstream_proxy_tasks(&mut self) -> Result<bool> {
        if self.downstream_session.supports_proxy_task_api() {
            self.downstream_session.write_downstream_proxy_tasks().await
        } else {
            Ok(false)
        }
    }

    pub async fn write_response_tasks(&mut self, mut tasks: Vec<HttpTask>) -> Result<bool> {
        let mut seen_upgraded = self.downstream_task_seen_upgraded || self.was_upgraded();
        for task in tasks.iter_mut() {
            self.downstream_response_task_filter(task, &mut seen_upgraded)
                .await?;
        }
        self.downstream_task_seen_upgraded = seen_upgraded;
        self.downstream_session.response_duplex_vec(tasks).await
    }

    /// Mark the upstream headers as modified by caching. This should lead to range filters being
    /// skipped when responding to the downstream.
    pub fn mark_upstream_headers_mutated_for_cache(&mut self) {
        self.upstream_headers_mutated_for_cache = true;
    }

    /// Check whether the upstream headers were marked as mutated during the request.
    pub fn upstream_headers_mutated_for_cache(&self) -> bool {
        self.upstream_headers_mutated_for_cache
    }

    fn set_upstream_h1_upgrade_request_status(&mut self, upstream_is_upgrade_req: bool) {
        self.h1_upgrade_request_status = H1UpgradeRequestStatus {
            upstream: Some(upstream_is_upgrade_req),
        };
    }

    fn h1_upgrade_request_snapshot(&self) -> H1UpgradeRequestSnapshot {
        H1UpgradeRequestSnapshot {
            downstream: self.downstream_session.is_upgrade_req(),
            upstream: self.h1_upgrade_request_status.upstream,
        }
    }

    /// Get the total upstream response body bytes received (payload only) recorded by the proxy layer.
    pub fn upstream_body_bytes_received(&self) -> usize {
        self.upstream_body_bytes_received
    }

    /// Set the total upstream response body bytes received (payload only). Intended for internal use by proxy layer.
    pub(crate) fn set_upstream_body_bytes_received(&mut self, n: usize) {
        self.upstream_body_bytes_received = n;
    }

    /// Get the request body bytes written to the upstream (payload only) recorded by the proxy
    /// layer.
    ///
    /// Returns `None` when the proxy layer does not track it (HTTP/2 and custom upstreams).
    pub fn upstream_body_bytes_sent(&self) -> Option<usize> {
        self.upstream_body_bytes_sent
    }

    /// Set the request body bytes written to the upstream (payload only). Intended for internal
    /// use by proxy layer.
    pub(crate) fn set_upstream_body_bytes_sent(&mut self, n: usize) {
        self.upstream_body_bytes_sent = Some(n);
    }

    /// Get the upstream write pending time recorded by the proxy layer. Returns [`Duration::ZERO`] for HTTP/2.
    pub fn upstream_write_pending_time(&self) -> Duration {
        self.upstream_write_pending_time
    }

    /// Set the upstream write pending time. Intended for internal use by proxy layer.
    pub(crate) fn set_upstream_write_pending_time(&mut self, d: Duration) {
        self.upstream_write_pending_time = d;
    }

    /// Is the proxy process in the process of shutting down (e.g. due to graceful upgrade)?
    pub fn is_process_shutting_down(&self) -> bool {
        self.shutdown_flag.load(Ordering::Acquire)
    }

    /// Returns a reference to the fully buffered request body, if non-empty.
    ///
    /// The body may be buffered automatically when
    /// [`ProxyHttp::early_request_body_buffer_limit()`] returns `Some(max_size)`, or supplied by
    /// application code through [`Self::set_buffered_body()`].
    #[cfg(feature = "early_body_buffer")]
    pub fn get_buffered_body(&self) -> Option<&Bytes> {
        self.buffered_request_body.as_ref()
    }

    /// Sets a fully consumed request body for upstream forwarding.
    ///
    /// The body is retained and replayed across upstream retries. `None` marks the request body as
    /// fully consumed and empty.
    ///
    /// When automatic early buffering is enabled, call this from
    /// [`ProxyHttp::request_filter()`], after Pingora has assembled the filtered chunks. Do not
    /// call it from [`ProxyHttp::early_request_body_filter()`]; that callback must modify its
    /// `body` argument, and the buffering loop may overwrite the body you set when it stores the
    /// assembled result.
    ///
    /// Application code calling this directly must first fully consume the downstream body and
    /// handle `Expect: 100-continue` before reading. It must also update `Content-Length` and
    /// `Transfer-Encoding` to describe the supplied body.
    #[cfg(feature = "early_body_buffer")]
    pub fn set_buffered_body(&mut self, body: Option<Bytes>) {
        self.body_buffered = true;
        self.buffered_request_body = body;
    }

    /// Returns whether a body has been buffered (or confirmed empty).
    ///
    /// When `true`, the body has been fully read and is available via `get_buffered_body()`,
    /// or the request has no body. Body forwarding will skip re-reading from downstream.
    #[cfg(feature = "early_body_buffer")]
    pub fn is_body_buffered(&self) -> bool {
        self.body_buffered
    }

    /// Marks the body as buffered without setting a body.
    ///
    /// Used when buffering confirms that the request body is empty.
    #[cfg(feature = "early_body_buffer")]
    pub fn mark_body_buffered(&mut self) {
        self.body_buffered = true;
    }

    /// Creates a Session from an H1 HttpSession (for testing only).
    #[cfg(all(test, feature = "early_body_buffer"))]
    pub fn new_h1_with_http_session(
        http_session: pingora_core::protocols::http::v1::server::HttpSession,
    ) -> Self {
        use pingora_core::protocols::http::ServerSession;

        Self::new(
            Box::new(ServerSession::H1(http_session)),
            &HttpModules::new(),
            #[cfg(feature = "upstream_modules")]
            &HttpModules::new(),
            Arc::new(AtomicBool::new(false)),
        )
    }

    pub fn downstream_custom_message(&mut self) -> Result<Option<DownstreamCustomMessageReader>> {
        if let Some(custom_session) = self.downstream_session.as_custom_mut() {
            custom_session
                .take_custom_message_reader()
                .map(Some)
                .ok_or(Error::explain(
                    ReadError,
                    "can't extract custom reader from downstream",
                ))
        } else {
            Ok(None)
        }
    }

    fn take_downstream_custom_message_reader(
        &mut self,
        downstream_custom_message_writer: &mut Option<Box<dyn CustomMessageWrite>>,
    ) -> Result<Option<DownstreamCustomMessageReader>> {
        if downstream_custom_message_writer.is_none() {
            return Ok(None);
        }

        let Some(custom_session) = self.downstream_session.as_custom_mut() else {
            return Ok(None);
        };

        let Some(reader) = custom_session.take_custom_message_reader() else {
            if let Some(writer) = downstream_custom_message_writer.take() {
                custom_session.restore_custom_message_writer(writer)?;
            }
            return Err(Error::explain(
                ReadError,
                "can't extract custom reader from downstream",
            ));
        };

        Ok(Some(reader))
    }
}

impl Session<()> {
    /// Create a new [Session] from the given [Stream]
    ///
    /// This function is mostly used for testing and mocking, given the downstream modules and
    /// shutdown flags will never be set.
    pub fn new_h1(stream: Stream) -> Self {
        let modules = HttpModules::new();
        Self::new(
            Box::new(HttpSession::new_http1(stream)),
            &modules,
            #[cfg(feature = "upstream_modules")]
            &HttpModules::new(),
            Arc::new(AtomicBool::new(false)),
        )
    }

    /// Create a new [Session] from the given [Stream] with modules
    ///
    /// This function is mostly used for testing and mocking, given the shutdown flag will never be
    /// set.
    pub fn new_h1_with_modules(stream: Stream, downstream_modules: &HttpModules) -> Self {
        Self::new(
            Box::new(HttpSession::new_http1(stream)),
            downstream_modules,
            #[cfg(feature = "upstream_modules")]
            &HttpModules::new(),
            Arc::new(AtomicBool::new(false)),
        )
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct H1UpgradeRequestStatus {
    upstream: Option<bool>,
}

#[derive(Clone, Copy, Debug)]
struct H1UpgradeRequestSnapshot {
    downstream: bool,
    upstream: Option<bool>,
}

impl H1UpgradeRequestSnapshot {
    fn mismatch(self) -> bool {
        // No upstream predicate means this helper cannot prove a mismatch. The
        // current proxy paths record it before upstream responses can be handled.
        matches!(self.upstream, Some(upstream) if self.downstream != upstream)
    }
}

/// Rejects a 101 response when the downstream and upstream H1 upgrade state differs.
///
/// Upstream and downstream must agree that this request is an upgrade before a
/// 101 can establish a tunnel. Otherwise one side changes protocol while the
/// other stays in HTTP handling, allowing tunneled traffic to bypass request
/// processing or corrupt the connection state.
fn reject_mismatched_h1_upgrade_101<DS>(
    session: &Session<DS>,
    header: &ResponseHeader,
    stage: &'static str,
) -> Result<()>
where
    DS: DownstreamSession,
{
    if header.status != http::StatusCode::SWITCHING_PROTOCOLS {
        return Ok(());
    }

    let status = session.h1_upgrade_request_snapshot();
    if status.mismatch() {
        return Error::e_explain(
            InvalidHTTPHeader,
            format!(
                "received 101 response with mismatched upstream/downstream upgrade status: stage={stage}, downstream_upgrade_req={}, upstream_upgrade_req={:?}, downstream_was_upgraded={}, downstream_task_seen_upgraded={}, response_version={:?}, response_upgrade_header_present={}, response_connection_header_present={}",
                status.downstream,
                status.upstream,
                session.was_upgraded(),
                session.downstream_task_seen_upgraded,
                header.version,
                header.headers.get(http::header::UPGRADE).is_some(),
                header.headers.get(http::header::CONNECTION).is_some(),
            ),
        );
    }
    Ok(())
}

fn reject_unexpected_task_after_h1_upgrade<DS>(
    session: &Session<DS>,
    task: &'static str,
    task_filter_seen_upgraded: bool,
) -> Result<()>
where
    DS: DownstreamSession,
{
    let status = session.h1_upgrade_request_snapshot();
    Error::e_explain(
        InvalidHTTPHeader,
        format!(
            "received {task} task after downstream 101 upgrade: downstream_upgrade_req={}, upstream_upgrade_req={:?}, downstream_was_upgraded={}, downstream_task_seen_upgraded={}, task_filter_seen_upgraded={}",
            status.downstream,
            status.upstream,
            session.was_upgraded(),
            session.downstream_task_seen_upgraded,
            task_filter_seen_upgraded
        ),
    )
    .map_err(|e| e.into_in())
}

fn reject_unexpected_upgraded_body_before_h1_upgrade<DS>(
    session: &Session<DS>,
    task_filter_seen_upgraded: bool,
) -> Result<()>
where
    DS: DownstreamSession,
{
    let status = session.h1_upgrade_request_snapshot();
    Error::e_explain(
        InvalidHTTPHeader,
        format!(
            "received upgraded body task before downstream 101 upgrade: downstream_upgrade_req={}, upstream_upgrade_req={:?}, downstream_was_upgraded={}, downstream_task_seen_upgraded={}, task_filter_seen_upgraded={}",
            status.downstream,
            status.upstream,
            session.was_upgraded(),
            session.downstream_task_seen_upgraded,
            task_filter_seen_upgraded
        ),
    )
    .map_err(|e| e.into_in())
}

impl<DS> AsRef<HttpSession<DS>> for Session<DS>
where
    DS: DownstreamSession,
{
    fn as_ref(&self) -> &HttpSession<DS> {
        &self.downstream_session
    }
}

impl<DS> AsMut<HttpSession<DS>> for Session<DS>
where
    DS: DownstreamSession,
{
    fn as_mut(&mut self) -> &mut HttpSession<DS> {
        &mut self.downstream_session
    }
}

use std::ops::{Deref, DerefMut};

impl<DS> Deref for Session<DS>
where
    DS: DownstreamSession,
{
    type Target = HttpSession<DS>;

    fn deref(&self) -> &Self::Target {
        &self.downstream_session
    }
}

impl<DS> DerefMut for Session<DS>
where
    DS: DownstreamSession,
{
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.downstream_session
    }
}

// generic HTTP 502 response sent when proxy_upstream_filter refuses to connect to upstream
static BAD_GATEWAY: Lazy<ResponseHeader> = Lazy::new(|| {
    let mut resp = ResponseHeader::build(http::StatusCode::BAD_GATEWAY, Some(3)).unwrap();
    resp.insert_header(header::SERVER, &SERVER_NAME[..])
        .unwrap();
    resp.insert_header(header::CONTENT_LENGTH, 0).unwrap();
    resp.insert_header(header::CACHE_CONTROL, "private, no-store")
        .unwrap();

    resp
});

impl<SV, C, DS> HttpProxy<SV, C, DS>
where
    C: custom::Connector,
    DS: DownstreamSession,
{
    async fn process_request(
        self: &Arc<Self>,
        mut session: Session<DS>,
        mut ctx: <SV as ProxyHttp<DS>>::CTX,
    ) -> Option<ReusedHttpStream>
    where
        SV: ProxyHttp<DS> + Send + Sync + 'static,
        <SV as ProxyHttp<DS>>::CTX: Send + Sync,
    {
        if let Err(e) = self
            .inner
            .early_request_filter(&mut session, &mut ctx)
            .await
        {
            return self
                .handle_error(session, &mut ctx, e, "Fail to early filter request:")
                .await;
        }

        // early body buffering: read full request body before request_filter
        // see https://github.com/cloudflare/pingora/issues/780
        #[cfg(feature = "early_body_buffer")]
        if !session.is_body_buffered() {
            if let Err(e) = self.buffer_request_body_early(&mut session, &mut ctx).await {
                return self
                    .handle_error(session, &mut ctx, e, "Failed to buffer request body:")
                    .await;
            }
        }

        if self.inner.allow_spawning_subrequest(&session, &ctx) {
            session.subrequest_spawner = Some(SubrequestSpawner::new(self.clone()));
        }

        let req = session.downstream_session.req_header_mut();

        // Built-in downstream request filters go first
        if let Err(e) = session
            .downstream_modules_ctx
            .request_header_filter(req)
            .await
        {
            return self
                .handle_error(
                    session,
                    &mut ctx,
                    e,
                    "Failed in downstream modules request filter:",
                )
                .await;
        }

        match self.inner.request_filter(&mut session, &mut ctx).await {
            Ok(response_sent) => {
                if response_sent {
                    // TODO: log error
                    self.inner.logging(&mut session, None, &mut ctx).await;
                    self.cleanup_sub_req(&mut session);
                    let mut persistent_settings = HttpPersistentSettings::for_session(&session);
                    if let Some(uc) = self.inner.persist_connection_context(&session, &ctx) {
                        persistent_settings.set_user_context(uc);
                    }
                    return session
                        .downstream_session
                        .finish()
                        .await
                        .ok()
                        .flatten()
                        .map(|s| ReusedHttpStream::from_reusable_stream(s, persistent_settings));
                }
                /* else continue */
            }
            Err(e) => {
                return self
                    .handle_error(session, &mut ctx, e, "Fail to filter request:")
                    .await;
            }
        }

        if let Some((reuse, err)) = self.proxy_cache(&mut session, &mut ctx).await {
            // cache hit
            return self.finish(session, &mut ctx, reuse, err).await;
        }
        // either uncacheable, or cache miss

        // there should not be a write lock in the sub req ctx after this point
        self.cleanup_sub_req(&mut session);

        // decide if the request is allowed to go to upstream
        match self
            .inner
            .proxy_upstream_filter(&mut session, &mut ctx)
            .await
        {
            Ok(proxy_to_upstream) => {
                if !proxy_to_upstream {
                    // The hook can choose to write its own response, but if it doesn't, we respond
                    // with a generic 502
                    if session.cache.enabled() {
                        // drop the cache lock that this request may be holding onto
                        session.cache.disable(NoCacheReason::DeclinedToUpstream);
                    }
                    if session.response_written().is_none() {
                        match session.write_response_header_ref(&BAD_GATEWAY, true).await {
                            Ok(()) => {}
                            Err(e) => {
                                return self
                                    .handle_error(
                                        session,
                                        &mut ctx,
                                        e,
                                        "Error responding with Bad Gateway:",
                                    )
                                    .await;
                            }
                        }
                    }

                    return self.finish(session, &mut ctx, true, None).await;
                }
                /* else continue */
            }
            Err(e) => {
                if session.cache.enabled() {
                    session.cache.disable(NoCacheReason::InternalError);
                }

                return self
                    .handle_error(
                        session,
                        &mut ctx,
                        e,
                        "Error deciding if we should proxy to upstream:",
                    )
                    .await;
            }
        }

        let mut retries: usize = 0;

        let mut server_reuse = false;
        let mut proxy_error: Option<Box<Error>> = None;

        while retries < self.max_retries {
            retries += 1;

            let (reuse, e) = self.proxy_to_upstream(&mut session, &mut ctx).await;
            server_reuse = reuse;

            match e {
                Some(error) => {
                    let retry = error.retry();
                    // only log error that will be retried here, the final error will be logged below
                    if retry
                        && !self.inner.suppress_proxy_warn_log(
                            &session,
                            &ctx,
                            &error,
                            ProxyWarnLogContext::UpstreamRetry,
                        )
                    {
                        warn!(
                            "Fail to proxy: {}, tries: {}, retry: {}, {}",
                            error,
                            retries,
                            retry,
                            self.inner.request_summary(&session, &ctx)
                        );
                    }
                    proxy_error = Some(error);
                    if !retry {
                        break;
                    }
                }
                None => {
                    proxy_error = None;
                    break;
                }
            };
        }

        // serve stale if error
        // Check both error and cache before calling the function because await is not cheap
        // allow unwrap until if let chains
        #[allow(clippy::unnecessary_unwrap)]
        let serve_stale_result = if proxy_error.is_some() && session.cache.can_serve_stale_error() {
            self.handle_stale_if_error(&mut session, &mut ctx, proxy_error.as_ref().unwrap())
                .await
        } else {
            None
        };

        let final_error = if let Some((reuse, stale_cache_error)) = serve_stale_result {
            // don't reuse server conn if serve stale polluted it
            server_reuse = server_reuse && reuse;
            stale_cache_error
        } else {
            proxy_error
        };

        if let Some(e) = final_error.as_ref() {
            // If we have errored and are still holding a cache lock, release it.
            if session.cache.enabled() {
                let reason = if *e.esource() == ErrorSource::Upstream {
                    NoCacheReason::UpstreamError
                } else {
                    NoCacheReason::InternalError
                };
                session.cache.disable(reason);
            }
            let res = self.inner.fail_to_proxy(&mut session, e, &mut ctx).await;

            // final error will have > 0 status unless downstream connection is dead
            if !self.inner.suppress_error_log(&session, &ctx, e) {
                error!(
                    "Fail to proxy: {}, status: {}, tries: {}, retry: {}, {}",
                    e,
                    res.error_code,
                    retries,
                    false, // we never retry here
                    self.inner.request_summary(&session, &ctx),
                );
            }
        }

        // logging() will be called in finish()
        self.finish(session, &mut ctx, server_reuse, final_error)
            .await
    }

    async fn handle_error(
        &self,
        mut session: Session<DS>,
        ctx: &mut <SV as ProxyHttp<DS>>::CTX,
        e: Box<Error>,
        context: &str,
    ) -> Option<ReusedHttpStream>
    where
        SV: ProxyHttp<DS> + Send + Sync + 'static,
        <SV as ProxyHttp<DS>>::CTX: Send + Sync,
    {
        let res = self.inner.fail_to_proxy(&mut session, &e, ctx).await;
        if !self.inner.suppress_error_log(&session, ctx, &e) {
            error!(
                "{context} {}, status: {}, {}",
                e,
                res.error_code,
                self.inner.request_summary(&session, ctx)
            );
        }
        self.inner.logging(&mut session, Some(&e), ctx).await;
        self.cleanup_sub_req(&mut session);

        session.downstream_session.on_proxy_failure(e);

        if res.can_reuse_downstream {
            let mut persistent_settings = HttpPersistentSettings::for_session(&session);
            if let Some(uc) = self.inner.persist_connection_context(&session, ctx) {
                persistent_settings.set_user_context(uc);
            }
            session
                .downstream_session
                .finish()
                .await
                .ok()
                .flatten()
                .map(|s| ReusedHttpStream::from_reusable_stream(s, persistent_settings))
        } else {
            None
        }
    }

    /// Buffer the entire request body before connecting to upstream.
    ///
    /// This enables early_request_body_filter to run BEFORE upstream_peer selection,
    /// allowing auth signature verification and content-based routing.
    ///
    /// Buffering is controlled by the trait method `early_request_body_buffer_limit()`:
    /// - Returns `None`: Skip buffering, stream body to upstream (default)
    /// - Returns `Some(max_size)`: Buffer body with size limit enforcement
    ///
    /// Size limit enforcement:
    /// - Content-Length checked first (fail fast before reading)
    /// - Accumulated size checked during reading (streaming protection)
    /// - Returns HTTP 413 (Payload Too Large) if exceeded
    #[cfg(feature = "early_body_buffer")]
    async fn buffer_request_body_early(
        &self,
        session: &mut Session<DS>,
        ctx: &mut <SV as ProxyHttp<DS>>::CTX,
    ) -> Result<()>
    where
        SV: ProxyHttp<DS> + Send + Sync,
        <SV as ProxyHttp<DS>>::CTX: Send + Sync,
    {
        // check trait method for opt-in and size limit
        let Some(max_size) = self.inner.early_request_body_buffer_limit(session, ctx) else {
            return Ok(());
        };

        // skip if already buffered
        if session.is_body_buffered() {
            return Ok(());
        }

        let total_timeout = self.inner.early_request_body_buffer_timeout(session, ctx);
        if let Some(total_timeout) = total_timeout {
            match pingora_timeout::timeout(
                total_timeout,
                self.buffer_request_body_early_inner(session, ctx, max_size),
            )
            .await
            {
                Ok(result) => result,
                Err(_) => Error::e_explain(
                    ReadTimedout,
                    format!("buffering request body, timeout: {total_timeout:?}"),
                )
                .map_err(|e| e.into_down()),
            }
        } else {
            self.buffer_request_body_early_inner(session, ctx, max_size)
                .await
        }
    }

    #[cfg(feature = "early_body_buffer")]
    async fn buffer_request_body_early_inner(
        &self,
        session: &mut Session<DS>,
        ctx: &mut <SV as ProxyHttp<DS>>::CTX,
        max_size: usize,
    ) -> Result<()>
    where
        SV: ProxyHttp<DS> + Send + Sync,
        <SV as ProxyHttp<DS>>::CTX: Send + Sync,
    {
        let content_length = header_value_content_length(
            session
                .downstream_session
                .req_header()
                .headers
                .get(header::CONTENT_LENGTH),
        );

        // fail fast: reject before reading if Content-Length exceeds limit
        if let Some(cl) = content_length {
            if cl > max_size {
                return Error::e_explain(
                    HTTPStatus(413),
                    format!(
                        "Request body too large: Content-Length {} exceeds limit {} bytes",
                        cl, max_size
                    ),
                );
            }
        }

        // Content-Length: 0 means no body; for all other cases (no Content-Length,
        // Transfer-Encoding, HTTP/2) attempt to read. read_request_body returns
        // None immediately if there's nothing.
        if content_length == Some(0) {
            session.mark_body_buffered();
            return Ok(());
        }

        if is_expect_continue_req(session.downstream_session.req_header()) {
            session
                .downstream_session
                .write_continue_response()
                .await
                .map_err(|e| e.into_down())?;
        }

        let mut body_parts: Vec<Bytes> = Vec::new();
        let mut total_size: usize = 0;

        // read body chunks until end of stream
        loop {
            let body_chunk: Option<Bytes> =
                match session.downstream_session.read_request_body().await {
                    Ok(chunk) => chunk,
                    Err(e) => return Err(e.into_down()),
                };

            // end of stream: None means no more data, or downstream reports done
            let end_of_body = body_chunk.is_none() || session.downstream_session.is_body_done();

            // run early body filter (not module filters, they haven't run header filter yet)
            let mut filter_data = body_chunk;
            self.inner
                .early_request_body_filter(session, &mut filter_data, end_of_body, ctx)
                .await?;

            // accumulate the (possibly filtered) data
            if let Some(filtered) = filter_data {
                total_size += filtered.len();

                // check size limit during accumulation
                if total_size > max_size {
                    return Error::e_explain(
                        HTTPStatus(413),
                        format!(
                            "Request body exceeded limit: {} > {} bytes",
                            total_size, max_size
                        ),
                    );
                }

                body_parts.push(filtered);
            }

            if end_of_body {
                break;
            }
        }

        if total_size == 0 {
            session.mark_body_buffered();
        } else if body_parts.len() == 1 {
            // common case: a single chunk can be moved out without a second copy
            session.set_buffered_body(body_parts.pop());
        } else {
            let mut combined = bytes::BytesMut::with_capacity(total_size);
            for part in body_parts {
                combined.extend_from_slice(&part);
            }
            session.set_buffered_body(Some(combined.freeze()));
        }

        Ok(())
    }
}

/* Make process_subrequest() a trait to workaround https://github.com/rust-lang/rust/issues/78649
   if process_subrequest() is implemented as a member of HttpProxy, rust complains

error[E0391]: cycle detected when computing type of `proxy_cache::<impl at pingora-proxy/src/proxy_cache.rs:7:1: 7:23>::proxy_cache::{opaque#0}`
   --> pingora-proxy/src/proxy_cache.rs:13:10
    |
13  |     ) -> Option<(bool, Option<Box<Error>>)>

*/
#[async_trait]
pub trait Subrequest<DS = ()>
where
    DS: DownstreamSession,
{
    async fn process_subrequest(
        self: Arc<Self>,
        session: Box<HttpSession<DS>>,
        sub_req_ctx: Box<SubrequestCtx>,
    );
}

#[async_trait]
impl<SV, C, DS> Subrequest<DS> for HttpProxy<SV, C, DS>
where
    SV: ProxyHttp<DS> + Send + Sync + 'static,
    <SV as ProxyHttp<DS>>::CTX: Send + Sync,
    C: custom::Connector,
    DS: DownstreamSession,
{
    async fn process_subrequest(
        self: Arc<Self>,
        session: Box<HttpSession<DS>>,
        sub_req_ctx: Box<SubrequestCtx>,
    ) {
        debug!("starting subrequest");

        let mut session = match self.handle_new_request(session).await {
            Some(downstream_session) => Session::new(
                downstream_session,
                &self.downstream_modules,
                #[cfg(feature = "upstream_modules")]
                &self.upstream_modules,
                self.shutdown_flag.clone(),
            ),
            None => return, // bad request
        };

        // no real downstream to keepalive, but it doesn't matter what is set here because at the end
        // of this fn the dummy connection will be dropped
        session.set_keepalive(None);

        session.subrequest_ctx.replace(sub_req_ctx);
        trace!("processing subrequest");
        let ctx = self.inner.new_ctx();
        self.process_request(session, ctx).await;
        trace!("subrequest done");
    }
}

/// A handle to the underlying HTTP proxy app that allows spawning subrequests.
pub struct SubrequestSpawner<DS = ()>
where
    DS: DownstreamSession,
{
    app: Arc<dyn Subrequest<DS> + Send + Sync>,
}

/// A [`PreparedSubrequest`] that is ready to run.
pub struct PreparedSubrequest<DS = ()>
where
    DS: DownstreamSession,
{
    app: Arc<dyn Subrequest<DS> + Send + Sync>,
    session: Box<HttpSession<DS>>,
    sub_req_ctx: Box<SubrequestCtx>,
}

impl<DS> PreparedSubrequest<DS>
where
    DS: DownstreamSession,
{
    pub async fn run(self) {
        self.app
            .process_subrequest(self.session, self.sub_req_ctx)
            .await
    }

    pub fn session(&self) -> &HttpSession<DS> {
        self.session.as_ref()
    }

    pub fn session_mut(&mut self) -> &mut HttpSession<DS> {
        self.session.deref_mut()
    }
}

impl<DS> SubrequestSpawner<DS>
where
    DS: DownstreamSession,
{
    /// Create a new [`SubrequestSpawner`].
    pub fn new(app: Arc<dyn Subrequest<DS> + Send + Sync>) -> SubrequestSpawner<DS> {
        SubrequestSpawner { app }
    }

    /// Spawn a background subrequest and return a join handle.
    // TODO: allow configuring the subrequest session before use
    pub fn spawn_background_subrequest(
        &self,
        session: &HttpSession<DS>,
        ctx: SubrequestCtx,
    ) -> tokio::task::JoinHandle<()> {
        let new_app = self.app.clone(); // Clone the Arc
        let (mut session, handle) = subrequest::create_session(session);
        if ctx.body_mode() == BodyMode::NoBody {
            session
                .as_subrequest_mut()
                .expect("created subrequest session")
                .clear_request_body_headers();
        }
        let sub_req_ctx = Box::new(ctx);
        handle.drain_tasks();
        tokio::spawn(async move {
            new_app
                .process_subrequest(Box::new(session), sub_req_ctx)
                .await;
        })
    }

    /// Create a subrequest that listens to `HttpTask`s sent from the returned `Sender`
    /// and sends `HttpTask`s to the returned `Receiver`.
    ///
    /// To run that subrequest, call `run()`.
    // TODO: allow configuring the subrequest session before use
    pub fn create_subrequest(
        &self,
        session: &HttpSession<DS>,
        ctx: SubrequestCtx,
    ) -> (PreparedSubrequest<DS>, SubrequestHandle) {
        let new_app = self.app.clone(); // Clone the Arc
        let (mut session, handle) = subrequest::create_session(session);
        if ctx.body_mode() == BodyMode::NoBody {
            session
                .as_subrequest_mut()
                .expect("created subrequest session")
                .clear_request_body_headers();
        }
        let sub_req_ctx = Box::new(ctx);
        (
            PreparedSubrequest {
                app: new_app,
                session: Box::new(session),
                sub_req_ctx,
            },
            handle,
        )
    }
}

#[async_trait]
impl<SV, C, DS> HttpServerApp<DS> for HttpProxy<SV, C, DS>
where
    SV: ProxyHttp<DS> + Send + Sync + 'static,
    <SV as ProxyHttp<DS>>::CTX: Send + Sync,
    C: custom::Connector,
    DS: DownstreamSession,
{
    async fn process_new_http(
        self: &Arc<Self>,
        mut session: HttpSession<DS>,
        shutdown: &ShutdownWatch,
    ) -> Option<ReusedHttpStream> {
        // Extract user context from the previous request before the session is moved into the Box
        let prev_user_ctx = session.take_connection_user_context();

        let session = Box::new(session);

        // TODO: keepalive pool, use stack
        let mut session = match self.handle_new_request(session).await {
            Some(downstream_session) => Session::new(
                downstream_session,
                &self.downstream_modules,
                #[cfg(feature = "upstream_modules")]
                &self.upstream_modules,
                self.shutdown_flag.clone(),
            ),
            None => return None, // bad request
        };

        if *shutdown.borrow() {
            // stop downstream from reusing if this service is shutting down soon
            session.set_keepalive(None);
        }

        let mut ctx = self.inner.new_ctx();

        // Deliver user context from the previous request on this reused connection
        if let Some(prev_ctx) = prev_user_ctx {
            self.inner
                .on_connection_reuse(&mut session, &mut ctx, prev_ctx);
        }

        self.process_request(session, ctx).await
    }

    async fn http_cleanup(&self) {
        self.shutdown_flag.store(true, Ordering::Release);
        // Notify all keepalived requests blocking on read_request() to abort
        self.shutdown.notify_waiters();
    }

    fn server_options(&self) -> Option<&HttpServerOptions> {
        self.server_options.as_ref()
    }

    fn h2_options(&self) -> Option<H2Options> {
        self.h2_options.clone()
    }
    async fn process_custom_session(
        self: Arc<Self>,
        stream: Stream,
        shutdown: &ShutdownWatch,
    ) -> Option<Stream> {
        let app = self.clone();

        let Some(process_custom_session) = app.process_custom_session.as_ref() else {
            warn!("custom was called on an empty on_custom");
            return None;
        };

        process_custom_session(self.clone(), stream, shutdown).await
    }

    // TODO implement h2_options
}

use pingora_core::services::listening::{RuntimeOptsOverride, Service};

/// Create an [`HttpProxy`] without wrapping it in a [`Service`].
///
/// This is useful when you need to integrate `HttpProxy` into a custom accept loop,
/// for example when implementing SNI-based routing that decides between TLS passthrough
/// and TLS termination on a single port.
///
/// The returned `HttpProxy` is fully initialized and ready to process requests via
/// [`HttpServerApp::process_new_http()`].
///
/// # Example
///
/// ```ignore
/// use pingora_proxy::http_proxy;
/// use std::sync::Arc;
///
/// // Create the proxy
/// let proxy = Arc::new(http_proxy(&server_conf, my_proxy_app));
///
/// // In your custom accept loop:
/// loop {
///     let (stream, addr) = listener.accept().await?;
///
///     // Peek SNI, decide routing...
///     if should_terminate_tls {
///         let tls_stream = my_acceptor.accept(stream).await?;
///         let session = HttpSession::new_http1(Box::new(tls_stream));
///         proxy.process_new_http(session, &shutdown).await;
///     }
/// }
/// ```
pub fn http_proxy<SV>(conf: &Arc<ServerConf>, inner: SV) -> HttpProxy<SV>
where
    SV: ProxyHttp,
{
    let mut proxy = HttpProxy::new(inner, conf.clone());
    proxy.handle_init_modules();
    proxy
}

/// Create a [Service] from the user implemented [ProxyHttp].
///
/// The returned [Service] can be hosted by a [pingora_core::server::Server] directly.
pub fn http_proxy_service<SV>(conf: &Arc<ServerConf>, inner: SV) -> Service<HttpProxy<SV, ()>>
where
    SV: ProxyHttp,
{
    http_proxy_service_with_name(conf, inner, "Pingora HTTP Proxy Service")
}

/// Create a [Service] from the user implemented [ProxyHttp].
///
/// The returned [Service] can be hosted by a [pingora_core::server::Server] directly.
pub fn http_proxy_service_with_name<SV>(
    conf: &Arc<ServerConf>,
    inner: SV,
    name: &str,
) -> Service<HttpProxy<SV, ()>>
where
    SV: ProxyHttp,
{
    let mut proxy = HttpProxy::new(inner, conf.clone());
    proxy.handle_init_modules();
    Service::new(name.to_string(), proxy)
}

/// Create a [`Service`] with a custom upstream connector and standard HTTP downstream sessions.
///
/// The returned [`Service`] can be hosted by a [`pingora_core::server::Server`] directly.
pub fn http_proxy_service_with_name_custom_connector<SV, C>(
    conf: &Arc<ServerConf>,
    inner: SV,
    name: &str,
    connector: C,
) -> Service<HttpProxy<SV, C>>
where
    SV: ProxyHttp + Send + Sync + 'static,
    SV::CTX: Send + Sync + 'static,
    C: custom::Connector,
{
    let mut proxy = HttpProxy::new_custom(inner, conf.clone(), connector, None, None, None);
    proxy.handle_init_modules();

    Service::new(name.to_string(), proxy)
}

/// Create a [Service] from the user implemented [ProxyHttp].
///
/// The returned [Service] can be hosted by a [pingora_core::server::Server] directly.
pub fn http_proxy_service_with_name_custom<SV, C, DS>(
    conf: &Arc<ServerConf>,
    inner: SV,
    name: &str,
    connector: C,
    on_custom: ProcessCustomSession<SV, C, DS>,
) -> Service<HttpProxy<SV, C, DS>, DS>
where
    SV: ProxyHttp<DS> + Send + Sync + 'static,
    <SV as ProxyHttp<DS>>::CTX: Send + Sync + 'static,
    C: custom::Connector,
    DS: DownstreamSession,
{
    let mut proxy =
        HttpProxy::new_custom(inner, conf.clone(), connector, Some(on_custom), None, None);
    proxy.handle_init_modules();

    Service::<_, DS>::new_with_custom_session(name.to_string(), proxy)
}

/// A builder for a [Service] that can be used to create a [HttpProxy] instance
///
/// The [ProxyServiceBuilder] can be used to construct a [HttpProxy] service with a custom name,
/// connector, and custom session handler.
///
pub struct ProxyServiceBuilder<SV, C, DS = ()>
where
    C: custom::Connector,
    DS: DownstreamSession,
{
    conf: Arc<ServerConf>,
    inner: SV,
    name: String,
    connector: C,
    custom: Option<ProcessCustomSession<SV, C, DS>>,
    server_options: Option<HttpServerOptions>,
    client_options: Option<ConnectorOptions>,
    runtime_opts_override: Option<RuntimeOptsOverride>,
}

impl<SV> ProxyServiceBuilder<SV, (), ()> {
    /// Create a new [ProxyServiceBuilder] with the given [ServerConf] and [ProxyHttp]
    /// implementation.
    ///
    /// The returned builder can be used to construct a [HttpProxy] service with a custom name,
    /// connector, and custom session handler.
    ///
    /// The [ProxyServiceBuilder] will default to using the [ProxyHttp] implementation and no custom
    /// session handler.
    ///
    pub fn new(conf: &Arc<ServerConf>, inner: SV) -> Self {
        ProxyServiceBuilder {
            conf: conf.clone(),
            inner,
            name: "Pingora HTTP Proxy Service".into(),
            connector: (),
            custom: None,
            server_options: None,
            client_options: None,
            runtime_opts_override: None,
        }
    }
}

impl<SV, C, DS> ProxyServiceBuilder<SV, C, DS>
where
    C: custom::Connector,
    DS: DownstreamSession,
{
    /// Sets the name of the [HttpProxy] service.
    pub fn name(mut self, name: impl AsRef<str>) -> Self {
        self.name = name.as_ref().to_owned();
        self
    }

    /// Set a custom connector and custom session handler for the [ProxyServiceBuilder].
    ///
    /// The custom connector is used to establish a connection to the upstream server.
    ///
    /// The custom session handler is used to handle custom protocol specific logic
    /// between the proxy and the upstream server.
    ///
    /// Returns a new [ProxyServiceBuilder] with the custom connector and session handler.
    pub fn custom<C2, DS2>(
        self,
        connector: C2,
        on_custom: ProcessCustomSession<SV, C2, DS2>,
    ) -> ProxyServiceBuilder<SV, C2, DS2>
    where
        C2: custom::Connector,
        DS2: DownstreamSession,
    {
        let Self {
            conf,
            inner,
            name,
            server_options,
            client_options,
            runtime_opts_override,
            ..
        } = self;
        ProxyServiceBuilder {
            conf,
            inner,
            name,
            connector,
            custom: Some(on_custom),
            server_options,
            client_options,
            runtime_opts_override,
        }
    }

    /// Set the upstream client connector options for the [ProxyServiceBuilder].
    ///
    /// Returns a new [ProxyServiceBuilder] with the upstream client connector options set.
    pub fn client_options(mut self, options: ConnectorOptions) -> Self {
        self.client_options = Some(options);
        self
    }

    /// Set the server options for the [ProxyServiceBuilder].
    ///
    /// Returns a new [ProxyServiceBuilder] with the server options set.
    pub fn server_options(mut self, options: HttpServerOptions) -> Self {
        self.server_options = Some(options);
        self
    }

    /// Set a runtime options override for the [Service] built by this builder.
    ///
    /// Returning [`None`] from the override uses the global runtime options.
    pub fn runtime_opts_override<F>(mut self, override_fn: F) -> Self
    where
        F: Fn(&RuntimeOpts) -> Option<RuntimeOpts> + Send + Sync + 'static,
    {
        self.runtime_opts_override = Some(Arc::new(override_fn));
        self
    }

    /// Builds a new [Service] from the [ProxyServiceBuilder].
    ///
    /// This function takes ownership of the [ProxyServiceBuilder] and returns a new [Service] with
    /// a fully initialized [HttpProxy].
    ///
    /// The returned [Service] is ready to be used by a [pingora_core::server::Server].
    pub fn build(self) -> Service<HttpProxy<SV, C, DS>, DS>
    where
        SV: ProxyHttp<DS> + Send + Sync + 'static,
        <SV as ProxyHttp<DS>>::CTX: Send + Sync + 'static,
    {
        let Self {
            conf,
            inner,
            name,
            connector,
            custom,
            server_options,
            client_options,
            runtime_opts_override,
        } = self;

        let mut proxy = HttpProxy::new_custom(
            inner,
            conf,
            connector,
            custom,
            server_options,
            client_options,
        );

        proxy.handle_init_modules();
        let mut service = Service::<_, DS>::new_with_custom_session(name, proxy);
        if let Some(runtime_opts_override) = runtime_opts_override {
            service.set_runtime_opts_override(runtime_opts_override);
        }
        service
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pingora_core::modules::http::{HttpModule, HttpModuleBuilder};
    use pingora_core::protocols::l4::stream::Stream as L4Stream;
    use pingora_core::protocols::l4::virt::{VirtualSockOpt, VirtualSocket, VirtualSocketStream};
    use pingora_error::RetryType;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Mutex;
    use std::task::{Context, Poll};
    use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

    #[derive(Debug)]
    struct StaticVirtualSocket {
        read_buf: Vec<u8>,
        read_pos: usize,
        write_buf: Arc<Mutex<Vec<u8>>>,
    }

    impl StaticVirtualSocket {
        fn new(read_buf: &[u8], write_buf: Arc<Mutex<Vec<u8>>>) -> Self {
            Self {
                read_buf: read_buf.to_vec(),
                read_pos: 0,
                write_buf,
            }
        }
    }

    impl AsyncRead for StaticVirtualSocket {
        fn poll_read(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<std::io::Result<()>> {
            let remaining = self.read_buf.len() - self.read_pos;
            let to_read = remaining.min(buf.remaining());
            if to_read > 0 {
                buf.put_slice(&self.read_buf[self.read_pos..self.read_pos + to_read]);
                self.read_pos += to_read;
            }
            Poll::Ready(Ok(()))
        }
    }

    impl AsyncWrite for StaticVirtualSocket {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            self.write_buf.lock().unwrap().extend_from_slice(buf);
            Poll::Ready(Ok(buf.len()))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    impl VirtualSocket for StaticVirtualSocket {
        fn set_socket_option(&self, _opt: VirtualSockOpt) -> std::io::Result<()> {
            Ok(())
        }
    }

    async fn new_request_session(request: &[u8], written: Arc<Mutex<Vec<u8>>>) -> Session {
        let socket = StaticVirtualSocket::new(request, written);
        let stream = L4Stream::from(VirtualSocketStream::new(Box::new(socket)));
        let mut session = Session::new_h1(Box::new(stream));
        session.read_request().await.unwrap();
        session
    }

    async fn new_upgrade_request_session(written: Arc<Mutex<Vec<u8>>>) -> Session {
        new_request_session(
            b"GET / HTTP/1.1\r\nHost: example.com\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\n",
            written,
        )
        .await
    }

    struct DefaultRetryProxy;

    #[async_trait]
    impl ProxyHttp for DefaultRetryProxy {
        type CTX = ();

        fn new_ctx(&self) -> Self::CTX {}

        async fn upstream_peer(
            &self,
            _session: &mut Session,
            _ctx: &mut Self::CTX,
        ) -> Result<Box<HttpPeer>> {
            unreachable!()
        }
    }

    fn default_policy_would_retry_for_session(
        session: &mut Session,
        retry: RetryType,
        client_reused: bool,
    ) -> bool {
        let mut error = Error::new_up(ReadError);
        error.retry = retry;

        DefaultRetryProxy
            .error_while_proxy(
                &HttpPeer::new("127.0.0.1:80", false, "".to_string()),
                session,
                error,
                &mut (),
                client_reused,
            )
            .retry()
    }

    async fn default_policy_would_retry(
        request: &[u8],
        retry: RetryType,
        client_reused: bool,
    ) -> bool {
        let mut session = new_request_session(request, Arc::new(Mutex::new(Vec::new()))).await;
        default_policy_would_retry_for_session(&mut session, retry, client_reused)
    }

    async fn buffered_put_session(body_len: usize) -> Session {
        let mut request =
            format!("PUT / HTTP/1.1\r\nHost: example.com\r\nContent-Length: {body_len}\r\n\r\n")
                .into_bytes();
        request.resize(request.len() + body_len, b'a');

        let mut session = new_request_session(&request, Arc::new(Mutex::new(Vec::new()))).await;
        session.enable_retry_buffering();
        while session.read_request_body().await.unwrap().is_some() {}
        session
    }

    #[tokio::test]
    async fn default_retry_policy_requires_an_idempotent_method() {
        let decided_retry = RetryType::Decided(true);
        assert!(
            default_policy_would_retry(
                b"GET / HTTP/1.1\r\nHost: example.com\r\n\r\n",
                decided_retry,
                false,
            )
            .await
        );
        assert!(
            default_policy_would_retry(
                b"PUT / HTTP/1.1\r\nHost: example.com\r\nContent-Length: 0\r\n\r\n",
                decided_retry,
                false,
            )
            .await
        );
        assert!(
            !default_policy_would_retry(
                b"POST / HTTP/1.1\r\nHost: example.com\r\nContent-Length: 0\r\n\r\n",
                decided_retry,
                false,
            )
            .await
        );
        assert!(
            !default_policy_would_retry(
                b"PATCH / HTTP/1.1\r\nHost: example.com\r\nContent-Length: 0\r\n\r\n",
                decided_retry,
                false,
            )
            .await
        );
    }

    #[tokio::test]
    async fn default_retry_policy_resolves_reused_only() {
        let request = b"GET / HTTP/1.1\r\nHost: example.com\r\n\r\n";

        assert!(default_policy_would_retry(request, RetryType::ReusedOnly, true).await);
        assert!(!default_policy_would_retry(request, RetryType::ReusedOnly, false).await);
    }

    #[tokio::test]
    async fn default_retry_policy_requires_an_untruncated_body_buffer() {
        let mut complete = buffered_put_session(64 * 1024).await;
        assert!(!complete.retry_buffer_truncated());
        assert!(default_policy_would_retry_for_session(
            &mut complete,
            RetryType::Decided(true),
            false,
        ));

        let mut truncated = buffered_put_session(64 * 1024 + 1).await;
        assert!(truncated.retry_buffer_truncated());
        assert!(!default_policy_would_retry_for_session(
            &mut truncated,
            RetryType::Decided(true),
            false,
        ));
        assert!(!default_policy_would_retry_for_session(
            &mut truncated,
            RetryType::ReusedOnly,
            true,
        ));
    }

    fn upgrade_response_header() -> ResponseHeader {
        let mut header =
            ResponseHeader::build(http::StatusCode::SWITCHING_PROTOCOLS, Some(2)).unwrap();
        header
            .insert_header(http::header::UPGRADE, "websocket")
            .unwrap();
        header
            .insert_header(http::header::CONNECTION, "Upgrade")
            .unwrap();
        header
    }

    struct SwitchTo101Module;

    #[async_trait]
    impl HttpModule for SwitchTo101Module {
        async fn response_header_filter(
            &mut self,
            resp: &mut ResponseHeader,
            _end_of_stream: bool,
        ) -> Result<()> {
            resp.set_status(http::StatusCode::SWITCHING_PROTOCOLS)?;
            resp.set_version(Version::HTTP_11);
            Ok(())
        }

        fn as_any(&self) -> &dyn std::any::Any {
            self
        }

        fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
            self
        }
    }

    struct SwitchTo101ModuleBuilder;

    impl HttpModuleBuilder for SwitchTo101ModuleBuilder {
        fn init(&self) -> pingora_core::modules::http::Module {
            Box::new(SwitchTo101Module)
        }
    }

    struct DoneBytesModule {
        called: Arc<AtomicBool>,
    }

    #[async_trait]
    impl HttpModule for DoneBytesModule {
        fn response_done_filter(&mut self) -> Result<Option<Bytes>> {
            self.called.store(true, Ordering::Release);
            Ok(Some(Bytes::from_static(b"hello")))
        }

        fn as_any(&self) -> &dyn std::any::Any {
            self
        }

        fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
            self
        }
    }

    struct DoneBytesModuleBuilder {
        called: Arc<AtomicBool>,
    }

    impl HttpModuleBuilder for DoneBytesModuleBuilder {
        fn init(&self) -> pingora_core::modules::http::Module {
            Box::new(DoneBytesModule {
                called: self.called.clone(),
            })
        }
    }

    struct DoneEmptyModule {
        called: Arc<AtomicBool>,
    }

    impl HttpModule for DoneEmptyModule {
        fn response_done_filter(&mut self) -> Result<Option<Bytes>> {
            self.called.store(true, Ordering::Release);
            Ok(None)
        }

        fn as_any(&self) -> &dyn std::any::Any {
            self
        }

        fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
            self
        }
    }

    struct DoneEmptyModuleBuilder {
        called: Arc<AtomicBool>,
    }

    impl HttpModuleBuilder for DoneEmptyModuleBuilder {
        fn init(&self) -> pingora_core::modules::http::Module {
            Box::new(DoneEmptyModule {
                called: self.called.clone(),
            })
        }
    }

    fn assert_raw_upgrade_payload(written: &[u8]) {
        assert!(
            written.starts_with(b"HTTP/1.1 101 Switching Protocols\r\n"),
            "unexpected response: {:?}",
            String::from_utf8_lossy(written)
        );
        assert!(
            written.ends_with(b"\r\n\r\nhello"),
            "upgrade payload should be written as raw tunneled bytes: {:?}",
            String::from_utf8_lossy(written)
        );
        assert!(
            !written
                .windows(b"\r\n5\r\nhello".len())
                .any(|w| w == b"\r\n5\r\nhello"),
            "upgrade payload must not be chunk framed: {:?}",
            String::from_utf8_lossy(written)
        );
    }

    #[tokio::test]
    async fn write_response_tasks_rejects_body_after_101() {
        let written = Arc::new(Mutex::new(Vec::new()));
        let mut session = new_upgrade_request_session(written.clone()).await;

        let err = session
            .write_response_tasks(vec![
                HttpTask::Header(Box::new(upgrade_response_header()), false),
                HttpTask::Body(Some(Bytes::from_static(b"hello")), true),
            ])
            .await
            .unwrap_err();

        assert_eq!(err.etype(), &InvalidHTTPHeader);
        assert_eq!(err.esource(), &ErrorSource::Internal);
        assert!(written.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn write_response_tasks_allows_upgraded_body_after_101() {
        let written = Arc::new(Mutex::new(Vec::new()));
        let mut session = new_upgrade_request_session(written.clone()).await;

        let response_done = session
            .write_response_tasks(vec![
                HttpTask::Header(Box::new(upgrade_response_header()), false),
                HttpTask::UpgradedBody(Some(Bytes::from_static(b"hello")), true),
            ])
            .await
            .unwrap();

        assert!(response_done);
        let written = written.lock().unwrap().clone();
        assert_raw_upgrade_payload(&written);
    }

    #[tokio::test]
    async fn write_response_tasks_rejects_upgraded_body_before_101() {
        let written = Arc::new(Mutex::new(Vec::new()));
        let mut session = new_upgrade_request_session(written.clone()).await;
        session.set_upstream_h1_upgrade_request_status(true);

        let err = session
            .write_response_tasks(vec![HttpTask::UpgradedBody(
                Some(Bytes::from_static(b"hello")),
                true,
            )])
            .await
            .unwrap_err();

        assert_eq!(err.etype(), &InvalidHTTPHeader);
        assert_eq!(err.esource(), &ErrorSource::Internal);
        assert!(written.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn write_response_tasks_rejects_trailer_after_101() {
        let written = Arc::new(Mutex::new(Vec::new()));
        let mut session = new_upgrade_request_session(written.clone()).await;

        let err = session
            .write_response_tasks(vec![
                HttpTask::Header(Box::new(upgrade_response_header()), false),
                HttpTask::Trailer(Some(Box::new(http::HeaderMap::new()))),
            ])
            .await
            .unwrap_err();

        assert_eq!(err.etype(), &InvalidHTTPHeader);
        assert_eq!(err.esource(), &ErrorSource::Internal);
        assert!(written.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn write_response_tasks_runs_done_filter_after_101_as_upgraded_body() {
        let written = Arc::new(Mutex::new(Vec::new()));
        let called = Arc::new(AtomicBool::new(false));
        let socket = StaticVirtualSocket::new(
            b"GET / HTTP/1.1\r\nHost: example.com\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\n",
            written.clone(),
        );
        let stream = L4Stream::from(VirtualSocketStream::new(Box::new(socket)));
        let mut modules = HttpModules::new();
        modules.add_module(Box::new(DoneBytesModuleBuilder {
            called: called.clone(),
        }));

        let mut session = Session::new_h1_with_modules(Box::new(stream), &modules);
        session.read_request().await.unwrap();

        let response_done = session
            .write_response_tasks(vec![
                HttpTask::Header(Box::new(upgrade_response_header()), false),
                HttpTask::Done,
            ])
            .await
            .unwrap();

        assert!(response_done);
        assert!(called.load(Ordering::Acquire));
        let written = written.lock().unwrap().clone();
        assert_raw_upgrade_payload(&written);
    }

    #[tokio::test]
    async fn write_response_tasks_allows_empty_done_after_101() {
        let written = Arc::new(Mutex::new(Vec::new()));
        let called = Arc::new(AtomicBool::new(false));
        let socket = StaticVirtualSocket::new(
            b"GET / HTTP/1.1\r\nHost: example.com\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\n",
            written.clone(),
        );
        let stream = L4Stream::from(VirtualSocketStream::new(Box::new(socket)));
        let mut modules = HttpModules::new();
        modules.add_module(Box::new(DoneEmptyModuleBuilder {
            called: called.clone(),
        }));

        let mut session = Session::new_h1_with_modules(Box::new(stream), &modules);
        session.read_request().await.unwrap();

        let response_done = session
            .write_response_tasks(vec![
                HttpTask::Header(Box::new(upgrade_response_header()), false),
                HttpTask::Done,
            ])
            .await
            .unwrap();

        assert!(response_done);
        assert!(called.load(Ordering::Acquire));
        let written = written.lock().unwrap().clone();
        assert!(
            written.starts_with(b"HTTP/1.1 101 Switching Protocols\r\n"),
            "unexpected response: {:?}",
            String::from_utf8_lossy(&written)
        );
        assert!(
            written.ends_with(b"\r\n\r\n"),
            "empty Done filter should only finish the upgraded response: {:?}",
            String::from_utf8_lossy(&written)
        );
    }

    #[tokio::test]
    async fn write_response_tasks_rejects_module_created_101_with_upgrade_mismatch() {
        let written = Arc::new(Mutex::new(Vec::new()));
        let socket = StaticVirtualSocket::new(
            b"GET / HTTP/1.1\r\nHost: example.com\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\n",
            written.clone(),
        );
        let stream = L4Stream::from(VirtualSocketStream::new(Box::new(socket)));
        let mut modules = HttpModules::new();
        modules.add_module(Box::new(SwitchTo101ModuleBuilder));

        let mut session = Session::new_h1_with_modules(Box::new(stream), &modules);
        session.read_request().await.unwrap();
        session.h1_upgrade_request_status = H1UpgradeRequestStatus {
            upstream: Some(false),
        };

        let err = session
            .write_response_tasks(vec![
                HttpTask::Header(
                    Box::new(ResponseHeader::build(200, Some(0)).unwrap()),
                    false,
                ),
                HttpTask::Body(Some(Bytes::from_static(b"hello")), true),
            ])
            .await
            .unwrap_err();

        assert_eq!(err.etype(), &InvalidHTTPHeader);
        assert_eq!(err.esource(), &ErrorSource::Internal);
        assert!(written.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn write_response_tasks_rejects_module_created_101_before_body() {
        let written = Arc::new(Mutex::new(Vec::new()));
        let socket = StaticVirtualSocket::new(
            b"GET / HTTP/1.1\r\nHost: example.com\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\n",
            written.clone(),
        );
        let stream = L4Stream::from(VirtualSocketStream::new(Box::new(socket)));
        let mut modules = HttpModules::new();
        modules.add_module(Box::new(SwitchTo101ModuleBuilder));

        let mut session = Session::new_h1_with_modules(Box::new(stream), &modules);
        session.read_request().await.unwrap();

        let err = session
            .write_response_tasks(vec![
                HttpTask::Header(
                    Box::new(ResponseHeader::build(200, Some(0)).unwrap()),
                    false,
                ),
                HttpTask::Body(Some(Bytes::from_static(b"hello")), true),
            ])
            .await
            .unwrap_err();

        assert_eq!(err.etype(), &InvalidHTTPHeader);
        assert_eq!(err.esource(), &ErrorSource::Internal);
        assert!(written.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn send_downstream_proxy_task_rejects_body_after_101() {
        let written = Arc::new(Mutex::new(Vec::new()));
        let mut session = new_upgrade_request_session(written.clone()).await;
        session.set_proxy_tasks_enabled(true);

        session
            .send_downstream_proxy_task(HttpTask::Header(
                Box::new(upgrade_response_header()),
                false,
            ))
            .await
            .unwrap();
        let err = session
            .send_downstream_proxy_task(HttpTask::Body(Some(Bytes::from_static(b"hello")), true))
            .await
            .unwrap_err();

        assert_eq!(err.etype(), &InvalidHTTPHeader);
        assert_eq!(err.esource(), &ErrorSource::Internal);
        assert!(written.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn send_downstream_proxy_task_allows_upgraded_body_after_101() {
        let written = Arc::new(Mutex::new(Vec::new()));
        let mut session = new_upgrade_request_session(written.clone()).await;
        session.set_proxy_tasks_enabled(true);

        session
            .send_downstream_proxy_task(HttpTask::Header(
                Box::new(upgrade_response_header()),
                false,
            ))
            .await
            .unwrap();
        session
            .send_downstream_proxy_task(HttpTask::UpgradedBody(
                Some(Bytes::from_static(b"hello")),
                true,
            ))
            .await
            .unwrap();

        let response_done = session.write_downstream_proxy_tasks().await.unwrap();

        assert!(response_done);
        let written = written.lock().unwrap().clone();
        assert_raw_upgrade_payload(&written);
    }

    #[tokio::test]
    async fn send_downstream_proxy_task_rejects_upgraded_body_before_101() {
        let written = Arc::new(Mutex::new(Vec::new()));
        let mut session = new_upgrade_request_session(written.clone()).await;
        session.set_upstream_h1_upgrade_request_status(true);
        session.set_proxy_tasks_enabled(true);

        let err = session
            .send_downstream_proxy_task(HttpTask::UpgradedBody(
                Some(Bytes::from_static(b"hello")),
                true,
            ))
            .await
            .unwrap_err();

        assert_eq!(err.etype(), &InvalidHTTPHeader);
        assert_eq!(err.esource(), &ErrorSource::Internal);
        assert!(!session.has_pending_downstream_tasks());
        assert!(written.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn send_downstream_proxy_task_runs_done_filter_after_101_as_upgraded_body() {
        let written = Arc::new(Mutex::new(Vec::new()));
        let called = Arc::new(AtomicBool::new(false));
        let socket = StaticVirtualSocket::new(
            b"GET / HTTP/1.1\r\nHost: example.com\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\n",
            written.clone(),
        );
        let stream = L4Stream::from(VirtualSocketStream::new(Box::new(socket)));
        let mut modules = HttpModules::new();
        modules.add_module(Box::new(DoneBytesModuleBuilder {
            called: called.clone(),
        }));

        let mut session = Session::new_h1_with_modules(Box::new(stream), &modules);
        session.read_request().await.unwrap();
        session.set_proxy_tasks_enabled(true);

        session
            .send_downstream_proxy_task(HttpTask::Header(
                Box::new(upgrade_response_header()),
                false,
            ))
            .await
            .unwrap();
        session
            .send_downstream_proxy_task(HttpTask::Done)
            .await
            .unwrap();

        let response_done = session.write_downstream_proxy_tasks().await.unwrap();

        assert!(response_done);
        assert!(called.load(Ordering::Acquire));
        let written = written.lock().unwrap().clone();
        assert_raw_upgrade_payload(&written);
    }

    #[cfg(feature = "early_body_buffer")]
    mod body_buffer {
        use super::*;
        use pingora_core::protocols::http::v1::server::HttpSession;
        use tokio_test::io::Builder;

        fn create_test_session() -> Session {
            let mock_io = Builder::new().build();
            let http_session = HttpSession::new(Box::new(mock_io));
            Session::new_h1_with_http_session(http_session)
        }

        #[test]
        fn test_initial_state() {
            let session = create_test_session();
            assert!(!session.is_body_buffered());
            assert!(session.get_buffered_body().is_none());
        }

        #[test]
        fn test_set_and_get_buffered_body() {
            let mut session = create_test_session();
            let body = Bytes::from("test body");

            session.set_buffered_body(Some(body.clone()));

            assert!(session.is_body_buffered());
            assert_eq!(session.get_buffered_body(), Some(&body));
        }

        #[test]
        fn test_buffered_body_survives_repeated_reads() {
            let mut session = create_test_session();
            let body = Bytes::from("test body");

            session.set_buffered_body(Some(body.clone()));

            for _ in 0..3 {
                assert_eq!(session.get_buffered_body(), Some(&body));
                assert!(session.is_body_buffered());
            }
        }

        #[test]
        fn test_mark_body_buffered() {
            let mut session = create_test_session();

            assert!(!session.is_body_buffered());
            session.mark_body_buffered();
            assert!(session.is_body_buffered());
            assert!(session.get_buffered_body().is_none());
        }

        #[test]
        fn test_set_none_marks_empty_body_buffered() {
            let mut session = create_test_session();

            assert!(!session.is_body_buffered());
            session.set_buffered_body(None);
            assert!(session.is_body_buffered());
            assert!(session.get_buffered_body().is_none());
        }
    }

    /// A socket whose reads never complete, like an idle keep-alive connection
    /// waiting for its next request.
    #[derive(Debug)]
    struct PendingVirtualSocket;

    impl AsyncRead for PendingVirtualSocket {
        fn poll_read(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _buf: &mut ReadBuf<'_>,
        ) -> Poll<std::io::Result<()>> {
            Poll::Pending
        }
    }

    impl AsyncWrite for PendingVirtualSocket {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            Poll::Ready(Ok(buf.len()))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    impl VirtualSocket for PendingVirtualSocket {
        fn set_socket_option(&self, _opt: VirtualSockOpt) -> std::io::Result<()> {
            Ok(())
        }
    }

    struct NoopProxy;

    #[async_trait]
    impl ProxyHttp for NoopProxy {
        type CTX = ();
        fn new_ctx(&self) -> Self::CTX {}
        async fn upstream_peer(
            &self,
            _session: &mut Session,
            _ctx: &mut Self::CTX,
        ) -> Result<Box<HttpPeer>> {
            Err(Error::new(InternalError))
        }
    }

    fn pending_session() -> Box<HttpSession> {
        let stream = L4Stream::from(VirtualSocketStream::new(Box::new(PendingVirtualSocket)));
        Box::new(HttpSession::new_http1(Box::new(stream)))
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn shutdown_wakes_parked_read_requests() {
        let conf = ServerConf {
            threads: 4,
            ..ServerConf::default()
        };
        let proxy = Arc::new(HttpProxy::new(NoopProxy, Arc::new(conf)));
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let proxy = proxy.clone();
                tokio::spawn(async move { proxy.handle_new_request(pending_session()).await })
            })
            .collect();
        // let the tasks park in read_request()
        time::sleep(Duration::from_millis(50)).await;
        proxy.http_cleanup().await;
        for handle in handles {
            let session = time::timeout(Duration::from_secs(5), handle)
                .await
                .expect("shutdown did not wake the parked read")
                .unwrap();
            assert!(session.is_none());
        }
    }

    #[tokio::test]
    async fn shutdown_before_read_request_parks_returns_immediately() {
        let proxy = Arc::new(HttpProxy::new(NoopProxy, Arc::new(ServerConf::default())));
        proxy.http_cleanup().await;
        // a request that parks after notify_waiters() already fired must not
        // wait for a notification that will never come
        let session = time::timeout(
            Duration::from_secs(5),
            proxy.handle_new_request(pending_session()),
        )
        .await
        .expect("read_request parked after shutdown");
        assert!(session.is_none());
    }
}
