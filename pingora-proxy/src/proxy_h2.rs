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

use futures::future::OptionFuture;
use futures::StreamExt;

use super::*;
use crate::proxy_cache::{range_filter::RangeBodyFilter, ServeFromCache};
use crate::proxy_common::*;
use http::{header::CONTENT_LENGTH, Method, StatusCode};
use pingora_cache::CachePhase;
use pingora_core::protocols::http::authority::{
    raw_target_authority, validate_request_authority, RawTargetAuthority,
};
use pingora_core::protocols::http::custom::CUSTOM_MESSAGE_QUEUE_SIZE;
use pingora_core::protocols::http::v2::{client::Http2Session, write_body};

/// Derive H2 `:path`, separating H1 absolute-form components ([RFC 9113 section 8.3.1]).
///
/// Reclassifies after filters and rejects non-UTF-8 or ambiguous targets.
///
/// [RFC 9113 section 8.3.1]: https://www.rfc-editor.org/rfc/rfc9113.html#section-8.3.1
fn h2_path_and_query(header: &RequestHeader) -> Result<http::uri::PathAndQuery> {
    let target = header.raw_path();
    if !header.raw_path_is_utf8() {
        return Error::e_explain(
            InvalidHTTPHeader,
            "non-UTF-8 request target cannot be forwarded over HTTP/2",
        );
    }
    let Some(uri_path_and_query) = header.uri.path_and_query() else {
        return Ok(http::uri::PathAndQuery::from_static("/"));
    };
    // Preserve origin-form and the server-wide `OPTIONS *` asterisk-form (RFC 9112 section 3.2.4).
    // https://www.rfc-editor.org/rfc/rfc9112.html#section-3.2.4
    if target.starts_with(b"/") || target == b"*" {
        return Ok(uri_path_and_query.clone());
    }

    // Origin-form and asterisk-form returned above; classify the remainder to extract an H1
    // absolute-form path/query or reject ambiguous authority syntax.
    let absolute_path_and_query = match raw_target_authority(target) {
        RawTargetAuthority::None => return Ok(uri_path_and_query.clone()),
        RawTargetAuthority::AmbiguousAuthority => {
            return Error::e_explain(
                InvalidHTTPHeader,
                "ambiguous HTTP absolute-form request target",
            )
        }
        RawTargetAuthority::Absolute { path_and_query, .. } => path_and_query,
    };
    if absolute_path_and_query.is_empty() {
        return Ok(http::uri::PathAndQuery::from_static("/"));
    }
    if absolute_path_and_query.first() == Some(&b'?') {
        let mut origin_form = Vec::with_capacity(absolute_path_and_query.len() + 1);
        origin_form.push(b'/');
        origin_form.extend_from_slice(absolute_path_and_query);
        return http::uri::PathAndQuery::try_from(origin_form).or_err(
            InvalidHTTPHeader,
            "invalid query in absolute-form request target",
        );
    }
    http::uri::PathAndQuery::try_from(absolute_path_and_query).or_err(
        InvalidHTTPHeader,
        "invalid path in absolute-form request target",
    )
}

fn update_h2_scheme_authority(
    header: &mut RequestHeader,
    raw_host: &[u8],
    tls: bool,
    path_and_query: http::uri::PathAndQuery,
) -> Result<()> {
    let authority = http::uri::Authority::try_from(raw_host).map_err(|cause| {
        Error::because(
            InvalidHTTPHeader,
            format!("invalid authority from Host {raw_host:?}"),
            cause,
        )
    })?;
    // Last guard before this authority is serialized on the wire.
    if authority.as_str().contains('@') {
        return Error::e_explain(InvalidHTTPHeader, "userinfo in Host header");
    }

    let scheme = if tls { "https" } else { "http" };
    let uri = http::uri::Builder::new()
        .scheme(scheme)
        .authority(authority)
        .path_and_query(path_and_query)
        .build();
    match uri {
        Ok(uri) => {
            header.set_uri(uri);
            Ok(())
        }
        Err(_) => Error::e_explain(
            InvalidHTTPHeader,
            format!("failed to build H2 URI from Host {raw_host:?}"),
        ),
    }
}

/// A failed [`HttpProxy::send_body_to2`].
struct H2BodySendError {
    error: Box<Error>,
    /// Whether the downstream H2 stream closed while the body write to the upstream was pending.
    /// A request body filter error is not a close, even with a downstream source.
    downstream_closed: bool,
}

impl From<Box<Error>> for H2BodySendError {
    fn from(error: Box<Error>) -> Self {
        Self {
            error,
            downstream_closed: false,
        }
    }
}

