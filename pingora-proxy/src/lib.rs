// Copyright 2024 Cloudflare, Inc.
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
use futures::future::FutureExt;
use http::{header, version::Version};
use log::{debug, error, trace, warn};
use once_cell::sync::Lazy;
use pingora_http::{RequestHeader, ResponseHeader};
use std::fmt::Debug;
use std::str;
use std::sync::Arc;
use tokio::sync::{mpsc, Notify};
use tokio::time;

use pingora_cache::NoCacheReason;
use pingora_core::apps::{HttpServerApp, HttpServerOptions};
use pingora_core::connectors::{http::Connector, ConnectorOptions};
use pingora_core::modules::http::compression::ResponseCompressionBuilder;
use pingora_core::modules::http::{HttpModuleCtx, HttpModules};
use pingora_core::protocols::http::client::HttpSession as ClientSession;
use pingora_core::protocols::http::v1::client::HttpSession as HttpSessionV1;
use pingora_core::protocols::http::HttpTask;
use pingora_core::protocols::http::ServerSession as HttpSession;
use pingora_core::protocols::http::SERVER_NAME;
use pingora_core::protocols::Stream;
use pingora_core::protocols::{Digest, UniqueID};
use pingora_core::server::configuration::ServerConf;
use pingora_core::server::ShutdownWatch;
use pingora_core::upstreams::peer::{HttpPeer, Peer};
use pingora_error::{Error, ErrorSource, ErrorType::*, OrErr, Result};

const MAX_RETRIES: usize = 16;
const TASK_BUFFER_SIZE: usize = 4;

mod proxy_cache;
mod proxy_common;
mod proxy_h1;
mod proxy_h2;
mod proxy_purge;
mod proxy_trait;
mod subrequest;

use subrequest::Ctx as SubReqCtx;

pub use proxy_purge::PurgeStatus;
pub use proxy_trait::ProxyHttp;

pub mod prelude {
    pub use crate::{http_proxy_service, ProxyHttp, Session};
}

/// The concrete type that holds the user defined HTTP proxy.
///
/// Users don't need to interact with this object directly.
pub struct HttpProxy<SV> {
    inner: SV, // TODO: name it better than inner
    client_upstream: Connector,
    shutdown: Notify,
    pub server_options: Option<HttpServerOptions>,
    pub downstream_modules: HttpModules,
}

impl<SV> HttpProxy<SV> {
    fn new(inner: SV, conf: Arc<ServerConf>) -> Self {
        HttpProxy {
            inner,
            client_upstream: Connector::new(Some(ConnectorOptions::from_server_conf(&conf))),
            shutdown: Notify::new(),
            server_options: None,
            downstream_modules: HttpModules::new(),
        }
    }

    fn handle_init_modules(&mut self)
    where
        SV: ProxyHttp,
    {
        self.inner
            .init_downstream_modules(&mut self.downstream_modules);
    }

    async fn handle_new_request(
        &self,
        mut downstream_session: Box<HttpSession>,
    ) -> Option<Box<HttpSession>>
    where
        SV: ProxyHttp + Send + Sync,
        SV::CTX: Send + Sync,
    {
        // phase 1 read request header

        let res = tokio::select! {
            biased; // biased select is cheaper, and we don't want to drop already buffered requests
            res = downstream_session.read_request() => { res }
            _ = self.shutdown.notified() => {
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
                error!("Fail to proxy: {}", e);
                if matches!(e.etype, InvalidHTTPHeader) {
                    downstream_session.respond_error(400).await;
                } // otherwise the connection must be broken, no need to send anything
                downstream_session.shutdown().await;
                return None;
            }
        }
        trace!(
            "Request header: {:?}",
            downstream_session.req_header().as_ref()
        );
        Some(downstream_session)
    }

    // return bool: server_session can be reused, and error if any
    async fn proxy_to_upstream(
        &self,
        session: &mut Session,
        ctx: &mut SV::CTX,
    ) -> (bool, Option<Box<Error>>)
    where
        SV: ProxyHttp + Send + Sync,
        SV::CTX: Send + Sync,
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
                                    .map_or(true, |alpn| alpn.get_min_http_version() == 1)
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

