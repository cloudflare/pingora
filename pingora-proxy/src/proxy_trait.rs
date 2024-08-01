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

use super::*;
use pingora_cache::{key::HashBinary, CacheKey, CacheMeta, RespCacheable, RespCacheable::*};
use std::time::Duration;

/// The interface to control the HTTP proxy
///
/// The methods in [ProxyHttp] are filters/callbacks which will be performed on all requests at their
/// particular stage (if applicable).
///
/// If any of the filters returns [Result::Err], the request will fail, and the error will be logged.
#[cfg_attr(not(doc_async_trait), async_trait)]
pub trait ProxyHttp {
    /// The per request object to share state across the different filters
    type CTX;

    /// Define how the `ctx` should be created.
    fn new_ctx(&self) -> Self::CTX;

    /// Define where the proxy should send the request to.
    ///
    /// The returned [HttpPeer] contains the information regarding where and how this request should
    /// be forwarded to.
    async fn upstream_peer(
        &self,
        session: &mut Session,
        ctx: &mut Self::CTX,
    ) -> Result<Box<HttpPeer>>;

    /// Set up downstream modules.
    ///
    /// In this phase, users can add or configure [HttpModules] before the server starts up.
    ///
    /// In the default implementation of this method, [ResponseCompressionBuilder] is added
    /// and disabled.
    fn init_downstream_modules(&self, modules: &mut HttpModules) {
        // Add disabled downstream compression module by default
        modules.add_module(ResponseCompressionBuilder::enable(0));
    }

    /// Handle the incoming request.
    ///
    /// In this phase, users can parse, validate, rate limit, perform access control and/or
    /// return a response for this request.
    ///
    /// If the user already sent a response to this request, an `Ok(true)` should be returned so that
    /// the proxy would exit. The proxy continues to the next phases when `Ok(false)` is returned.
    ///
    /// By default this filter does nothing and returns `Ok(false)`.
    async fn request_filter(&self, _session: &mut Session, _ctx: &mut Self::CTX) -> Result<bool>
    where
        Self::CTX: Send + Sync,
    {
        Ok(false)
    }

    /// Handle the incoming request before any downstream module is executed.
    ///
    /// This function is similar to [Self::request_filter()] but execute before any other logic
    /// especially the downstream modules. The main purpose of this function is to provide finer
    /// grained control of behavior of the modules.
    ///
    /// Note that because this function is executed before any module that might provide access
    /// control or rate limiting, logic should stay in request_filter() if it can in order to be
    /// protected by said modules.
    async fn early_request_filter(&self, _session: &mut Session, _ctx: &mut Self::CTX) -> Result<()>
    where
        Self::CTX: Send + Sync,
    {
        Ok(())
    }

    /// Handle the incoming request body.
    ///
    /// This function will be called every time a piece of request body is received. The `body` is
    /// **not the entire request body**.
    ///
    /// The async nature of this function allows to throttle the upload speed and/or executing
    /// heavy computation logic such as WAF rules on offloaded threads without blocking the threads
    /// who process the requests themselves.
    async fn request_body_filter(
        &self,
        _session: &mut Session,
        _body: &mut Option<Bytes>,
        _end_of_stream: bool,
        _ctx: &mut Self::CTX,
    ) -> Result<()>
    where
        Self::CTX: Send + Sync,
    {
        Ok(())
    }

    /// This filter decides if the request is cacheable and what cache backend to use
    ///
    /// The caller can interact with `Session.cache` to enable caching.
    ///
    /// By default this filter does nothing which effectively disables caching.
    // Ideally only session.cache should be modified, TODO: reflect that in this interface
    fn request_cache_filter(&self, _session: &mut Session, _ctx: &mut Self::CTX) -> Result<()> {
        Ok(())
    }

    /// This callback generates the cache key
    ///
    /// This callback is called only when cache is enabled for this request
    ///
    /// By default this callback returns a default cache key generated from the request.
    fn cache_key_callback(&self, session: &Session, _ctx: &mut Self::CTX) -> Result<CacheKey> {
        let req_header = session.req_header();
        Ok(CacheKey::default(req_header))
    }

    /// This callback is invoked when a cacheable response is ready to be admitted to cache
    fn cache_miss(&self, session: &mut Session, _ctx: &mut Self::CTX) {
        session.cache.cache_miss();
    }

    /// This filter is called after a successful cache lookup and before the cache asset is ready to
    /// be used.
    ///
    /// This filter allow the user to log or force expire the asset.
    // flex purge, other filtering, returns whether asset is should be force expired or not
    async fn cache_hit_filter(
        &self,
        _session: &Session,
        _meta: &CacheMeta,
        _ctx: &mut Self::CTX,
    ) -> Result<bool>
    where
        Self::CTX: Send + Sync,
    {
        Ok(false)
    }