impl<SV, C, DS> HttpProxy<SV, C, DS>
where
    C: custom::Connector,
    DS: DownstreamSession,
{
    pub(crate) async fn proxy_down_to_up(
        &self,
        session: &mut Session<DS>,
        client_session: &mut Http2Session,
        peer: &HttpPeer,
        ctx: &mut <SV as ProxyHttp<DS>>::CTX,
    ) -> (bool, Option<Box<Error>>)
    // (reuse_server, error)
    where
        SV: ProxyHttp<DS> + Send + Sync,
        <SV as ProxyHttp<DS>>::CTX: Send + Sync,
    {
        let mut req = session.req_header().clone();
        let authority_policy = AuthorityPolicy::from(session.downstream_session.is_custom());

        // A patched HTTP/1 parser can preserve non-UTF-8 request-target bytes, but `http::Uri`,
        // which the H2 client API requires, cannot represent them. Reject client input as a
        // downstream error before a filter has a chance to mutate the target. This wire-format
        // constraint applies to both standard and custom downstream authority policies.
        if !req.raw_path_is_utf8() {
            let e = Error::explain(
                InvalidHTTPHeader,
                "non-UTF-8 request target cannot be forwarded over HTTP/2",
            );
            return (false, Some(e.into_down()));
        }

        if req.version != Version::HTTP_2 || authority_policy.is_custom() {
            if let Err(e) =
                sanitize_h2_upstream_request(&mut req, peer.options.http_upstream_request_policy)
            {
                return (false, Some(e.into_down()));
            }
            /* remove H1 specific headers */
            // https://github.com/hyperium/h2/blob/d3b9f1e36aadc1a7a6804e2f8e86d3fe4a244b4f/src/proto/streams/send.rs#L72
            req.remove_header(&http::header::TRANSFER_ENCODING);
            req.remove_header(&http::header::CONNECTION);
            req.remove_header(&http::header::UPGRADE);
            req.remove_header(KEEP_ALIVE);
            req.remove_header(PROXY_CONNECTION);
        }

        /* turn it into h2 */
        req.set_version(Version::HTTP_2);

        if session.cache.enabled() {
            pingora_cache::filters::upstream::request_filter(
                &mut req,
                session.cache.maybe_cache_meta(),
            );
            session.mark_upstream_headers_mutated_for_cache();
        }

        match self
            .inner
            .upstream_request_filter(session, &mut req, ctx)
            .await
        {
            Ok(_) => { /* continue */ }
            Err(e) => {
                return (false, Some(e));
            }
        }

        if authority_policy.is_standard() {
            if let Err(e) = reconcile_upstream_authority(&mut req) {
                return (false, Some(e.into_in()));
            }
            if let Err(e) = validate_request_authority(&req) {
                // The final filter-produced request is invalid, so classify this as internal.
                return (false, Some(e.into_in()));
            }
        }

        // A Host-less H1 absolute-form request still needs H2 :authority. Copy its raw target
        // authority into local storage without inserting a temporary Host header.
        let raw_authority =
            if req.headers.get(http::header::HOST).is_none() && authority_policy.is_standard() {
                raw_target_authority(req.raw_path())
                    .authority()
                    .map(<[u8]>::to_owned)
            } else {
                None
            };
        if req.headers.get(http::header::HOST).is_none()
            && req.uri.authority().is_none()
            && raw_authority.is_none()
        {
            // The final filter-produced request has no authority source.
            let e = Error::explain(InvalidHTTPHeader, "no authority for H2 upstream request");
            return (false, Some(e.into_in()));
        }

        // Run for every request, including Host-less HTTP/1.0, before conversion to
        // `http::request::Parts` discards RequestHeader's raw byte fallback. A failure here after
        // the initial check was produced by a filter and is therefore internal.
        let path_and_query = match h2_path_and_query(&req) {
            Ok(path_and_query) => path_and_query,
            Err(e) => return (false, Some(e.into_in())),
        };

        // Remove H1 `Host` header, save it in order to add to :authority
        // We do this because certain H2 servers expect request not to have a host header.
        // The `Host` is removed after the upstream filters above for 2 reasons
        // 1. there is no API to change the :authority header
        // 2. the filter code needs to be aware of the host vs :authority across http versions otherwise
        let host = req.remove_header(&http::header::HOST);

        session.upstream_compression.request_filter(&req);
        let body_empty = session.as_mut().is_body_empty();

        // whether we support sending END_STREAM on HEADERS if body is empty
        let send_end_stream = req.send_end_stream().expect("req must be h2");

        // Host is consumed locally to build :authority and is never sent on the H2 wire.
        let authority = host
            .as_ref()
            .map(|host| host.as_bytes())
            .or(raw_authority.as_deref());
        if let Some(authority) = authority {
            if let Err(e) =
                update_h2_scheme_authority(&mut req, authority, peer.is_tls(), path_and_query)
            {
                return (false, Some(e));
            }
        }

        let req: http::request::Parts = req.into();

        debug!("Request to h2: {req:?}");

        // send END_STREAM on HEADERS
        let send_header_eos = send_end_stream && body_empty;
        debug!("send END_STREAM on HEADERS: {send_end_stream}");

        let req = Box::new(RequestHeader::from(req));
        if let Err(e) = client_session.write_request_header(req, send_header_eos) {
            return (false, Some(e.into_up()));
        }

        if !send_end_stream && body_empty {
            // send END_STREAM on empty DATA frame
            match client_session.write_request_body(Bytes::new(), true).await {
                Ok(()) => debug!("sent empty DATA frame to h2"),
                Err(e) => {
                    return (false, Some(e.into_up()));
                }
            }
        }

        client_session.read_timeout = peer.options.read_timeout;

        let mut downstream_custom_message_writer = session
            .downstream_session
            .as_custom_mut()
            .and_then(|c| c.take_custom_message_writer());
        // Keep the reader in this caller so it is restored even if retryable
        // upstream errors make try_join! cancel the downstream future.
        let mut downstream_custom_message_reader = match session
            .take_downstream_custom_message_reader(&mut downstream_custom_message_writer)
        {
            Ok(reader) => reader,
            Err(e) => return (false, Some(e)),
        };

        // take the body writer out of the client for easy duplex
        let mut client_body = client_session
            .take_request_body_writer()
            .expect("already send request header");

        // need to get the write_timeout here since we pass the h2 SendStream
        // directly to bidirection_down_to_up
        let write_timeout = peer.options.write_timeout;

        let (tx, rx) = mpsc::channel::<HttpTask>(TASK_BUFFER_SIZE);

        session.as_mut().enable_retry_buffering();

        // Shared signal so the upstream half can distinguish an expected task-pipe
        // closure (the downstream half finished and dropped rx) from an unexpected one.
        let pipe_state = Arc::new(AtomicU8::new(PipeState::Active as u8));

        /* read downstream body and upstream response at the same time */

        let ret = tokio::try_join!(
            self.bidirection_down_to_up(
                session,
                &mut client_body,
                rx,
                ctx,
                write_timeout,
                &mut downstream_custom_message_writer,
                &mut downstream_custom_message_reader,
                pipe_state.clone(),
            ),
            pipe_up_to_down_response(client_session, tx, pipe_state)
        );

        if let Some(custom_session) = session.downstream_session.as_custom_mut() {
            if let Some(downstream_custom_message_writer) = downstream_custom_message_writer {
                match custom_session.restore_custom_message_writer(downstream_custom_message_writer)
                {
                    Ok(_) => { /* continue */ }
                    Err(e) => {
                        return (false, Some(e));
                    }
                }
            }
            if let Some(downstream_custom_message_reader) = downstream_custom_message_reader {
                match custom_session.restore_custom_message_reader(downstream_custom_message_reader)
                {
                    Ok(_) => { /* continue */ }
                    Err(e) => {
                        return (false, Some(e));
                    }
                }
            }
        }

        match ret {
            Ok((downstream_can_reuse, _upstream)) => (downstream_can_reuse, None),
            Err(e) => {
                let upstream_read_timeout =
                    e.esource == ErrorSource::Upstream && matches!(e.etype, ReadTimedout);
                let downstream_error = e.esource == ErrorSource::Downstream;
                // On application level upstream read timeouts, send RST_STREAM CANCEL,
                // we know we have not received END_STREAM at this point since we read timed out.
                // Also cancel the upstream stream when downstream goes away/resets so the
                // upstream peer can release the stream promptly.
                // TODO: implement for write timeouts?
                if upstream_read_timeout || downstream_error {
                    client_body.send_reset(h2::Reason::CANCEL);
                    if upstream_read_timeout {
                        // Mark the underlying H2 connection for shutdown so it's not used
                        // for new streams in case it is hung.
                        client_session.conn.mark_shutdown();
                    }
                }
                (false, Some(e))
            }
        }
    }

    pub(crate) async fn proxy_to_h2_upstream(
        &self,
        session: &mut Session<DS>,
        client_session: &mut Http2Session,
        reused: bool,
        peer: &HttpPeer,
        ctx: &mut <SV as ProxyHttp<DS>>::CTX,
    ) -> (bool, Option<Box<Error>>)
    where
        SV: ProxyHttp<DS> + Send + Sync,
        <SV as ProxyHttp<DS>>::CTX: Send + Sync,
    {
        #[cfg(windows)]
        let raw = client_session.fd() as std::os::windows::io::RawSocket;
        #[cfg(unix)]
        let raw = client_session.fd();

        if let Err(e) = self
            .inner
            .connected_to_upstream(session, reused, peer, raw, client_session.digest(), ctx)
            .await
        {
            return (false, Some(e));
        }

        let (server_session_reuse, error) = self
            .proxy_down_to_up(session, client_session, peer, ctx)
            .await;

        // Record upstream response body bytes received (HTTP/2 DATA payload).
        let upstream_bytes_total = client_session.body_bytes_received();
        session.set_upstream_body_bytes_received(upstream_bytes_total);

        // Note: upstream_write_pending_time is not tracked for HTTP/2 (multiplexed streams).

        (server_session_reuse, error)
    }

    #[allow(clippy::too_many_arguments)]
    async fn process_upstream_tasks_h2(
        &self,
        session: &mut Session<DS>,
        ctx: &mut <SV as ProxyHttp<DS>>::CTX,
        initial_task: HttpTask,
        rx: &mut mpsc::Receiver<HttpTask>,
        serve_from_cache: &mut ServeFromCache,
        range_body_filter: &mut proxy_cache::range_filter::RangeBodyFilter,
        response_state: &mut ResponseStateMachine,
    ) -> Result<Option<bool>>
    where
        SV: ProxyHttp<DS> + Send + Sync,
        <SV as ProxyHttp<DS>>::CTX: Send + Sync,
    {
        if serve_from_cache.should_discard_upstream() {
            response_state
                .maybe_set_upstream_producer_done(ends_upstream_task_stream(&initial_task));
            // Serving the cached response and discarding the upstream one; nothing
            // is written downstream this round, so return None and let the caller
            // continue.
            return Ok(None);
        }

        // Batch: pull as many tasks as we can from rx
        let mut tasks = Vec::with_capacity(TASK_BUFFER_SIZE);
        tasks.push(initial_task);
        // tokio::task::unconstrained because now_or_never may yield None when the future is ready
        while let Some(maybe_task) = tokio::task::unconstrained(rx.recv()).now_or_never() {
            if let Some(t) = maybe_task {
                tasks.push(t);
            } else {
                break; // upstream closed
            }
        }
        response_state
            .maybe_set_upstream_producer_done(tasks.iter().any(ends_upstream_task_stream));

        /* run filters before sending to downstream */
        let mut filtered_tasks = Vec::with_capacity(TASK_BUFFER_SIZE);
        for mut t in tasks {
            if self.revalidate_or_stale(session, &mut t, ctx).await {
                serve_from_cache.enable();
                response_state.enable_cached_response();
                // skip downstream filtering entirely as the 304 will not be sent
                break;
            }
            #[cfg(feature = "upstream_modules")]
            if let HttpTask::Header(header, end_of_stream) = &t {
                self.inner
                    .adjust_upstream_modules(session, header, *end_of_stream, ctx)
                    .await?;
            }
            #[cfg(feature = "upstream_modules")]
            session.upstream_modules_filter_task(&mut t).await?;
            session.upstream_compression.response_filter(&mut t);
            // check error and abort
            // otherwise the error is surfaced via write_response_tasks()
            if !serve_from_cache
                .should_write_response(session.downstream_abandoned_during_cache_fill())
            {
                if let HttpTask::Failed(e) = t {
                    return Err(e);
                }
            }
            let task = self
                .h2_response_filter(session, t, ctx, serve_from_cache, range_body_filter, false)
                .await?;
            if serve_from_cache.is_miss_header() {
                response_state.enable_cached_response();
            }
            if session.downstream_abandoned_during_cache_fill() {
                if serve_from_cache.abandoned_before_admission(&session.cache) {
                    return Err(downstream_closed_before_cache_admission());
                }
                continue;
            }
            filtered_tasks.push(task);
        }

        if !serve_from_cache.should_write_response(session.downstream_abandoned_during_cache_fill())
        {
            // TODO: need to derive response_done from filtered_tasks in case downstream failed already
            return Ok(None);
        }

        let response_done = session.write_response_tasks(filtered_tasks).await?;

        Ok(Some(response_done))
    }

    // returns whether server (downstream) session can be reused
    #[allow(clippy::too_many_arguments)]
    async fn bidirection_down_to_up(
        &self,
        session: &mut Session<DS>,
        client_body: &mut h2::SendStream<bytes::Bytes>,
        mut rx: mpsc::Receiver<HttpTask>,
        ctx: &mut <SV as ProxyHttp<DS>>::CTX,
        write_timeout: Option<Duration>,
        downstream_custom_message_writer: &mut Option<Box<dyn CustomMessageWrite>>,
        downstream_custom_message_reader: &mut Option<
            Box<dyn futures::Stream<Item = Result<Bytes>> + Unpin + Send + Sync + 'static>,
        >,
        pipe_state: Arc<AtomicU8>,
    ) -> Result<bool>
    where
        SV: ProxyHttp<DS> + Send + Sync,
        <SV as ProxyHttp<DS>>::CTX: Send + Sync,
    {
        // setup custom message forwarding, if downstream supports it
        let (
            mut downstream_custom_read,
            mut downstream_custom_write,
            downstream_custom_message_custom_forwarding,
            mut downstream_custom_message_inject_rx,
        ) = if downstream_custom_message_writer.is_some() {
            let (inject_tx, inject_rx) = mpsc::channel::<Bytes>(CUSTOM_MESSAGE_QUEUE_SIZE);
            let inject_rx =
                (!session.downstream_abandoned_during_cache_fill()).then_some(inject_rx);
            (true, true, Some(inject_tx), inject_rx)
        } else {
            (false, false, None, None)
        };

        if let Some(custom_forwarding) = downstream_custom_message_custom_forwarding {
            // Custom handles are owned by the caller so an early error here still
            // lets the caller restore them before retrying another upstream.
            self.inner
                .custom_forwarding(session, ctx, None, custom_forwarding)
                .await?;
        }

        let request_body_finished = session.as_mut().is_body_done();
        let mut downstream_state = DownstreamStateMachine::for_proxy_attempt(
            request_body_finished,
            session.downstream_abandoned_during_cache_fill(),
        );

        // retry, send buffer if it exists
        if let Some(buffer) = session.as_mut().get_retry_buffer() {
            self.send_body_to2(
                session,
                Some(buffer),
                request_body_finished,
                client_body,
                ctx,
                write_timeout,
            )
            .await
            .map_err(|e| e.error)?;
        }

        let mut response_state = ResponseStateMachine::new();

        // these two below can be wrapped into an internal ctx
        // use cache when upstream revalidates (or TODO: error)
        let mut serve_from_cache = ServeFromCache::new();
        let mut range_body_filter = proxy_cache::range_filter::RangeBodyFilter::new();

        let mut next_upstream_task: Option<HttpTask> = None;
        let mut fill_cancellation = CacheFillCancellationState::new();

        /* duplex mode
         * see the Same function for h1 for more comments
         */
        while !downstream_state.is_done()
            || !session.response_work_done(&response_state)
            || downstream_custom_read && !downstream_state.is_errored()
            || downstream_custom_write && !session.downstream_abandoned_during_cache_fill()
        {
            let downstream_abandoned = session.downstream_abandoned_during_cache_fill();
            if downstream_abandoned {
                downstream_custom_message_inject_rx = None;
            }
            fill_cancellation.start_watching(downstream_abandoned, &session.cache);
            // Use optional futures to allow using optional channels in select branches
            let custom_inject_rx_recv: OptionFuture<_> = downstream_custom_message_inject_rx
                .as_mut()
                .map(|rx| rx.recv())
                .into();
            let custom_reader_next: OptionFuture<_> = downstream_custom_message_reader
                .as_mut()
                .map(|reader| reader.next())
                .into();

            // partial read support, this check will also be false if cache is disabled.
            let support_cache_partial_read =
                session.cache.support_streaming_partial_write() == Some(true);
            let upgraded = session.was_upgraded();

            // Similar logic in h1 need to reserve capacity first to avoid deadlock
            // But we don't need to do the same because the h2 client_body pipe is unbounded (never block)
            tokio::select! {
                // NOTE: cannot avoid this copy since h2 owns the buf
                body = session.downstream_session.read_body_or_idle(downstream_state.is_done()), if downstream_state.can_poll() => {
                    debug!("downstream event");
                    let body = match body {
                        Ok(b) => b,
                        Err(e) => {
                            let wait_for_cache_fill = (!serve_from_cache.is_on() && support_cache_partial_read)
                                || serve_from_cache.is_miss();
                            if wait_for_cache_fill {
                                // ignore downstream error so that upstream can continue to write cache
                                if downstream_read_error_means_client_left(&e) {
                                    self.abandon_downstream_for_cache_fill(
                                        &serve_from_cache,
                                        &mut downstream_state,
                                        &mut response_state,
                                        session,
                                        ctx,
                                    );
                                } else {
                                    // Any other read error, such as a timeout or a malformed
                                    // body, leaves the client connected.
                                    downstream_state.to_errored();
                                }
                                if !self.inner.suppress_proxy_warn_log(
                                    session,
                                    ctx,
                                    &e,
                                    ProxyWarnLogContext::DownstreamCache,
                                ) {
                                    warn!(
                                        "Downstream Error ignored during caching: {}, {}",
                                        e,
                                        self.inner.request_summary(session, ctx)
                                    );
                                }
                                // This will not be treated as a final error, but we should signal to
                                // downstream session regardless
                                session.downstream_session.on_proxy_failure(e);
                                continue;
                           } else {
                                return Err(e.into_down());
                           }
                        }
                    };
                    let is_body_done = session.is_body_done();
                    match self.send_body_to2(session, body, is_body_done, client_body, ctx, write_timeout).await {
                        Ok(request_done) =>  {
                            downstream_state.maybe_finished(request_done);
                        },
                        Err(H2BodySendError { error: e, downstream_closed }) if e.esource == ErrorSource::Downstream => {
                            // Downstream reset/errored while the upstream write was blocked
                            // (e.g. on upstream flow control). Same policy as the read error
                            // handling above: ignore the downstream error if the upstream
                            // response is being admitted to cache, otherwise fail so the
                            // downstream stream handles are dropped promptly.
                            let wait_for_cache_fill = (!serve_from_cache.is_on() && support_cache_partial_read)
                                || serve_from_cache.is_miss();
                            if !wait_for_cache_fill {
                                return Err(e);
                            }
                            // ignore downstream error so that upstream can continue to write cache
                            if downstream_closed {
                                self.abandon_downstream_for_cache_fill(
                                    &serve_from_cache,
                                    &mut downstream_state,
                                    &mut response_state,
                                    session,
                                    ctx,
                                );
                            } else {
                                // A request body filter failed while the client is still connected.
                                downstream_state.to_errored();
                            }
                            if !self.inner.suppress_proxy_warn_log(
                                session,
                                ctx,
                                &e,
                                ProxyWarnLogContext::DownstreamCache,
                            ) {
                                warn!(
                                    "Downstream Error ignored during caching: {}, {}",
                                    e,
                                    self.inner.request_summary(session, ctx)
                                );
                            }
                            // This will not be treated as a final error, but we should signal to
                            // downstream session anyway.
                            session.downstream_session.on_proxy_failure(e);
                        },
                        Err(H2BodySendError { error: e, .. }) => {
                            // mark request done, attempt to drain receive
                            warn!("Upstream h2 body send error: {e}");
                            // upstream is what actually errored but we don't want to continue
                            // polling the downstream body
                            downstream_state.to_errored();
                        }
                    };
                },

                // Handle buffered upstream task from previous iteration
                task = async { next_upstream_task.take() }, if next_upstream_task.is_some() => {
                    debug!("buffered upstream event: {:?}", task);
                    if let Some(t) = task {
                        let Some(response_done) = self.process_upstream_tasks_h2(
                            session,
                            ctx,
                            t,
                            &mut rx,
                            &mut serve_from_cache,
                            &mut range_body_filter,
                            &mut response_state,
                        ).await? else {
                            // nothing sent downstream e.g. serve_from_cache
                            continue;
                        };
                        if session.was_upgraded() {
                            return Error::e_explain(H2Error, "upgraded while proxying to h2 session");
                        }
                        response_state.maybe_set_upstream_delivery_done(response_done);
                    } else {
                        debug!("empty upstream event");
                        response_state.maybe_set_upstream_delivery_done(true);
                    }
                },

                task = fill_cancellation.recv_upstream(&mut rx), if session.should_poll_upstream_tasks(&response_state) && next_upstream_task.is_none() => {
                    let task = task?;
                    debug!("upstream event: {:?}", task);
                    if let Some(t) = task {
                        let Some(response_done) = self.process_upstream_tasks_h2(
                            session,
                            ctx,
                            t,
                            &mut rx,
                            &mut serve_from_cache,
                            &mut range_body_filter,
                            &mut response_state,
                        ).await? else {
                            // nothing sent downstream e.g. serve_from_cache
                            continue;
                        };
                        if session.was_upgraded() {
                            // it is very weird if the downstream session decides to upgrade
                            // since the client h2 session cannot, return an error on this case
                            return Error::e_explain(H2Error, "upgraded while proxying to h2 session");
                        }
                        response_state.maybe_set_upstream_delivery_done(response_done);
                    } else {
                        debug!("empty upstream event");
                        if session.downstream_abandoned_during_cache_fill() {
                            return Error::e_explain(
                                InternalError,
                                "upstream response task pipe closed before completion",
                            );
                        }
                        response_state.maybe_set_upstream_delivery_done(true);
                    }
                },

                task = serve_from_cache.next_http_task(&mut session.cache, &mut range_body_filter, upgraded),
                    if !response_state.cached_done()
                        && !downstream_state.is_errored()
                        && serve_from_cache.is_on()
                        && !session.has_pending_downstream_tasks() => { // backpressure: don't queue if pending writes

                    let task = self.h2_response_filter(session, task?, ctx,
                        &mut serve_from_cache,
                        &mut range_body_filter, true).await?;
                    debug!("serve_from_cache task {task:?}");

                    if session.downstream_session.supports_proxy_task_api() {
                        session.send_downstream_proxy_task(task).await?;
                    } else {
                        match session.write_response_tasks(vec![task]).await {
                            Ok(b) => response_state.maybe_set_cache_done(b),
                            Err(e) => if serve_from_cache.is_miss() {
                                // give up writing to downstream but wait for upstream cache write to finish
                                response_state.maybe_set_cache_done(true);
                                self.abandon_downstream_for_cache_fill(
                                    &serve_from_cache,
                                    &mut downstream_state,
                                    &mut response_state,
                                    session,
                                    ctx,
                                );
                                if !self.inner.suppress_proxy_warn_log(
                                    session,
                                    ctx,
                                    &e,
                                    ProxyWarnLogContext::DownstreamCache,
                                ) {
                                    warn!(
                                        "Downstream Error ignored during caching: {}, {}",
                                        e,
                                        self.inner.request_summary(session, ctx)
                                    );
                                }
                                // This will not be treated as a final error, but we should signal to
                                // downstream session regardless
                                session.downstream_session.on_proxy_failure(e);
                                continue;
                            } else {
                                return Err(e);
                            }
                        }
                        // A storage error can disable cache between cached_done
                        // being set and here; see the same guard in proxy_h1.rs.
                        if response_state.cached_done() && session.cache.enabled() {
                            if let Err(e) = session.cache.finish_hit_handler().await {
                                warn!("Error during finish_hit_handler: {}", e);
                            }
                        }
                    }
                }

                // Write queued downstream proxy tasks while also polling for upstream tasks.
                // This allows cache writes to continue even when downstream is stalled.
                //
                // "Gate" branch: ready(()) resolves immediately, so the guard controls
                // whether we enter. This is not a busy-loop because every path through
                // the inner select either (a) drains all pending tasks via
                // write_downstream_proxy_tasks (making the guard false), (b) observes a
                // downstream write error (making downstream_state errored and the guard false),
                // (c) stores an upstream task in next_upstream_task (making the guard false), or
                // (d) blocks on real I/O inside the nested select.
                _ = std::future::ready(()),
                    if !downstream_state.is_errored()
                        && session.has_pending_downstream_tasks()
                        && next_upstream_task.is_none() => {
                    tokio::select! {
                        // Try to write downstream proxy tasks (cancel-safe)
                        write_result = session.write_downstream_proxy_tasks() => {
                            match write_result {
                                Ok(end) => {
                                    response_state.maybe_set_cache_done(end);
                                    // See disabled() guard comment above.
                                    // See enabled() guard comment above.
                                    if response_state.cached_done() && session.cache.enabled() {
                                        if let Err(e) = session.cache.finish_hit_handler().await {
                                            warn!("Error during finish_hit_handler: {}", e);
                                        }
                                    }
                                }
                                Err(e) => if serve_from_cache.is_miss() {
                                    // give up writing to downstream but wait for upstream cache write to finish
                                    response_state.maybe_set_cache_done(true);
                                    self.abandon_downstream_for_cache_fill(
                                        &serve_from_cache,
                                        &mut downstream_state,
                                        &mut response_state,
                                        session,
                                        ctx,
                                    );
                                    if !self.inner.suppress_proxy_warn_log(
                                        session,
                                        ctx,
                                        &e,
                                        ProxyWarnLogContext::DownstreamCache,
                                    ) {
                                        warn!(
                                            "Downstream write error ignored during caching: {}, {}",
                                            e,
                                            self.inner.request_summary(session, ctx)
                                        );
                                    }
                                    session.downstream_session.on_proxy_failure(e);
                                } else {
                                    return Err(e);
                                }
                            }
                        }

                        // Also poll for upstream tasks - if we get one, cancel the write and handle it.
                        upstream_task = rx.recv(), if !response_state.upstream_delivery_done() && serve_from_cache.is_on() && next_upstream_task.is_none() => {
                            if let Some(t) = upstream_task {
                                next_upstream_task = Some(t);
                                continue;
                            } else {
                                response_state.maybe_set_upstream_delivery_done(true);
                            }
                        }
                    }
                }
                data = custom_reader_next, if downstream_custom_read && !downstream_state.is_errored()  => {
                    let Some(data) = data.flatten() else {

                        downstream_custom_read = false;
                        continue;
                    };

                    let data = match data {
                        Ok(data) => data,
                        Err(err) =>  {
                            warn!("downstream_custom_message_reader got error: {err}");
                            downstream_custom_read = false;
                            continue;
                        },
                    };

                    self.inner
                        .downstream_custom_message_proxy_filter(session, data, ctx, true) // true, because it's the last hop for downstream proxying
                        .await?;
                },

                data = custom_inject_rx_recv,
                    if downstream_custom_write && !session.downstream_abandoned_during_cache_fill() => {
                    match data.flatten() {
                        Some(data) => {
                            if let Some(ref mut custom_writer) = downstream_custom_message_writer {
                                custom_writer.write_custom_message(data).await?
                            }
                        },
                        None => {
                            downstream_custom_write = false;
                            if let Some(ref mut custom_writer) = downstream_custom_message_writer {
                                custom_writer.finish_custom().await?;
                            }
                        },
                    }
                },

                else => {
                    break;
                }
            }
        }

        let mut reuse_downstream = !downstream_state.is_errored();
        if reuse_downstream {
            match session.as_mut().finish_body().await {
                Ok(_) => {
                    debug!("finished sending body to downstream");
                }
                Err(e) => {
                    error!("Error finish sending body to downstream: {}", e);
                    reuse_downstream = false;
                }
            }
        }
        // Signal the upstream half that the downstream half completed cleanly before
        // dropping rx, so a resulting task-pipe closure is treated as benign.
        pipe_state.store(PipeState::DownstreamComplete as u8, Ordering::Release);
        Ok(reuse_downstream)
    }

    async fn h2_response_filter(
        &self,
        session: &mut Session<DS>,
        mut task: HttpTask,
        ctx: &mut <SV as ProxyHttp<DS>>::CTX,
        serve_from_cache: &mut ServeFromCache,
        range_body_filter: &mut RangeBodyFilter,
        from_cache: bool, // are the task from cache already
    ) -> Result<HttpTask>
    where
        SV: ProxyHttp<DS> + Send + Sync,
        <SV as ProxyHttp<DS>>::CTX: Send + Sync,
    {
        if !from_cache {
            if let Some(duration) = self.upstream_filter(session, &mut task, ctx).await? {
                trace!("delaying upstream response for {duration:?}");
                wait_for_upstream_delay(
                    &session.cache,
                    session.downstream_abandoned_during_cache_fill(),
                    duration,
                )
                .await;
            }

            // cache the original response before any downstream transformation
            // requests that bypassed cache still need to run filters to see if the response has become cacheable
            if session.cache.enabled() || session.cache.bypassing() {
                if let Err(e) = self
                    .cache_http_task(session, &task, ctx, serve_from_cache)
                    .await
                {
                    let reason = session.cache_failure_reason(&e, NoCacheReason::StorageError);
                    session.cache.disable(reason);
                    if serve_from_cache.cache_write_failure_is_fatal(
                        session.downstream_abandoned_during_cache_fill(),
                    ) {
                        // if the response stream cache body during miss but write fails, it has to
                        // give up the entire request
                        return Err(e);
                    } else {
                        // otherwise, continue processing the response
                        warn!(
                            "Fail to cache response: {}, {}",
                            e,
                            self.inner.request_summary(session, ctx)
                        );
                    }
                }
            }
            // skip the downstream filtering if these tasks are just for cache admission
            if !serve_from_cache
                .should_write_response(session.downstream_abandoned_during_cache_fill())
            {
                return Ok(task);
            }
        } // else: cached/local response, no need to trigger upstream filters and caching

        // normally max file size is tracked in cache_http_task filters (when cache enabled),
        // we will track it in these filters before sending to downstream on specific conditions
        // when cache is disabled
        let track_max_cache_size = matches!(
            session.cache.phase(),
            CachePhase::Disabled(NoCacheReason::PredictedResponseTooLarge)
        );

        let res = match task {
            HttpTask::Header(mut header, eos) => {
                /* Downstream revalidation, only needed when cache is on because otherwise origin
                 * will handle it */
                if session.upstream_headers_mutated_for_cache() {
                    self.downstream_response_conditional_filter(
                        serve_from_cache,
                        session,
                        &mut header,
                        ctx,
                    );
                    if !session.ignore_downstream_range {
                        let range_type = self.inner.range_header_filter(session, &mut header, ctx);
                        range_body_filter.set(range_type);
                    }
                }

                self.inner
                    .response_filter(session, &mut header, ctx)
                    .await?;
                /* Downgrade the version so that write_response_header won't panic */
                header.set_version(Version::HTTP_11);

                // these status codes / method cannot have body, so no need to add chunked encoding
                let no_body = session.req_header().method == "HEAD"
                    || matches!(header.status.as_u16(), 204 | 304);

                /* Add chunked header to tell downstream to use chunked encoding
                 * during the absent of content-length in h2 */
                if !no_body
                    && !header.status.is_informational()
                    && header.headers.get(http::header::CONTENT_LENGTH).is_none()
                {
                    header.insert_header(http::header::TRANSFER_ENCODING, "chunked")?;
                }
                Ok(HttpTask::Header(header, eos))
            }
            HttpTask::Body(data, eos) => {
                if track_max_cache_size {
                    session
                        .cache
                        .track_body_bytes_for_max_file_size(data.as_ref().map_or(0, |d| d.len()));
                }

                let mut data = range_body_filter.filter_body(data);
                if let Some(duration) = self
                    .inner
                    .response_body_filter(session, &mut data, eos, ctx)
                    .await?
                {
                    trace!("delaying downstream response for {duration:?}");
                    time::sleep(duration).await;
                }
                Ok(HttpTask::Body(data, eos))
            }
            HttpTask::UpgradedBody(..) => {
                // An h2 session should not be able to send an h2 upgraded response body,
                // and logically that is impossible unless there is a bug in the client v2 session
                panic!("Unexpected UpgradedBody task while proxy h2");
            }
            HttpTask::Trailer(mut trailers) => {
                let trailer_buffer = match trailers.as_mut() {
                    Some(trailers) => {
                        debug!("Parsing response trailers..");
                        match self
                            .inner
                            .response_trailer_filter(session, trailers, ctx)
                            .await
                        {
                            Ok(buf) => buf,
                            Err(e) => {
                                error!(
                                    "Encountered error while filtering upstream trailers {:?}",
                                    e
                                );
                                None
                            }
                        }
                    }
                    _ => None,
                };
                // if we have a trailer buffer write it to the downstream response body
                if let Some(buffer) = trailer_buffer {
                    // write_body will not write additional bytes after reaching the content-length
                    // for gRPC H2 -> H1 this is not a problem but may be a problem for non gRPC code
                    // https://http2.github.io/http2-spec/#malformed
                    Ok(HttpTask::Body(Some(buffer), true))
                } else {
                    Ok(HttpTask::Trailer(trailers))
                }
            }
            HttpTask::Done => Ok(task),
            HttpTask::Failed(_) => Ok(task), // Do nothing just pass the error down
        };
        // On end, check if the response (based on file size) can be considered cacheable again
        if let Ok(task) = res.as_ref() {
            if track_max_cache_size
                && task.is_end()
                && !matches!(task, HttpTask::Failed(_))
                && !session.cache.exceeded_max_file_size()
            {
                session.cache.response_became_cacheable();
            }
        }
        res
    }

    async fn send_body_to2(
        &self,
        session: &mut Session<DS>,
        mut data: Option<Bytes>,
        end_of_body: bool,
        client_body: &mut h2::SendStream<bytes::Bytes>,
        ctx: &mut <SV as ProxyHttp<DS>>::CTX,
        write_timeout: Option<Duration>,
    ) -> std::result::Result<bool, H2BodySendError>
    where
        SV: ProxyHttp<DS> + Send + Sync,
        <SV as ProxyHttp<DS>>::CTX: Send + Sync,
    {
        session
            .downstream_modules_ctx
            .request_body_filter(&mut data, end_of_body)
            .await?;

        self.inner
            .request_body_filter(session, &mut data, end_of_body, ctx)
            .await?;

        /* it is normal to get 0 bytes because of multi-chunk parsing or request_body_filter.
         * Although there is no harm writing empty byte to h2, unlike h1, we ignore it
         * for consistency */
        if !end_of_body && data.as_ref().is_some_and(|d| d.is_empty()) {
            return Ok(false);
        }

        let (data, end) = match data {
            Some(data) => {
                debug!("Write {} bytes body to h2 upstream", data.len());
                (data, end_of_body)
            }
            None => {
                debug!("Read downstream body done");
                /* send a standalone END_STREAM flag */
                (Bytes::new(), true)
            }
        };

        /* For H2 downstreams, race the upstream write against downstream stream
         * closure. A write blocked on upstream flow control would otherwise keep the
         * downstream stream handles referenced while a downstream RST_STREAM goes
         * unobserved, pinning the downstream connection window credit until the
         * write completes. */
        if let Some(stream_close) = session.downstream_session.watch_h2_stream_close() {
            tokio::select! {
                biased;
                res = write_body(client_body, data, end, write_timeout) => {
                    res.map_err(|e| e.into_up())?;
                }
                close_result = stream_close => {
                    return match close_result {
                        Ok(reason) => Error::e_explain(
                            H2Error,
                            format!("downstream H2 stream closed (reason: {reason}) while writing body to upstream"),
                        ),
                        Err(e) => Err(e),
                    }
                    .map_err(|e| H2BodySendError { error: e.into_down(), downstream_closed: true });
                }
            }
        } else {
            write_body(client_body, data, end, write_timeout)
                .await
                .map_err(|e| e.into_up())?;
        }

        Ok(end_of_body)
    }
}