    fn upstream_filter(
        &self,
        session: &mut Session,
        task: &mut HttpTask,
        ctx: &mut SV::CTX,
    ) -> Result<()>
    where
        SV: ProxyHttp,
    {
        match task {
            HttpTask::Header(header, _eos) => {
                self.inner.upstream_response_filter(session, header, ctx)
            }
            HttpTask::Body(data, eos) => self
                .inner
                .upstream_response_body_filter(session, data, *eos, ctx),
            HttpTask::Trailer(Some(trailers)) => self
                .inner
                .upstream_response_trailer_filter(session, trailers, ctx)?,
            _ => {
                // task does not support a filter
            }
        }
        Ok(())
    }

    async fn finish(
        &self,
        mut session: Session,
        ctx: &mut SV::CTX,
        reuse: bool,
        error: Option<&Error>,
    ) -> Option<Stream>
    where
        SV: ProxyHttp + Send + Sync,
        SV::CTX: Send + Sync,
    {
        self.inner.logging(&mut session, error, ctx).await;

        if reuse {
            // TODO: log error
            session.downstream_session.finish().await.ok().flatten()
        } else {
            None
        }
    }
}

use pingora_cache::HttpCache;
use pingora_core::protocols::http::compression::ResponseCompressionCtx;

/// The established HTTP session
///
/// This object is what users interact with in order to access the request itself or change the proxy
/// behavior.
pub struct Session {
    /// the HTTP session to downstream (the client)
    pub downstream_session: Box<HttpSession>,
    /// The interface to control HTTP caching
    pub cache: HttpCache,
    /// (de)compress responses coming into the proxy (from upstream)
    pub upstream_compression: ResponseCompressionCtx,
    /// ignore downstream range (skip downstream range filters)
    pub ignore_downstream_range: bool,
    // the context from parent request
    subrequest_ctx: Option<Box<SubReqCtx>>,
    // Downstream filter modules
    pub downstream_modules_ctx: HttpModuleCtx,
}

impl Session {
    fn new(
        downstream_session: impl Into<Box<HttpSession>>,
        downstream_modules: &HttpModules,
    ) -> Self {
        Session {
            downstream_session: downstream_session.into(),
            cache: HttpCache::new(),
            upstream_compression: ResponseCompressionCtx::new(0, false), // disable both
            ignore_downstream_range: false,
            subrequest_ctx: None,
            downstream_modules_ctx: downstream_modules.build_ctx(),
        }
    }

    /// Create a new [Session] from the given [Stream]
    ///
    /// This function is mostly used for testing and mocking.
    pub fn new_h1(stream: Stream) -> Self {
        let modules = HttpModules::new();
        Self::new(Box::new(HttpSession::new_http1(stream)), &modules)
    }

    /// Create a new [Session] from the given [Stream] with modules
    ///
    /// This function is mostly used for testing and mocking.
    pub fn new_h1_with_modules(stream: Stream, downstream_modules: &HttpModules) -> Self {
        Self::new(Box::new(HttpSession::new_http1(stream)), downstream_modules)
    }

    pub fn as_downstream_mut(&mut self) -> &mut HttpSession {
        &mut self.downstream_session
    }

    pub fn as_downstream(&self) -> &HttpSession {
        &self.downstream_session
    }