    /// Decide if a request should continue to upstream after not being served from cache.
    ///
    /// returns: Ok(true) if the request should continue, Ok(false) if a response was written by the
    /// callback and the session should be finished, or an error
    ///
    /// This filter can be used for deferring checks like rate limiting or access control to when they
    /// actually needed after cache miss.
    async fn proxy_upstream_filter(
        &self,
        _session: &mut Session,
        _ctx: &mut Self::CTX,
    ) -> Result<bool>
    where
        Self::CTX: Send + Sync,
    {
        Ok(true)
    }

    /// Decide if the response is cacheable
    fn response_cache_filter(
        &self,
        _session: &Session,
        _resp: &ResponseHeader,
        _ctx: &mut Self::CTX,
    ) -> Result<RespCacheable> {
        Ok(Uncacheable(NoCacheReason::Custom("default")))
    }

    /// Decide how to generate cache vary key from both request and response
    ///
    /// None means no variance is needed.
    fn cache_vary_filter(
        &self,
        _meta: &CacheMeta,
        _ctx: &mut Self::CTX,
        _req: &RequestHeader,
    ) -> Option<HashBinary> {
        // default to None for now to disable vary feature
        None
    }

    /// Decide if the incoming request's condition _fails_ against the cached response.
    ///
    /// Returning `Ok(true)` means that the response does _not_ match against the condition, and
    /// that the proxy can return `304 Not Modified` downstream.
    ///
    /// An example is a conditional GET request with `If-None-Match: "foobar"`. If the cached
    /// response contains the `ETag: "foobar"`, then the condition fails, and `304 Not Modified`
    /// should be returned. Else, the condition passes which means the full `200 OK` response must
    /// be sent.
    fn cache_not_modified_filter(
        &self,
        session: &Session,
        resp: &ResponseHeader,
        _ctx: &mut Self::CTX,
    ) -> Result<bool> {
        Ok(
            pingora_core::protocols::http::conditional_filter::not_modified_filter(
                session.req_header(),
                resp,
            ),
        )
    }

    /// Modify the request before it is sent to the upstream
    ///
    /// Unlike [Self::request_filter()], this filter allows to change the request headers to send
    /// to the upstream.
    async fn upstream_request_filter(
        &self,
        _session: &mut Session,
        _upstream_request: &mut RequestHeader,
        _ctx: &mut Self::CTX,
    ) -> Result<()>
    where
        Self::CTX: Send + Sync,
    {
        Ok(())
    }

    /// Modify the response header from the upstream
    ///
    /// The modification is before caching, so any change here will be stored in the cache if enabled.
    ///
    /// Responses served from cache won't trigger this filter. If the cache needed revalidation,
    /// only the 304 from upstream will trigger the filter (though it will be merged into the
    /// cached header, not served directly to downstream).
    fn upstream_response_filter(
        &self,
        _session: &mut Session,
        _upstream_response: &mut ResponseHeader,
        _ctx: &mut Self::CTX,
    ) {
    }

    /// Modify the response header before it is send to the downstream
    ///
    /// The modification is after caching. This filter is called for all responses including
    /// responses served from cache.
    async fn response_filter(
        &self,
        _session: &mut Session,
        _upstream_response: &mut ResponseHeader,
        _ctx: &mut Self::CTX,
    ) -> Result<()>
    where
        Self::CTX: Send + Sync,
    {
        Ok(())
    }

    /// Similar to [Self::upstream_response_filter()] but for response body
    ///
    /// This function will be called every time a piece of response body is received. The `body` is
    /// **not the entire response body**.
    fn upstream_response_body_filter(
        &self,
        _session: &mut Session,
        _body: &mut Option<Bytes>,
        _end_of_stream: bool,
        _ctx: &mut Self::CTX,
    ) {
    }

    /// Similar to [Self::upstream_response_filter()] but for response trailers
    fn upstream_response_trailer_filter(
        &self,
        _session: &mut Session,
        _upstream_trailers: &mut header::HeaderMap,
        _ctx: &mut Self::CTX,
    ) -> Result<()> {
        Ok(())
    }

    /// Similar to [Self::response_filter()] but for response body chunks
    fn response_body_filter(
        &self,
        _session: &mut Session,
        _body: &mut Option<Bytes>,
        _end_of_stream: bool,
        _ctx: &mut Self::CTX,
    ) -> Result<Option<Duration>>
    where
        Self::CTX: Send + Sync,
    {
        Ok(None)
    }

    /// Similar to [Self::response_filter()] but for response trailers.
    /// Note, returning an Ok(Some(Bytes)) will result in the downstream response
    /// trailers being written to the response body.
    ///
    /// TODO: make this interface more intuitive
    async fn response_trailer_filter(
        &self,
        _session: &mut Session,
        _upstream_trailers: &mut header::HeaderMap,
        _ctx: &mut Self::CTX,
    ) -> Result<Option<Bytes>>
    where
        Self::CTX: Send + Sync,
    {
        Ok(None)
    }

    /// This filter is called when the entire response is sent to the downstream successfully or
    /// there is a fatal error that terminate the request.
    ///
    /// An error log is already emitted if there is any error. This phase is used for collecting
    /// metrics and sending access logs.
    async fn logging(&self, _session: &mut Session, _e: Option<&Error>, _ctx: &mut Self::CTX)
    where
        Self::CTX: Send + Sync,
    {
    }