/* Read response header, body and trailer from h2 upstream and send them to tx */
pub(crate) async fn pipe_up_to_down_response(
    client: &mut Http2Session,
    tx: mpsc::Sender<HttpTask>,
    pipe_state: Arc<AtomicU8>,
) -> Result<()> {
    client
        .read_response_header()
        .await
        .map_err(|e| e.into_up())?; // should we send the error as an HttpTask?

    let resp_header = Box::new(client.response_header().expect("just read").clone());

    match client.check_response_end_or_error() {
        Ok(eos) => {
            // XXX: the h2 crate won't check for content-length underflow
            // if a header frame with END_STREAM is sent without data frames
            // As stated by RFC, "204 or 304 responses contain no content,
            // as does the response to a HEAD request"
            // https://datatracker.ietf.org/doc/html/rfc9113#section-8.1.1
            let req_header = client.request_header().expect("must have sent req");
            if eos
                && req_header.method != Method::HEAD
                && resp_header.status != StatusCode::NO_CONTENT
                && resp_header.status != StatusCode::NOT_MODIFIED
                // RFC technically allows for leading zeroes
                // https://datatracker.ietf.org/doc/html/rfc9110#name-content-length
                && resp_header
                    .headers
                    .get(CONTENT_LENGTH)
                    .is_some_and(|cl| cl.as_bytes().iter().any(|b| *b != b'0'))
            {
                let _ = tx
                    .send(HttpTask::Failed(
                        Error::explain(H2Error, "non-zero content-length on EOS headers frame")
                            .into_up(),
                    ))
                    .await;
                return Ok(());
            }
            tx.send(HttpTask::Header(resp_header, eos))
                .await
                .or_err(InternalError, "sending h2 headers to pipe")?;
        }
        Err(e) => {
            // If upstream errored, then push error to downstream and then quit
            // Don't care if send fails (which means downstream already gone)
            // we were still able to retrieve the headers, so try sending
            let _ = tx.send(HttpTask::Header(resp_header, false)).await;
            let _ = tx.send(HttpTask::Failed(e.into_up())).await;
            return Ok(());
        }
    }

    // Read body from H2 upstream, racing each read against tx.closed().
    //
    // When proxying an H2 upstream response with Content-Length to an H1 downstream,
    // bidirection_down_to_up() may determine the response is complete (all Content-Length
    // bytes written) and exit before the H2 stream signals END_STREAM. This drops the
    // receiving end (rx) of the channel. Without this race, read_response_body() would
    // block until the H2 stream eventually ends (e.g. via trailers or read_timeout),
    // while the downstream side (which could be H1) is in theory already done.
    loop {
        let chunk = tokio::select! {
            biased;
            body = client.read_response_body() => {
                body.map_err(|e| e.into_up()).transpose()
            }
            _ = tx.closed() => None,
        };
        let Some(chunk) = chunk else {
            break;
        };
        let data = match chunk {
            Ok(d) => d,
            Err(e) => {
                // Push the error to downstream and then quit
                let _ = tx.send(HttpTask::Failed(e.into_up())).await;
                // Downstream should consume all remaining data and handle the error
                return Ok(());
            }
        };
        match client.check_response_end_or_error() {
            Ok(eos) => {
                let empty = data.is_empty();
                if empty && !eos {
                    /* it is normal to get 0 bytes because of multi-chunk
                     * don't write 0 bytes to downstream since it will be
                     * misread as the terminating chunk */
                    continue;
                }
                // A send failure is benign only when the downstream half signaled it
                // completed (e.g. an H1 downstream finished by Content-Length before the
                // H2 stream signaled end-of-stream): stop reading the upstream stream.
                // Otherwise the closure is unexpected, so surface the original error.
                let send_result = tx.send(HttpTask::Body(Some(data), eos)).await;
                if send_result.is_err()
                    && PipeState::is_downstream_complete(pipe_state.load(Ordering::Acquire))
                {
                    return Ok(());
                }
                send_result.or_err(InternalError, "sending h2 body to pipe")?;
            }
            Err(e) => {
                // Similar to above, push the error to downstream and then quit
                let _ = tx.send(HttpTask::Failed(e.into_up())).await;
                return Ok(());
            }
        }
    }

    // If the channel is already closed, the downstream half is finished. This
    // skips trailers/done, but the downstream half has already finished so there
    // is nothing more to send. Benign only if the downstream half signaled
    // completion; otherwise the closure is unexpected, so surface it.
    if tx.is_closed() {
        if PipeState::is_downstream_complete(pipe_state.load(Ordering::Acquire)) {
            return Ok(());
        }
        return Error::e_explain(
            InternalError,
            "h2 task pipe closed unexpectedly before trailers",
        );
    }

    // attempt to get trailers, racing against channel close
    let trailers = tokio::select! {
        biased;
        t = client.read_trailers() => {
            match t {
                Ok(t) => t,
                Err(e) => {
                    let _ = tx.send(HttpTask::Failed(e.into_up())).await;
                    return Ok(());
                }
            }
        }
        _ = tx.closed() => {
            // Benign only if the downstream half signaled completion; otherwise
            // the closure is unexpected, so surface it.
            if PipeState::is_downstream_complete(pipe_state.load(Ordering::Acquire)) {
                return Ok(());
            }
            return Error::e_explain(InternalError, "h2 task pipe closed unexpectedly while reading trailers");
        }
    };

    let trailers = trailers.map(Box::new);

    if trailers.is_some() {
        // Benign only if the downstream signaled completion, same as the body sends above.
        let send_result = tx.send(HttpTask::Trailer(trailers)).await;
        if send_result.is_err()
            && PipeState::is_downstream_complete(pipe_state.load(Ordering::Acquire))
        {
            return Ok(());
        }
        send_result.or_err(InternalError, "sending h2 trailer to pipe")?;
    }

    let send_result = tx.send(HttpTask::Done).await;
    if send_result.is_err() && PipeState::is_downstream_complete(pipe_state.load(Ordering::Acquire))
    {
        debug!("h2 to h1 channel closed!");
        return Ok(());
    }
    send_result.or_err(InternalError, "sending h2 done to pipe")?;

    Ok(())
}