    /// Write HTTP response with the given error code to the downstream
    pub async fn respond_error(&mut self, error: u16) -> Result<()> {
        let resp = HttpSession::generate_error(error);
        self.write_response_header(Box::new(resp), true)
            .await
            .unwrap_or_else(|e| {
                self.downstream_session.set_keepalive(None);
                error!("failed to send error response to downstream: {e}");
            });
        Ok(())
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

    pub async fn write_response_tasks(&mut self, mut tasks: Vec<HttpTask>) -> Result<bool> {
        for task in tasks.iter_mut() {
            match task {
                HttpTask::Header(resp, end) => {
                    self.downstream_modules_ctx
                        .response_header_filter(resp, *end)
                        .await?;
                }
                HttpTask::Body(data, end) => {
                    self.downstream_modules_ctx
                        .response_body_filter(data, *end)?;
                }
                _ => { /* HttpModules doesn't handle trailer yet */ }
            }
        }
        self.downstream_session.response_duplex_vec(tasks).await
    }
}

impl AsRef<HttpSession> for Session {
    fn as_ref(&self) -> &HttpSession {
        &self.downstream_session
    }
}

impl AsMut<HttpSession> for Session {
    fn as_mut(&mut self) -> &mut HttpSession {
        &mut self.downstream_session
    }
}

use std::ops::{Deref, DerefMut};

impl Deref for Session {
    type Target = HttpSession;

    fn deref(&self) -> &Self::Target {
        &self.downstream_session
    }
}

impl DerefMut for Session {
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

impl<SV> HttpProxy<SV> {
    async fn process_request(
        self: &Arc<Self>,
        mut session: Session,
        mut ctx: <SV as ProxyHttp>::CTX,
    ) -> Option<Stream>
    where
        SV: ProxyHttp + Send + Sync + 'static,
        <SV as ProxyHttp>::CTX: Send + Sync,
    {
        if let Err(e) = self
            .inner
            .early_request_filter(&mut session, &mut ctx)
            .await
        {
            self.handle_error(&mut session, &mut ctx, e, "Fail to early filter request:")
                .await;
            return None;
        }

        let req = session.downstream_session.req_header_mut();

        // Built-in downstream request filters go first
        if let Err(e) = session
            .downstream_modules_ctx
            .request_header_filter(req)
            .await
        {
            self.handle_error(
                &mut session,
                &mut ctx,
                e,
                "Failed in downstream modules request filter:",
            )
            .await;
            return None;
        }

        match self.inner.request_filter(&mut session, &mut ctx).await {
            Ok(response_sent) => {
                if response_sent {
                    // TODO: log error
                    self.inner.logging(&mut session, None, &mut ctx).await;
                    return session.downstream_session.finish().await.ok().flatten();
                }
                /* else continue */
            }
            Err(e) => {
                self.handle_error(&mut session, &mut ctx, e, "Fail to filter request:")
                    .await;
                return None;
            }
        }

        if let Some((reuse, err)) = self.proxy_cache(&mut session, &mut ctx).await {
            // cache hit
            return self.finish(session, &mut ctx, reuse, err.as_deref()).await;
        }
        // either uncacheable, or cache miss

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
                    if session.response_written().is_none() {
                        match session.write_response_header_ref(&BAD_GATEWAY).await {
                            Ok(()) => {}
                            Err(e) => {
                                self.handle_error(
                                    &mut session,
                                    &mut ctx,
                                    e,
                                    "Error responding with Bad Gateway:",
                                )
                                .await;

                                return None;
                            }
                        }
                    }

                    return self.finish(session, &mut ctx, false, None).await;
                }
                /* else continue */
            }
            Err(e) => {
                self.handle_error(
                    &mut session,
                    &mut ctx,
                    e,
                    "Error deciding if we should proxy to upstream:",
                )
                .await;
                return None;
            }
        }

        let mut retries: usize = 0;

        let mut server_reuse = false;
        let mut proxy_error: Option<Box<Error>> = None;

        while retries < MAX_RETRIES {
            retries += 1;

            let (reuse, e) = self.proxy_to_upstream(&mut session, &mut ctx).await;
            server_reuse = reuse;

            match e {
                Some(error) => {
                    let retry = error.retry();
                    proxy_error = Some(error);
                    if !retry {
                        break;
                    }
                    // only log error that will be retried here, the final error will be logged below
                    warn!(
                        "Fail to proxy: {}, tries: {}, retry: {}, {}",
                        proxy_error.as_ref().unwrap(),
                        retries,
                        retry,
                        self.inner.request_summary(&session, &ctx)
                    );
                }
                None => {
                    proxy_error = None;
                    break;
                }
            };
        }