    /// A value of true means that the log message will be suppressed. The default value is false.
    fn suppress_error_log(&self, _session: &Session, _ctx: &Self::CTX, _error: &Error) -> bool {
        false
    }

    /// This filter is called when there is an error **after** a connection is established (or reused)
    /// to the upstream.
    fn error_while_proxy(
        &self,
        peer: &HttpPeer,
        session: &mut Session,
        e: Box<Error>,
        _ctx: &mut Self::CTX,
        client_reused: bool,
    ) -> Box<Error> {
        let mut e = e.more_context(format!("Peer: {}", peer));
        // only reused client connections where retry buffer is not truncated
        e.retry
            .decide_reuse(client_reused && !session.as_ref().retry_buffer_truncated());
        e
    }

    /// This filter is called when there is an error in the process of establishing a connection
    /// to the upstream.
    ///
    /// In this filter the user can decide whether the error is retry-able by marking the error `e`.
    ///
    /// If the error can be retried, [Self::upstream_peer()] will be called again so that the user
    /// can decide whether to send the request to the same upstream or another upstream that is possibly
    /// available.
    fn fail_to_connect(
        &self,
        _session: &mut Session,
        _peer: &HttpPeer,
        _ctx: &mut Self::CTX,
        e: Box<Error>,
    ) -> Box<Error> {
        e
    }

    /// This filter is called when the request encounters a fatal error.
    ///
    /// Users may write an error response to the downstream if the downstream is still writable.
    ///
    /// The response status code of the error response maybe returned for logging purpose.
    async fn fail_to_proxy(&self, session: &mut Session, e: &Error, _ctx: &mut Self::CTX) -> u16
    where
        Self::CTX: Send + Sync,
    {
        let server_session = session.as_mut();
        let code = match e.etype() {
            HTTPStatus(code) => *code,
            _ => {
                match e.esource() {
                    ErrorSource::Upstream => 502,
                    ErrorSource::Downstream => {
                        match e.etype() {
                            WriteError | ReadError | ConnectionClosed => {
                                /* conn already dead */
                                0
                            }
                            _ => 400,
                        }
                    }
                    ErrorSource::Internal | ErrorSource::Unset => 500,
                }
            }
        };
        if code > 0 {
            server_session.respond_error(code).await
        }
        code
    }

    /// Decide whether should serve stale when encountering an error or during revalidation
    ///
    /// An implementation should follow
    /// <https://datatracker.ietf.org/doc/html/rfc9111#section-4.2.4>
    /// <https://www.rfc-editor.org/rfc/rfc5861#section-4>
    ///
    /// This filter is only called if cache is enabled.
    // 5xx HTTP status will be encoded as ErrorType::HTTPStatus(code)
    fn should_serve_stale(
        &self,
        _session: &mut Session,
        _ctx: &mut Self::CTX,
        error: Option<&Error>, // None when it is called during stale while revalidate
    ) -> bool {
        // A cache MUST NOT generate a stale response unless
        // it is disconnected
        // or doing so is explicitly permitted by the client or origin server
        // (e.g. headers or an out-of-band contract)
        error.map_or(false, |e| e.esource() == &ErrorSource::Upstream)
    }

    /// This filter is called when the request just established or reused a connection to the upstream
    ///
    /// This filter allows user to log timing and connection related info.
    async fn connected_to_upstream(
        &self,
        _session: &mut Session,
        _reused: bool,
        _peer: &HttpPeer,
        #[cfg(unix)] _fd: std::os::unix::io::RawFd,
        #[cfg(windows)] _sock: std::os::windows::io::RawSocket,
        _digest: Option<&Digest>,
        _ctx: &mut Self::CTX,
    ) -> Result<()>
    where
        Self::CTX: Send + Sync,
    {
        Ok(())
    }

    /// This callback is invoked every time request related error log needs to be generated
    ///
    /// Users can define what is important to be written about this request via the returned string.
    fn request_summary(&self, session: &Session, _ctx: &Self::CTX) -> String {
        session.as_ref().request_summary()
    }

    /// Whether the request should be used to invalidate(delete) the HTTP cache
    ///
    /// - `true`: this request will be used to invalidate the cache.
    /// - `false`: this request is a treated as a normal request
    fn is_purge(&self, _session: &Session, _ctx: &Self::CTX) -> bool {
        false
    }

    /// This filter is called after the proxy cache generates the downstream response to the purge
    /// request (to invalidate or delete from the HTTP cache), based on the purge status, which
    /// indicates whether the request succeeded or failed.
    ///
    /// The filter allows the user to modify or replace the generated downstream response.
    /// If the filter returns `Err`, the proxy will instead send a 500 response.
    fn purge_response_filter(
        &self,
        _session: &Session,
        _ctx: &mut Self::CTX,
        _purge_status: PurgeStatus,
        _purge_response: &mut std::borrow::Cow<'static, ResponseHeader>,
    ) -> Result<()> {
        Ok(())
    }
}