#[test]
fn test_update_h2_scheme_authority() {
    fn update(header: &mut RequestHeader, raw_host: &[u8], tls: bool) -> Result<()> {
        let path_and_query = h2_path_and_query(header)?;
        update_h2_scheme_authority(header, raw_host, tls, path_and_query)
    }

    let parts = http::request::Builder::new()
        .body(())
        .unwrap()
        .into_parts()
        .0;
    let mut header = RequestHeader::from(parts);
    update(&mut header, b"example.com", true).unwrap();
    assert_eq!("example.com", header.uri.authority().unwrap());
    let err = update(&mut header, b"user@example.com", true).unwrap_err();
    assert_eq!(err.etype(), &InvalidHTTPHeader);
    update(&mut header, b"example.com:456", true).unwrap();
    assert_eq!("example.com:456", header.uri.authority().unwrap());
    update(&mut header, b"example.com:", true).unwrap();
    assert_eq!("example.com:", header.uri.authority().unwrap());
    let err = update(&mut header, b"example.com:123:345", true).unwrap_err();
    assert_eq!(err.etype(), &InvalidHTTPHeader);
    update(&mut header, b"[::1]", true).unwrap();
    assert_eq!("[::1]", header.uri.authority().unwrap());

    // verify scheme
    update(&mut header, b"example.com", true).unwrap();
    assert_eq!("https://example.com", header.uri);
    update(&mut header, b"example.com", false).unwrap();
    assert_eq!("http://example.com", header.uri);

    // H1 absolute-form is decomposed into H2 pseudo-header components.
    let mut header = RequestHeader::build("GET", b"http://example.com/path?q=1", None).unwrap();
    update(&mut header, b"example.com", false).unwrap();
    assert_eq!(header.uri.path_and_query().unwrap().as_str(), "/path?q=1");

    let mut header = RequestHeader::build(
        "GET",
        b"http://example.com/caf%C3%A9?q=r%C3%A9sum%C3%A9",
        None,
    )
    .unwrap();
    update(&mut header, b"example.com", false).unwrap();
    assert_eq!(
        header.uri.path_and_query().unwrap().as_str(),
        "/caf%C3%A9?q=r%C3%A9sum%C3%A9"
    );

    let mut header = RequestHeader::build("GET", b"http://example.com?only=query", None).unwrap();
    update(&mut header, b"example.com", false).unwrap();
    assert_eq!(
        header.uri.path_and_query().unwrap().as_str(),
        "/?only=query"
    );

    let mut header = RequestHeader::build("GET", b"http://example.com", None).unwrap();
    update(&mut header, b"example.com", false).unwrap();
    assert_eq!(header.uri.path_and_query().unwrap().as_str(), "/");

    // Origin-form remains unchanged.
    let mut header = RequestHeader::build("GET", b"/path?q=1", None).unwrap();
    update(&mut header, b"example.com", false).unwrap();
    assert_eq!(header.uri.path_and_query().unwrap().as_str(), "/path?q=1");

    let mut header = RequestHeader::build("GET", b"http:/\\/\\other.example/admin", None).unwrap();
    let err = update(&mut header, b"example.com", false).unwrap_err();
    assert_eq!(err.etype(), &InvalidHTTPHeader);
    assert_eq!(
        err.context.as_ref().map(|context| context.as_str()),
        Some("ambiguous HTTP absolute-form request target")
    );

    let raw_target = b"http://example.com/\xff";
    let mut header = RequestHeader::build("GET", raw_target, None).unwrap();
    let err = update(&mut header, b"example.com", false).unwrap_err();
    assert_eq!(err.etype(), &InvalidHTTPHeader);
    assert_eq!(
        err.context.as_ref().map(|context| context.as_str()),
        Some("non-UTF-8 request target cannot be forwarded over HTTP/2")
    );
    assert_eq!(header.raw_path(), raw_target);
}