        // serve stale if error
        // Check both error and cache before calling the function because await is not cheap
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
            let status = self.inner.fail_to_proxy(&mut session, e, &mut ctx).await;

            // final error will have > 0 status unless downstream connection is dead
            if !self.inner.suppress_error_log(&session, &ctx, e) {
                error!(
                    "Fail to proxy: {}, status: {}, tries: {}, retry: {}, {}",
                    final_error.as_ref().unwrap(),
                    status,
                    retries,
                    false, // we never retry here
                    self.inner.request_summary(&session, &ctx)
                );
            }
        }

        // logging() will be called in finish()
        self.finish(session, &mut ctx, server_reuse, final_error.as_deref())
            .await
    }

    async fn handle_error(
        &self,
        session: &mut Session,
        ctx: &mut <SV as ProxyHttp>::CTX,
        e: Box<Error>,
        context: &str,
    ) where
        SV: ProxyHttp + Send + Sync + 'static,
        <SV as ProxyHttp>::CTX: Send + Sync,
    {
        if !self.inner.suppress_error_log(session, ctx, &e) {
            error!(
                "{context} {}, {}",
                e,
                self.inner.request_summary(session, ctx)
            );
        }
        self.inner.fail_to_proxy(session, &e, ctx).await;
        self.inner.logging(session, Some(&e), ctx).await;
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
trait Subrequest {
    async fn process_subrequest(
        self: &Arc<Self>,
        session: Box<HttpSession>,
        sub_req_ctx: Box<SubReqCtx>,
    );
}

#[async_trait]
impl<SV> Subrequest for HttpProxy<SV>
where
    SV: ProxyHttp + Send + Sync + 'static,
    <SV as ProxyHttp>::CTX: Send + Sync,
{
    async fn process_subrequest(
        self: &Arc<Self>,
        session: Box<HttpSession>,
        sub_req_ctx: Box<SubReqCtx>,
    ) {
        debug!("starting subrequest");
        let mut session = match self.handle_new_request(session).await {
            Some(downstream_session) => Session::new(downstream_session, &self.downstream_modules),
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

#[async_trait]
impl<SV> HttpServerApp for HttpProxy<SV>
where
    SV: ProxyHttp + Send + Sync + 'static,
    <SV as ProxyHttp>::CTX: Send + Sync,
{
    async fn process_new_http(
        self: &Arc<Self>,
        session: HttpSession,
        shutdown: &ShutdownWatch,
    ) -> Option<Stream> {
        let session = Box::new(session);

        // TODO: keepalive pool, use stack
        let mut session = match self.handle_new_request(session).await {
            Some(downstream_session) => Session::new(downstream_session, &self.downstream_modules),
            None => return None, // bad request
        };

        if *shutdown.borrow() {
            // stop downstream from reusing if this service is shutting down soon
            session.set_keepalive(None);
        } else {
            // default 60s
            session.set_keepalive(Some(60));
        }

        let ctx = self.inner.new_ctx();
        self.process_request(session, ctx).await
    }

    async fn http_cleanup(&self) {
        // Notify all keepalived requests blocking on read_request() to abort
        self.shutdown.notify_waiters();

        // TODO: impl shutting down flag so that we don't need to read stack.is_shutting_down()
    }

    fn server_options(&self) -> Option<&HttpServerOptions> {
        self.server_options.as_ref()
    }

    // TODO implement h2_options
}

use pingora_core::services::listening::Service;

/// Create a [Service] from the user implemented [ProxyHttp].
///
/// The returned [Service] can be hosted by a [pingora_core::server::Server] directly.
pub fn http_proxy_service<SV>(conf: &Arc<ServerConf>, inner: SV) -> Service<HttpProxy<SV>>
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
) -> Service<HttpProxy<SV>>
where
    SV: ProxyHttp,
{
    let mut proxy = HttpProxy::new(inner, conf.clone());
    proxy.handle_init_modules();
    Service::new(name.to_string(), proxy)
}