#[test]
fn test_h2_path_is_rooted_for_targets_with_no_authority() {
    // Targets with no absolute-form authority carry no absolute path, so :path is rooted
    // rather than forwarding bytes that are invalid there (RFC 9113 section 8.3.1).
    // Forwarding them would also concatenate against the authority, rendering as
    // "http://example.comfoo/bar".
    for target in [&b"foo/bar"[..], b"myproto:opaque", b"foo?q=1"] {
        let mut header = RequestHeader::build("GET", target, None).unwrap();
        let label = String::from_utf8_lossy(target);
        // The H1 wire serializes these bytes; only the H2 rewrite below discards them.
        assert_eq!(target, header.raw_path(), "{label}");

        let path_and_query = h2_path_and_query(&header).unwrap();
        assert_eq!("/", path_and_query.as_str(), "{label}");

        update_h2_scheme_authority(&mut header, b"example.com", false, path_and_query).unwrap();
        assert_eq!("http://example.com/", header.uri.to_string(), "{label}");
        assert_eq!(
            Some("example.com"),
            header.uri.authority().map(|authority| authority.as_str()),
            "{label}"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{
        cache_miss_session, cache_miss_session_on, drive_custom_sender_while_client_leaves,
        AbandonmentSignalProxy, AbandonmentTiming, CustomSenderOutcome, CustomWriteCalls,
        HeldCustomSenderProxy, RecordingCustomWriter, CANCELLATION_WATCHED_STORAGE,
        WRITER_DEMAND_CACHE,
    };
    use pingora_cache::fill_cancellation::ERR_CACHE_FILL_CANCELLED;
    use pingora_cache::{CacheKey, NoCacheReason};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    const REJECTED: NoCacheReason = NoCacheReason::Custom("rejected request body");

    /// Fails `request_body_filter` with a downstream error, after disabling the cache if
    /// `disables_cache` is set.
    #[derive(Default)]
    struct FailingBodyFilter {
        disables_cache: bool,
        body_filtered: Arc<tokio::sync::Notify>,
        abandonment_calls: Arc<AtomicUsize>,
        downstream_cache_warnings: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl ProxyHttp for FailingBodyFilter {
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
            if self.disables_cache {
                session.cache.disable(REJECTED);
            }
            self.body_filtered.notify_one();
            Error::e_explain(InternalError, "rejected request body").map_err(|e| e.into_down())
        }

        fn downstream_abandoned_during_cache_fill(&self, session: &Session, _ctx: &mut ()) {
            self.abandonment_calls.fetch_add(1, Ordering::Relaxed);
            // As an application whose storage tracks writer demand does.
            session.cache.release_miss_writer_demand();
        }

        fn suppress_proxy_warn_log(
            &self,
            _session: &Session,
            _ctx: &(),
            _error: &Error,
            context: ProxyWarnLogContext,
        ) -> bool {
            if context == ProxyWarnLogContext::DownstreamCache {
                self.downstream_cache_warnings
                    .fetch_add(1, Ordering::Relaxed);
            }
            false
        }
    }

    /// What [`run_failing_body_filter`] saw.
    struct BodyFilterOutcome {
        result: Result<bool>,
        abandonment_calls: usize,
        released_writers: usize,
        /// How many downstream errors the loop ignored so that the cache fill could continue.
        downstream_cache_warnings: usize,
        cache_phase: CachePhase,
        /// The bytes the client received.
        response: Vec<u8>,
    }

    /// Proxies a POST to an HTTP/2 upstream whose `request_body_filter` fails with a downstream
    /// error (see [`FailingBodyFilter`]). With `cache_miss` the request is a cache miss;
    /// otherwise its cache was never enabled. The upstream answers 200 only after that filter ran.
    async fn run_failing_body_filter(
        path: &str,
        handles_abandonment: bool,
        disables_cache: bool,
        cache_miss: bool,
    ) -> BodyFilterOutcome {
        let (mut client, server) = tokio::io::duplex(64 * 1024);
        client
            .write_all(
                format!(
                    "POST {path} HTTP/1.1\r\nHost: example.com\r\nContent-Length: 5\r\n\r\nhello"
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        let mut session = Session::new_h1(Box::new(server) as pingora_core::protocols::Stream);
        session.set_handles_downstream_abandonment(handles_abandonment);
        session.read_request().await.unwrap();
        if cache_miss {
            session
                .cache
                .enable(&*WRITER_DEMAND_CACHE, None, None, None, None);
            session.cache.set_cache_key(CacheKey::new(path, ""));
            session.cache.cache_miss();
        }

        let inner = FailingBodyFilter {
            disables_cache,
            ..Default::default()
        };
        let body_filtered = Arc::clone(&inner.body_filtered);
        let abandonment_calls = Arc::clone(&inner.abandonment_calls);
        let downstream_cache_warnings = Arc::clone(&inner.downstream_cache_warnings);
        let proxy = HttpProxy::new(inner, Arc::new(ServerConf::default()));
        let (response_tx, response_rx) = mpsc::channel(TASK_BUFFER_SIZE);
        let (client_io, _server_io) = tokio::io::duplex(64 * 1024);
        let (mut h2_client, _connection) = h2::client::handshake(client_io).await.unwrap();
        let request = http::Request::builder()
            .method("POST")
            .uri(format!("https://example.com{path}"))
            .body(())
            .unwrap();
        let (_response, mut request_body) = h2_client.send_request(request, false).unwrap();
        let mut custom_writer = None;
        let mut custom_reader = None;
        let mut ctx = ();

        let proxy_loop = proxy.bidirection_down_to_up(
            &mut session,
            &mut request_body,
            response_rx,
            &mut ctx,
            None,
            &mut custom_writer,
            &mut custom_reader,
            Arc::new(AtomicU8::new(PipeState::Active as u8)),
        );
        let upstream = async move {
            body_filtered.notified().await;
            let header = ResponseHeader::build(200, None).unwrap();
            // A loop that has already ended dropped its receiver; its result says why.
            let tasks = [
                HttpTask::Header(Box::new(header), false),
                HttpTask::Body(Some(Bytes::from_static(b"ok")), true),
                HttpTask::Done,
            ];
            for task in tasks {
                if response_tx.send(task).await.is_err() {
                    break;
                }
            }
        };
        let (result, ()) = tokio::time::timeout(Duration::from_secs(5), async {
            tokio::join!(proxy_loop, upstream)
        })
        .await
        .expect("H2 transport loop did not finish");
        let cache_phase = session.cache.phase();
        drop(session);
        let mut response = Vec::new();
        client.read_to_end(&mut response).await.unwrap();
        BodyFilterOutcome {
            result,
            abandonment_calls: abandonment_calls.load(Ordering::Relaxed),
            released_writers: WRITER_DEMAND_CACHE.released_writers(path),
            downstream_cache_warnings: downstream_cache_warnings.load(Ordering::Relaxed),
            cache_phase,
            response,
        }
    }

    // The downstream error is ignored as the cache fill it was sampled for could still write the
    // cache, even though the filter has since disabled that cache, and the response is still
    // relayed. The opted-in run's client is still connected, and a fill whose cache is disabled
    // cannot be abandoned, so it ends the same way as the opted-out run.
    #[tokio::test]
    async fn body_filter_downstream_error_after_disabling_cache_is_ignored() {
        for (path, handles_abandonment) in [
            ("/h2-body-filter-fails-opted-in", true),
            ("/h2-body-filter-fails-opted-out", false),
        ] {
            let outcome = run_failing_body_filter(path, handles_abandonment, true, true).await;
            assert!(!outcome
                .result
                .expect("the ignored downstream error failed the request"));
            assert_eq!(outcome.abandonment_calls, 0);
            assert_eq!(outcome.released_writers, 0);
            assert_eq!(outcome.downstream_cache_warnings, 1);
            assert_eq!(outcome.cache_phase, CachePhase::Disabled(REJECTED));
            assert_client_received_ok(&outcome.response);
        }
    }

    /// Asserts that the client received the upstream's 200 response with its chunked `ok` body.
    fn assert_client_received_ok(response: &[u8]) {
        let response = std::str::from_utf8(response).unwrap();
        assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response:?}");
        assert!(
            response.ends_with("\r\n\r\n2\r\nok\r\n0\r\n\r\n"),
            "{response:?}"
        );
    }

    // A downstream error from `request_body_filter` is ignored for the cache fill, and the
    // uncacheable response still reaches the client. The opted-in run's client is still
    // connected, so it is not abandoned and receives the same response as the opted-out run.
    #[tokio::test]
    async fn body_filter_downstream_error_still_delivers_response() {
        for (path, handles_abandonment) in [
            ("/h2-body-filter-error-opted-in", true),
            ("/h2-body-filter-error-opted-out", false),
        ] {
            let outcome = run_failing_body_filter(path, handles_abandonment, false, true).await;
            assert!(!outcome
                .result
                .expect("the ignored downstream error failed the request"));
            assert_eq!(outcome.abandonment_calls, 0);
            assert_client_received_ok(&outcome.response);
        }
    }

    // Without a cache miss to protect, the same filter failure ends the loop with the filter's
    // downstream error before any response reaches the client.
    #[tokio::test]
    async fn body_filter_downstream_error_without_cache_fails_request() {
        let outcome =
            run_failing_body_filter("/h2-body-filter-fails-no-cache", false, true, false).await;
        let error = outcome
            .result
            .expect_err("the filter error should end the loop");
        assert_eq!(error.etype, InternalError);
        assert_eq!(error.esource, ErrorSource::Downstream);
        assert_eq!(
            outcome.cache_phase,
            CachePhase::Disabled(NoCacheReason::NeverEnabled)
        );
        assert_eq!(outcome.downstream_cache_warnings, 0);
        assert!(outcome.response.is_empty());
    }

    #[tokio::test]
    async fn abandoned_downstream_does_not_wait_for_or_write_custom_messages() {
        let outcome = run_custom_sender_while_client_leaves(
            "/h2-held-custom-message",
            AbandonmentTiming::BeforeAttempt,
            true,
        )
        .await;
        assert!(!outcome.result.unwrap());
        assert!(!outcome.late_message_accepted);
        assert_eq!(outcome.custom_writes, 0);
        assert_eq!(outcome.custom_finishes, 0);
    }

    #[tokio::test]
    async fn abandonment_closes_custom_sender_during_cache_fill() {
        let outcome = run_custom_sender_while_client_leaves(
            "/h2-custom-message-after-admission",
            AbandonmentTiming::AfterAdmission,
            true,
        )
        .await;
        assert!(!outcome.result.unwrap());
        assert!(!outcome.late_message_accepted);
        assert_eq!(outcome.custom_writes, 0);
        assert_eq!(outcome.custom_finishes, 0);
        assert_eq!(outcome.abandonments, 1);
        assert_eq!(outcome.released_writers, 1);
    }

    // The control: without the handling custom messages keep flowing and no demand is released.
    #[tokio::test]
    async fn abandonment_closes_custom_sender_during_cache_fill_without_abandonment_handling() {
        let outcome = run_custom_sender_while_client_leaves(
            "/h2-custom-message-after-admission-ungated",
            AbandonmentTiming::AfterAdmission,
            false,
        )
        .await;
        assert!(!outcome.result.unwrap());
        assert!(outcome.late_message_accepted);
        assert_eq!(outcome.custom_writes, 1);
        assert_eq!(outcome.custom_finishes, 1);
        assert_eq!(outcome.abandonments, 0);
        assert_eq!(outcome.released_writers, 0);
    }

    async fn run_custom_sender_while_client_leaves(
        path: &str,
        abandonment: AbandonmentTiming,
        handles_abandonment: bool,
    ) -> CustomSenderOutcome {
        let (mut session, client) =
            cache_miss_session(path, &abandonment, handles_abandonment).await;
        let (inner, custom_sender, observed) = HeldCustomSenderProxy::new();
        let proxy = HttpProxy::new(inner, Arc::new(ServerConf::default()));
        let calls = Arc::new(CustomWriteCalls::default());
        let mut custom_writer =
            Some(Box::new(RecordingCustomWriter::new(Arc::clone(&calls)))
                as Box<dyn CustomMessageWrite>);
        let mut custom_reader = None;
        let (response_tx, response_rx) = mpsc::channel(TASK_BUFFER_SIZE);
        let mut ctx = ();

        let (client_io, _server_io) = tokio::io::duplex(4096);
        let (mut h2_client, _connection) = h2::client::handshake(client_io).await.unwrap();
        let request = http::Request::builder()
            .uri("https://example.com/")
            .body(())
            .unwrap();
        let (_response, mut request_body) = h2_client.send_request(request, false).unwrap();

        let proxy_loop = proxy.bidirection_down_to_up(
            &mut session,
            &mut request_body,
            response_rx,
            &mut ctx,
            None,
            &mut custom_writer,
            &mut custom_reader,
            Arc::new(AtomicU8::new(PipeState::Active as u8)),
        );
        let drive = drive_custom_sender_while_client_leaves(
            custom_sender,
            client,
            response_tx,
            abandonment,
            true,
            handles_abandonment,
            &observed,
        );

        let (result, (late_message_accepted, _held_sender)) =
            tokio::time::timeout(Duration::from_secs(5), async {
                tokio::join!(proxy_loop, drive)
            })
            .await
            .expect("H2 transport loop stayed open for the application's custom-message sender");

        CustomSenderOutcome {
            result,
            late_message_accepted,
            custom_writes: calls.writes(),
            custom_finishes: calls.finishes(),
            abandonments: observed.abandonments(),
            released_writers: WRITER_DEMAND_CACHE.released_writers(path),
        }
    }

    // A cancellation requested once the abandoned fill is parked on its upstream read wakes the
    // loop.
    #[tokio::test]
    async fn abandoned_fill_is_cancelled_without_another_upstream_task() {
        assert_cancellation_ends_admitted_fill(
            "/h2-cancel-after-abandonment",
            AbandonmentTiming::AfterAdmission,
            false,
        )
        .await;
    }

    // A cancellation requested before the attempt starts ends the fill as soon as the writer
    // exists.
    #[tokio::test]
    async fn fill_cancelled_before_the_attempt_ends_at_admission() {
        assert_cancellation_ends_admitted_fill(
            "/h2-cancel-before-attempt",
            AbandonmentTiming::BeforeAttempt,
            false,
        )
        .await;
    }

    #[tokio::test]
    async fn detached_fill_cancellation_interrupts_body_pacing() {
        assert_cancellation_ends_admitted_fill(
            "/h2-cancel-during-detached-pacing",
            AbandonmentTiming::AfterAdmission,
            true,
        )
        .await;
    }

    async fn assert_cancellation_ends_admitted_fill(
        path: &str,
        abandonment: AbandonmentTiming,
        pace_body: bool,
    ) {
        let (mut session, mut client) =
            cache_miss_session_on(path, &abandonment, true, &*CANCELLATION_WATCHED_STORAGE).await;
        let cancellation = CANCELLATION_WATCHED_STORAGE.cancellation_for(path);
        assert!(!cancellation.is_cancelled(), "{path} was not fresh");
        let cancel_before_attempt = matches!(abandonment, AbandonmentTiming::BeforeAttempt);
        if cancel_before_attempt {
            assert!(cancellation.cancel());
        }
        let inner = AbandonmentSignalProxy {
            body_delay: pace_body.then_some(Duration::from_secs(65)),
            ..Default::default()
        };
        let abandoned = Arc::clone(&inner.abandoned);
        let body_delayed = Arc::clone(&inner.body_delayed);
        let proxy = HttpProxy::new(inner, Arc::new(ServerConf::default()));
        let mut custom_writer = None;
        let mut custom_reader = None;
        let (response_tx, response_rx) = mpsc::channel(TASK_BUFFER_SIZE);
        let mut ctx = ();

        let (client_io, _server_io) = tokio::io::duplex(4096);
        let (mut h2_client, _connection) = h2::client::handshake(client_io).await.unwrap();
        let request = http::Request::builder()
            .uri("https://example.com/")
            .body(())
            .unwrap();
        let (_response, mut request_body) = h2_client.send_request(request, false).unwrap();

        let proxy_loop = proxy.bidirection_down_to_up(
            &mut session,
            &mut request_body,
            response_rx,
            &mut ctx,
            None,
            &mut custom_writer,
            &mut custom_reader,
            Arc::new(AtomicU8::new(PipeState::Active as u8)),
        );
        let drive = async {
            let header = ResponseHeader::build(200, None).unwrap();
            response_tx
                .send(HttpTask::Header(Box::new(header), false))
                .await
                .unwrap();
            if !cancel_before_attempt {
                // Wait for the loop to actually admit and flush the response before cancelling.
                let mut response = Vec::new();
                while !response.ends_with(b"\r\n\r\n") {
                    response.push(client.read_u8().await.unwrap());
                }
                drop(client);
                abandoned.notified().await;
                if pace_body {
                    response_tx
                        .send(HttpTask::Body(Some(Bytes::from_static(b"paced")), false))
                        .await
                        .unwrap();
                    body_delayed.notified().await;
                }
                assert!(cancellation.cancel());
            }
            // Keep the upstream pipe open: only the cancellation may end the loop.
            response_tx
        };

        let (result, _response_tx) = tokio::time::timeout(Duration::from_secs(5), async {
            tokio::join!(proxy_loop, drive)
        })
        .await
        .expect("H2 transport loop kept waiting for upstream after the cancellation");

        let error = result.unwrap_err();
        assert_eq!(error.etype(), &ERR_CACHE_FILL_CANCELLED);
        assert_eq!(error.retry, pingora_error::RetryType::Decided(false));
        assert!(session.downstream_abandoned_during_cache_fill());
        assert!(
            session.cache.is_cache_admission_active(),
            "the loop must leave the writer for the request to drop"
        );
    }

    // Cancellation only interrupts waiting for the upstream: a response that is admitted and
    // completed within one upstream batch is committed even though a cancellation was pending.
    // This exercises the same `start_watching`/`recv_upstream` wiring as the parked-cancellation
    // tests above, but with the whole response already queued before the loop's first poll.
    #[tokio::test]
    async fn batched_complete_response_with_pacing_commits_despite_a_pending_cancellation() {
        let path = "/h2-cancel-before-batched-response";
        let (mut session, _client) = cache_miss_session_on(
            path,
            &AbandonmentTiming::BeforeAttempt,
            true,
            &*CANCELLATION_WATCHED_STORAGE,
        )
        .await;
        assert!(
            !CANCELLATION_WATCHED_STORAGE.is_committed(path).await,
            "{path} was not fresh"
        );
        let cancellation = CANCELLATION_WATCHED_STORAGE.cancellation_for(path);
        assert!(cancellation.cancel(), "{path} was not fresh");
        let proxy = HttpProxy::new(
            AbandonmentSignalProxy {
                body_delay: Some(Duration::from_secs(65)),
                ..Default::default()
            },
            Arc::new(ServerConf::default()),
        );
        let mut custom_writer = None;
        let mut custom_reader = None;
        let (response_tx, response_rx) = mpsc::channel(TASK_BUFFER_SIZE);
        let mut ctx = ();

        let (client_io, _server_io) = tokio::io::duplex(4096);
        let (mut h2_client, _connection) = h2::client::handshake(client_io).await.unwrap();
        let request = http::Request::builder()
            .uri("https://example.com/")
            .body(())
            .unwrap();
        let (_response, mut request_body) = h2_client.send_request(request, false).unwrap();

        let header = ResponseHeader::build(200, None).unwrap();
        response_tx
            .try_send(HttpTask::Header(Box::new(header), false))
            .unwrap();
        response_tx
            .try_send(HttpTask::Body(Some(Bytes::from_static(b"complete")), true))
            .unwrap();
        response_tx.try_send(HttpTask::Done).unwrap();
        drop(response_tx);

        let result = tokio::time::timeout(
            Duration::from_secs(5),
            proxy.bidirection_down_to_up(
                &mut session,
                &mut request_body,
                response_rx,
                &mut ctx,
                None,
                &mut custom_writer,
                &mut custom_reader,
                Arc::new(AtomicU8::new(PipeState::Active as u8)),
            ),
        )
        .await
        .expect("H2 transport loop did not finish the queued response");

        assert!(
            !result.unwrap(),
            "a complete batched response must end the loop normally"
        );
        assert!(
            CANCELLATION_WATCHED_STORAGE.is_committed(path).await,
            "a response that was already complete was discarded for an older cancellation"
        );
    }

    // An HTTP/2 upstream ends a response with `Done` after its end-of-stream body, and the
    // abandoned loop waits for that `Done`. A cancellation that arrives after the admission has
    // already committed, while the loop waits, must not fail the request.
    #[tokio::test]
    async fn cancellation_after_the_admission_committed_does_not_fail_the_request() {
        // Lets the loop run until it waits on the upstream again. Nothing here waits on real
        // I/O: the storage is in memory and the abandoned downstream is never polled.
        async fn let_the_loop_park() {
            for _ in 0..16 {
                tokio::task::yield_now().await;
            }
        }

        let path = "/h2-cancel-after-commit";
        let (mut session, _client) = cache_miss_session_on(
            path,
            &AbandonmentTiming::BeforeAttempt,
            true,
            &*CANCELLATION_WATCHED_STORAGE,
        )
        .await;
        assert!(
            !CANCELLATION_WATCHED_STORAGE.is_committed(path).await,
            "{path} was not fresh"
        );
        let cancellation = CANCELLATION_WATCHED_STORAGE.cancellation_for(path);
        let proxy = HttpProxy::new(
            AbandonmentSignalProxy::default(),
            Arc::new(ServerConf::default()),
        );
        let mut custom_writer = None;
        let mut custom_reader = None;
        let (response_tx, response_rx) = mpsc::channel(TASK_BUFFER_SIZE);
        let mut ctx = ();

        let (client_io, _server_io) = tokio::io::duplex(4096);
        let (mut h2_client, _connection) = h2::client::handshake(client_io).await.unwrap();
        let request = http::Request::builder()
            .uri("https://example.com/")
            .body(())
            .unwrap();
        let (_response, mut request_body) = h2_client.send_request(request, false).unwrap();

        let proxy_loop = proxy.bidirection_down_to_up(
            &mut session,
            &mut request_body,
            response_rx,
            &mut ctx,
            None,
            &mut custom_writer,
            &mut custom_reader,
            Arc::new(AtomicU8::new(PipeState::Active as u8)),
        );
        let drive = async {
            let header = ResponseHeader::build(200, None).unwrap();
            response_tx
                .send(HttpTask::Header(Box::new(header), false))
                .await
                .unwrap();
            response_tx
                .send(HttpTask::Body(Some(Bytes::from_static(b"first ")), false))
                .await
                .unwrap();
            // The admission is now active, so the loop watches its writer.
            let_the_loop_park().await;
            response_tx
                .send(HttpTask::Body(Some(Bytes::from_static(b"last")), true))
                .await
                .unwrap();
            // The admission commits; the loop still waits for the upstream's `Done`.
            let_the_loop_park().await;
            assert!(cancellation.cancel());
            let_the_loop_park().await;
            response_tx.send(HttpTask::Done).await.unwrap();
        };

        let (result, ()) = tokio::time::timeout(Duration::from_secs(5), async {
            tokio::join!(proxy_loop, drive)
        })
        .await
        .expect("H2 transport loop did not finish the committed response");

        assert!(
            !result.expect("a cancellation after the commit failed the request"),
            "a committed response must end the loop normally"
        );
        assert!(CANCELLATION_WATCHED_STORAGE.is_committed(path).await);
    }
}
