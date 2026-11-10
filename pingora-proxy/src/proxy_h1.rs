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
use pingora_cache::CachePhase;
use pingora_core::protocols::http::{
    authority::{raw_target_authority, validate_request_authority},
    custom::CUSTOM_MESSAGE_QUEUE_SIZE,
    v1::common::is_upgrade_req as is_h1_upgrade_req,
};

impl<SV, C, DS> HttpProxy<SV, C, DS>
where
    C: custom::Connector,
    DS: DownstreamSession,
{
    pub(crate) async fn proxy_1to1(
        &self,
        session: &mut Session<DS>,
        client_session: &mut HttpSessionV1,
        peer: &HttpPeer,
        ctx: &mut <SV as ProxyHttp<DS>>::CTX,
    ) -> (bool, bool, Option<Box<Error>>)
    where
        SV: ProxyHttp<DS> + Send + Sync,
        <SV as ProxyHttp<DS>>::CTX: Send + Sync,
    {
        client_session.read_timeout = peer.options.read_timeout;
        client_session.write_timeout = peer.options.write_timeout;

        // phase 2 send to upstream

        let mut req = session.req_header().clone();
        let authority_policy = AuthorityPolicy::from(session.downstream_session.is_custom());
        let downstream_had_host = req.headers.contains_key(http::header::HOST);
        req.set_version(Version::HTTP_11);

        // H2 requires :authority but not Host; most H1 servers expect Host, so synthesize it when
        // sending an H2 downstream request to an H1 upstream.
        if session.req_header().version == Version::HTTP_2 {
            let result = match authority_policy {
                AuthorityPolicy::Standard => set_h1_host_from_authority(&mut req),
                AuthorityPolicy::Custom => set_h1_host_from_authority_if_absent(&mut req),
            };
            if let Err(e) = result {
                return (false, true, Some(e.into_down()));
            }
        }

        if let Err(e) = sanitize_h1_upstream_request(
            &mut req,
            peer.options.http_upstream_request_policy,
            session.req_header().version == Version::HTTP_11,
        ) {
            return (false, true, Some(e.into_down()));
        }

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
                return (false, true, Some(e));
            }
        }

        #[cfg(feature = "early_body_buffer")]
        if session.is_body_buffered() {
            req.remove_header(&header::EXPECT);
        }

        // Reconcile and revalidate after request filters, which can mutate Host, URI, or target.
        if authority_policy.is_standard() {
            if let Err(e) = reconcile_upstream_authority(&mut req) {
                return (false, true, Some(e.into_in()));
            }
            // A filter may delete Host. Restore it for requests that originally carried one or
            // whose URI or absolute-form target has an authority, without adding a new empty Host
            // to Host-less HTTP/1.0 origin-form.
            let target_has_authority = raw_target_authority(req.raw_path()).authority().is_some();
            if downstream_had_host || req.uri.authority().is_some() || target_has_authority {
                if let Err(e) = set_h1_host_from_authority(&mut req) {
                    return (false, true, Some(e.into_in()));
                }
            }
            if let Err(e) = validate_request_authority(&req) {
                // The final filter-produced request is invalid, so classify this as internal.
                return (false, true, Some(e.into_in()));
            }
        }

        // A filter may supply a body for a request that arrived without one.
        #[cfg(feature = "early_body_buffer")]
        let body_empty = if session.is_body_buffered() {
            session.get_buffered_body().is_none_or(Bytes::is_empty)
        } else {
            session.is_body_empty()
        };
        #[cfg(not(feature = "early_body_buffer"))]
        let body_empty = session.is_body_empty();
        if let Err(e) = finalize_h1_upstream_request_framing(&mut req, !body_empty) {
            return (false, true, Some(e));
        }

        session.set_upstream_h1_upgrade_request_status(is_h1_upgrade_req(&req));

        session.upstream_compression.request_filter(&req);

        debug!("Sending header to upstream {:?}", req);

        match client_session.write_request_header(Box::new(req)).await {
            Ok(_) => { /* Continue */ }
            Err(e) => {
                return (false, false, Some(e.into_up()));
            }
        }

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
            Err(e) => return (false, false, Some(e)),
        };

        let (tx_upstream, rx_upstream) = mpsc::channel::<HttpTask>(TASK_BUFFER_SIZE);
        let (tx_downstream, rx_downstream) = mpsc::channel::<HttpTask>(TASK_BUFFER_SIZE);

        session.as_mut().enable_retry_buffering();

        // Shared signal so the upstream half can distinguish an expected task-pipe
        // closure (the downstream half finished and dropped rx) from an unexpected one.
        let pipe_state = Arc::new(AtomicU8::new(PipeState::Active as u8));

        // start bi-directional streaming
        let ret = tokio::try_join!(
            self.proxy_handle_downstream(
                session,
                tx_downstream,
                rx_upstream,
                ctx,
                &mut downstream_custom_message_writer,
                &mut downstream_custom_message_reader,
                pipe_state.clone(),
            ),
            self.proxy_handle_upstream(client_session, tx_upstream, rx_downstream, pipe_state),
        );

        if let Some(custom_session) = session.downstream_session.as_custom_mut() {
            if let Some(downstream_custom_message_writer) = downstream_custom_message_writer {
                match custom_session.restore_custom_message_writer(downstream_custom_message_writer)
                {
                    Ok(_) => { /* continue */ }
                    Err(e) => {
                        return (false, false, Some(e));
                    }
                }
            }
            if let Some(downstream_custom_message_reader) = downstream_custom_message_reader {
                match custom_session.restore_custom_message_reader(downstream_custom_message_reader)
                {
                    Ok(_) => { /* continue */ }
                    Err(e) => {
                        return (false, false, Some(e));
                    }
                }
            }
        }

        match ret {
            Ok((downstream_can_reuse, upstream_can_reuse)) => {
                (downstream_can_reuse, upstream_can_reuse, None)
            }
            Err(e) => (false, false, Some(e)),
        }
    }

    pub(crate) async fn proxy_to_h1_upstream(
        &self,
        session: &mut Session<DS>,
        client_session: &mut HttpSessionV1,
        reused: bool,
        peer: &HttpPeer,
        ctx: &mut <SV as ProxyHttp<DS>>::CTX,
    ) -> (bool, bool, Option<Box<Error>>)
    // (reuse_server, reuse_client, error)
    where
        SV: ProxyHttp<DS> + Send + Sync,
        <SV as ProxyHttp<DS>>::CTX: Send + Sync,
    {
        #[cfg(windows)]
        let raw = client_session.id() as std::os::windows::io::RawSocket;
        #[cfg(unix)]
        let raw = client_session.id();

        let initial_write_pending = client_session.stream().get_write_pending_time();

        if let Err(e) = self
            .inner
            .connected_to_upstream(
                session,
                reused,
                peer,
                raw,
                Some(client_session.digest()),
                ctx,
            )
            .await
        {
            return (false, false, Some(e));
        }

        let (server_session_reuse, client_session_reuse, error) =
            self.proxy_1to1(session, client_session, peer, ctx).await;

        // Record upstream response body bytes received (payload only) for logging consumers.
        let upstream_bytes_total = client_session.body_bytes_received();
        session.set_upstream_body_bytes_received(upstream_bytes_total);

        // Record request body bytes written to the upstream (payload only) for logging consumers.
        // Only HTTP/1.x tracks this; see `Session::upstream_body_bytes_sent`.
        session.set_upstream_body_bytes_sent(client_session.body_bytes_sent());

        // Record upstream write pending time for this session only (delta from baseline).
        let current_write_pending = client_session.stream().get_write_pending_time();
        let upstream_write_pending = current_write_pending.saturating_sub(initial_write_pending);
        session.set_upstream_write_pending_time(upstream_write_pending);

        (server_session_reuse, client_session_reuse, error)
    }

    async fn proxy_handle_upstream(
        &self,
        client_session: &mut HttpSessionV1,
        tx: mpsc::Sender<HttpTask>,
        mut rx: mpsc::Receiver<HttpTask>,
        pipe_state: Arc<AtomicU8>,
    ) -> Result<bool>
    where
        SV: ProxyHttp<DS> + Send + Sync,
        <SV as ProxyHttp<DS>>::CTX: Send + Sync,
    {
        let mut request_done = false;
        let mut response_done = false;
        let mut upstream_can_reuse = true;
        let mut send_error = None;
        let mut upgraded = false;

        /* duplex mode, wait for either to complete */
        while !request_done || !response_done {
            tokio::select! {
                res = client_session.read_response_task(), if !response_done => {
                    match res {
                        Ok(task) => {
                            response_done = task.is_end();
                            if !upgraded && client_session.was_upgraded() {
                                // upgrade can only happen once
                                upgraded = true;
                                if send_error.is_none() {
                                    // continue receiving from downstream after body mode change
                                    request_done = false;
                                }
                            }
                            let type_str = task.type_str();
                            let result = tx.send(task)
                                .await.or_err_with(
                                    InternalError,
                                    || format!("Failed to send upstream task {type_str}{} to pipe",
                                        if response_done { " (end)" } else {""})
                                );
                            // If the request is upgraded, the downstream pipe can early exit
                            // when the downstream connection is closed.
                            // In that case, this function should ignore that the pipe is closed.
                            // So that this function could read the rest events from rx including
                            // the closure, then exit.
                            if result.is_err() && !client_session.was_upgraded() {
                                if PipeState::is_downstream_complete(
                                    pipe_state.load(Ordering::Acquire),
                                ) {
                                    // The downstream half finished the response by its own framing
                                    // and dropped the task pipe, so this send failure is benign.
                                    if response_done {
                                        // The whole upstream response was already read off the
                                        // socket. Keep looping so the request side can finish; the
                                        // natural loop exit then decides reuse from both directions
                                        // (the connection is reusable only if the request was also
                                        // fully sent, which a premature response can leave undone).
                                        continue;
                                    }
                                    // We stopped mid-response, so the upstream connection has
                                    // unread bytes and is unsafe to reuse.
                                    return Ok(false);
                                }
                                // The pipe closed but the downstream half did not signal completion:
                                // this is an unexpected closure, so surface the original send error.
                                return Err(result.expect_err("send failure already checked via is_err() above"));
                            }
                        },
                        Err(e) => {
                            // Push the error to downstream and then quit
                            // Don't care if send fails: downstream already gone
                            let _ = tx.send(HttpTask::Failed(send_error.unwrap_or(e).into_up())).await;
                            // A response read error means the HTTP/1 message boundary was not
                            // established successfully, so the connection cannot be reused.
                            return Ok(false)
                        }
                    }
                },

                body = rx.recv(), if !request_done => {
                    // The downstream half only marks tasks `UpgradedBody` once it has observed
                    // the upgrade, so the end of the upgrade request's own body is a plain
                    // `Body` task. When the upstream's 101 is read before that task is consumed,
                    // it arrives with `was_upgraded()` already true and must not be mistaken for
                    // the end of the tunnel.
                    let ends_tunnel = !matches!(body, Some(HttpTask::Body(..)));
                    match send_body_to1(client_session, body).await {
                        Ok(send_done) => {
                            request_done = send_done;
                            // An upgraded request is terminated when either side is done
                            if request_done && client_session.was_upgraded() {
                                if ends_tunnel {
                                    response_done = true;
                                } else {
                                    // Keep streaming the downstream half of the tunnel.
                                    request_done = false;
                                }
                            }
                        },
                        Err(e) => {
                           warn!("send error, draining read buf: {e}");
                           request_done = true;

                           // Built-in HTTP downstream sessions are expected to reject
                           // incomplete bodies before reaching this state. A downstream
                           // session that does not report such an error or an upstream request
                           // mutation that creates inconsistent framing can still reach it.
                           // A complete response can be forwarded, but the partially written
                           // HTTP/1 request makes this connection unsafe to reuse.
                           upstream_can_reuse = false;
                           send_error = Some(e);
                           continue
                        }
                    }
                },

                else => {
                    // this shouldn't be reached as the while loop would already exit
                    break;
                }
            }
        }

        Ok(upstream_can_reuse)
    }

    #[allow(clippy::too_many_arguments)]
    async fn process_upstream_tasks(
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
            // The HTTP/1 producer stops reading at its first `is_end` task.
            response_state.maybe_set_upstream_producer_done(initial_task.is_end());
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
            debug!("upstream event now: {:?}", maybe_task);
            if let Some(t) = maybe_task {
                tasks.push(t);
            } else {
                break; // upstream closed
            }
        }
        response_state.maybe_set_upstream_producer_done(tasks.iter().any(HttpTask::is_end));

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
            let task = self
                .h1_response_filter(session, t, ctx, serve_from_cache, range_body_filter, false)
                .await?;
            if serve_from_cache.is_miss_header() {
                response_state.enable_cached_response();
            }
            // check error and abort
            // otherwise the error is surfaced via write_response_tasks()
            if !serve_from_cache
                .should_write_response(session.downstream_abandoned_during_cache_fill())
            {
                if let HttpTask::Failed(e) = task {
                    return Err(e);
                }
                if session.downstream_abandoned_during_cache_fill() {
                    if serve_from_cache.abandoned_before_admission(&session.cache) {
                        return Err(downstream_closed_before_cache_admission());
                    }
                    continue;
                }
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

    // todo use this function to replace bidirection_1to2()
    // returns whether this server (downstream) session can be reused
    #[allow(clippy::too_many_arguments)]
    async fn proxy_handle_downstream(
        &self,
        session: &mut Session<DS>,
        tx: mpsc::Sender<HttpTask>,
        mut rx: mpsc::Receiver<HttpTask>,
        ctx: &mut <SV as ProxyHttp<DS>>::CTX,
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

        let (request_body_finished, buffer) = session.upstream_body_source();
        let mut downstream_state = DownstreamStateMachine::for_proxy_attempt(
            request_body_finished,
            session.downstream_abandoned_during_cache_fill(),
        );

        // Keep is_body_empty() last: it initializes the H1 reader and must stay lazy when buffered.
        #[cfg(feature = "early_body_buffer")]
        let should_send_initial_body =
            buffer.is_some() || session.is_body_buffered() || session.as_mut().is_body_empty();
        #[cfg(not(feature = "early_body_buffer"))]
        let should_send_initial_body = buffer.is_some() || session.as_mut().is_body_empty();

        // Send the final task for buffered empty bodies so H1 chunked framing is terminated.
        if should_send_initial_body {
            let send_permit = tx
                .reserve()
                .await
                .or_err(InternalError, "reserving body pipe")?;
            self.send_body_to_pipe(session, buffer, request_body_finished, send_permit, ctx)
                .await?;
        }

        let mut response_state = ResponseStateMachine::new();

        // these two below can be wrapped into an internal ctx
        // use cache when upstream revalidates (or TODO: error)
        let mut serve_from_cache = proxy_cache::ServeFromCache::new();
        let mut range_body_filter = proxy_cache::range_filter::RangeBodyFilter::new();

        let mut next_upstream_task: Option<HttpTask> = None;
        let mut fill_cancellation = CacheFillCancellationState::new();

        /* duplex mode without caching
         * Read body from downstream while reading response from upstream
         * If response is done, only read body from downstream
         * If request is done, read response from upstream while idling downstream (to close quickly)
         * If both are done, quit the loop
         *
         * With caching + but without partial read support
         * Similar to above, cache admission write happen when the data is write to downstream
         *
         * With caching + partial read support
         * A. Read upstream response and write to cache
         * B. Read data from cache and send to downstream
         * If B fails (usually downstream close), continue A.
         * If A fails, exit with error.
         * If both are done, quit the loop
         * Usually there is no request body to read for cacheable request
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
            // reserve tx capacity ahead to avoid deadlock, see below

            let send_permit = tx
                .try_reserve()
                .or_err(InternalError, "try_reserve() body pipe for upstream");

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

            tokio::select! {
                // only try to send to pipe if there is capacity to avoid deadlock
                // Otherwise deadlock could happen if both upstream and downstream are blocked
                // on sending to their corresponding pipes which are both full.
                body = session.downstream_session.read_body_or_idle(downstream_state.is_done()),
                    if downstream_state.can_poll() && send_permit.is_ok() => {

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
                    // If the request is websocket, `None` body means the request is closed.
                    // Set the response to be done as well so that the request completes normally.
                    if body.is_none() && session.was_upgraded() {
                        response_state.maybe_set_upstream_delivery_done(true);
                    }
                    // TODO: consider just drain this if serve_from_cache is set
                    let is_body_done = session.is_body_done();
                    let request_done = self.send_body_to_pipe(
                        session,
                        body,
                        is_body_done,
                        send_permit.unwrap(), // safe because we checked is_ok()
                        ctx,
                    )
                    .await?;
                    downstream_state.maybe_finished(request_done);
                },

                _ = tx.reserve(), if downstream_state.is_reading() && send_permit.is_err() => {
                    // If tx is closed, the upstream has already finished its job.
                    downstream_state.maybe_finished(tx.is_closed());
                    debug!("waiting for permit {send_permit:?}, upstream closed {}", tx.is_closed());
                     /* No permit, wait on more capacity to avoid starving.
                     * Otherwise this select only blocks on rx, which might send no data
                     * before the entire body is uploaded.
                     * once more capacity arrives we just loop back
                     */
                },

                // Handle buffered upstream task from previous iteration
                task = async { next_upstream_task.take() }, if next_upstream_task.is_some() => {
                    debug!("buffered upstream event: {:?}", task);
                    if let Some(t) = task {
                        let Some(response_done) = self.process_upstream_tasks(
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
                        response_state.maybe_set_upstream_delivery_done(response_done);
                        // unsuccessful upgrade response may force the request done
                        downstream_state.maybe_finished(session.is_body_done());
                    } else {
                        debug!("empty upstream event");
                        response_state.maybe_set_upstream_delivery_done(true);
                    }
                },

                task = fill_cancellation.recv_upstream(&mut rx), if session.should_poll_upstream_tasks(&response_state) && next_upstream_task.is_none() => {
                    let task = task?;
                    debug!("upstream event: {:?}", task);
                    if let Some(t) = task {
                        let upgraded = session.was_upgraded();
                        let Some(response_done) = self.process_upstream_tasks(
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
                        if !upgraded && session.was_upgraded() && downstream_state.can_poll() {
                            // TODO: write can happen async now
                            // just upgraded, the downstream state should be reset to continue to
                            // poll body
                            trace!("reset downstream state on upgrade");
                            downstream_state.reset();
                        }
                        response_state.maybe_set_upstream_delivery_done(response_done);
                        // unsuccessful upgrade response (or end of upstream upgraded conn,
                        // which forces the body reader to complete) may force the request done
                        downstream_state.maybe_finished(session.is_body_done());
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

                    let task = self.h1_response_filter(session, task?, ctx,
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
                        // being set and here; disable() drops the enabled_ctx so
                        // finish_hit_handler would panic without this guard.
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
                                    // This will not be treated as a final error, but we should signal to
                                    // downstream session regardless
                                    session.downstream_session.on_proxy_failure(e);
                                } else {
                                    return Err(e);
                                }
                            }
                        }

                        // Also poll for upstream tasks - if we get one, cancel the write and handle it.
                        // Only poll if there is no buffered task already waiting to be processed.
                        upstream_task = rx.recv(), if !response_state.upstream_delivery_done() && serve_from_cache.is_on() && next_upstream_task.is_none() => {
                            if let Some(t) = upstream_task {
                                // Store this upstream task to be processed next iteration
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

    async fn h1_response_filter(
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
        // skip caching if already served from cache
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

            if let HttpTask::Header(header, _) = &task {
                reject_mismatched_h1_upgrade_101(session, header, "h1_upstream_filter")
                    .map_err(|e| e.into_up())?;
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
            HttpTask::Header(mut header, end) => {
                /* Downstream revalidation/range, only needed when cache modified headers because otherwise origin
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

                // TODO: just set version to Version::HTTP_11 unconditionally here,
                // (with another todo being an option to faithfully proxy the <1.1 responses)
                // as we are already trying to mutate this for HTTP/1.1 downstream reuse

                /* Convert HTTP 1.0 style response to chunked encoding so that we don't
                 * have to close the downstream connection */
                // these status codes / method cannot have body, so no need to add chunked encoding
                let no_body = session.req_header().method == http::method::Method::HEAD
                    || matches!(header.status.as_u16(), 204 | 304);
                if !no_body
                    && !header.status.is_informational()
                    && header
                        .headers
                        .get(http::header::TRANSFER_ENCODING)
                        .is_none()
                    && header.headers.get(http::header::CONTENT_LENGTH).is_none()
                    && !end
                {
                    // Upgrade the http version to 1.1 because 1.0/0.9 doesn't support chunked
                    header.set_version(Version::HTTP_11);
                    header.insert_header(http::header::TRANSFER_ENCODING, "chunked")?;
                }

                self.inner
                    .response_filter(session, &mut header, ctx)
                    .await?;
                if !from_cache {
                    // Re-check after response_filter in case it changed the final status to 101.
                    reject_mismatched_h1_upgrade_101(session, &header, "h1_response_filter")
                        .map_err(|e| e.into_in())?;
                }
                Ok(HttpTask::Header(header, end))
            }
            HttpTask::Body(data, end) => {
                if track_max_cache_size {
                    session
                        .cache
                        .track_body_bytes_for_max_file_size(data.as_ref().map_or(0, |d| d.len()));
                }

                // before it can mark it as cacheable again.
                let mut data = range_body_filter.filter_body(data);
                if let Some(duration) = self
                    .inner
                    .response_body_filter(session, &mut data, end, ctx)
                    .await?
                {
                    trace!("delaying downstream response for {:?}", duration);
                    time::sleep(duration).await;
                }

                Ok(HttpTask::Body(data, end))
            }
            HttpTask::UpgradedBody(mut data, end) => {
                if track_max_cache_size {
                    session
                        .cache
                        .track_body_bytes_for_max_file_size(data.as_ref().map_or(0, |d| d.len()));
                }

                // range doesn't apply to upgraded body
                if let Some(duration) = self
                    .inner
                    .response_body_filter(session, &mut data, end, ctx)
                    .await?
                {
                    trace!("delaying downstream upgraded response for {:?}", duration);
                    time::sleep(duration).await;
                }

                Ok(HttpTask::UpgradedBody(data, end))
            }
            HttpTask::Trailer(h) => Ok(HttpTask::Trailer(h)), // TODO: support trailers for h1
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

    // TODO:: use this function to replace send_body_to2
    async fn send_body_to_pipe(
        &self,
        session: &mut Session<DS>,
        mut data: Option<Bytes>,
        end_of_body: bool,
        tx: mpsc::Permit<'_, HttpTask>,
        ctx: &mut <SV as ProxyHttp<DS>>::CTX,
    ) -> Result<bool>
    where
        SV: ProxyHttp<DS> + Send + Sync,
        <SV as ProxyHttp<DS>>::CTX: Send + Sync,
    {
        // None: end of body
        // this var is to signal if downstream finish sending the body, which shouldn't be
        // affected by the request_body_filter
        let end_of_body = end_of_body || data.is_none();

        session
            .downstream_modules_ctx
            .request_body_filter(&mut data, end_of_body)
            .await?;

        // TODO: request body filter to have info about upgraded status?
        // (can also check session.was_upgraded())
        self.inner
            .request_body_filter(session, &mut data, end_of_body, ctx)
            .await?;

        // the flag to signal to upstream
        let upstream_end_of_body = end_of_body || data.is_none();

        /* It is normal to get 0 bytes because of multi-chunk or request_body_filter decides not to
         * output anything yet.
         * Don't write 0 bytes to the network since it will be
         * treated as the terminating chunk */
        if !upstream_end_of_body && data.as_ref().is_some_and(|d| d.is_empty()) {
            return Ok(false);
        }

        debug!(
            "Read {} bytes body from downstream",
            data.as_ref().map_or(-1, |d| d.len() as isize)
        );

        // upgraded body needs to be marked
        if session.was_upgraded() {
            tx.send(HttpTask::UpgradedBody(data, upstream_end_of_body));
        } else {
            tx.send(HttpTask::Body(data, upstream_end_of_body));
        }

        Ok(end_of_body)
    }
}

pub(crate) async fn send_body_to1(
    client_session: &mut HttpSessionV1,
    recv_task: Option<HttpTask>,
) -> Result<bool> {
    let body_done;

    if let Some(task) = recv_task {
        match task {
            HttpTask::Body(data, end) => {
                body_done = end;
                if let Some(d) = data {
                    let m = client_session.write_body(&d).await;
                    match m {
                        Ok(m) => match m {
                            Some(n) => {
                                debug!("Write {} bytes body to upstream", n);
                            }
                            None => {
                                warn!("Upstream body is already finished. Nothing to write");
                            }
                        },
                        Err(e) => {
                            return e.into_up().into_err();
                        }
                    }
                }
            }
            HttpTask::UpgradedBody(data, end) => {
                client_session.maybe_upgrade_body_writer();

                body_done = end;
                if let Some(d) = data {
                    let m = client_session.write_body(&d).await;
                    match m {
                        Ok(m) => {
                            match m {
                                Some(n) => {
                                    debug!("Write {} bytes upgraded body to upstream", n);
                                }
                                None => {
                                    warn!("Upstream upgraded body is already finished. Nothing to write");
                                }
                            }
                        }
                        Err(e) => {
                            return e.into_up().into_err();
                        }
                    }
                }
            }
            _ => {
                // should never happen, sender only sends body
                warn!("Unexpected task sent to upstream");
                body_done = true;
                // error here,
                // for client sessions that received upgrade but didn't
                // receive any UpgradedBody,
                // no more data is arriving so we should consider this
                // as downstream finalizing its upgrade payload
                client_session.maybe_upgrade_body_writer();
            }
        }
    } else {
        // sender dropped
        body_done = true;
        // for client sessions that received upgrade but didn't
        // receive any UpgradedBody,
        // no more data is arriving so we should consider this
        // as downstream finalizing its upgrade payload
        client_session.maybe_upgrade_body_writer();
    }

    if body_done {
        match client_session.finish_body().await {
            Ok(_) => {
                debug!("finish sending body to upstream");
                Ok(true)
            }
            Err(e) => e.into_up().into_err(),
        }
    } else {
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{
        cache_miss_session, drive_custom_sender_while_client_leaves, AbandonmentTiming,
        CustomSenderOutcome, CustomWriteCalls, HeldCustomSenderProxy, RecordingCustomWriter,
        WRITER_DEMAND_CACHE,
    };
    use crate::test_support::{
        cache_miss_session_on, AbandonmentSignalProxy, CANCELLATION_WATCHED_STORAGE,
    };
    use pingora_cache::fill_cancellation::ERR_CACHE_FILL_CANCELLED;
    use std::sync::Arc;
    use tokio::io::AsyncReadExt;
    use tokio::io::AsyncWriteExt;
    use tokio::time::{timeout, Duration};

    struct ResponseFilter101;

    #[async_trait]
    impl ProxyHttp for ResponseFilter101 {
        type CTX = ();

        fn new_ctx(&self) -> Self::CTX {}

        async fn upstream_peer(
            &self,
            _session: &mut Session,
            _ctx: &mut Self::CTX,
        ) -> Result<Box<HttpPeer>> {
            unreachable!("test calls h1_response_filter directly")
        }

        async fn response_filter(
            &self,
            _session: &mut Session,
            response: &mut ResponseHeader,
            _ctx: &mut Self::CTX,
        ) -> Result<()> {
            response.set_status(http::StatusCode::SWITCHING_PROTOCOLS)?;
            response.set_version(Version::HTTP_11);
            Ok(())
        }
    }

    async fn upgrade_request_session() -> Session {
        let (mut client, server) = tokio::io::duplex(1024);
        client
            .write_all(
                b"GET / HTTP/1.1\r\nHost: example.com\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\n",
            )
            .await
            .expect("test request should be written");

        let mut session = Session::new_h1(Box::new(server) as pingora_core::protocols::Stream);
        session
            .read_request()
            .await
            .expect("test request should parse");
        session
    }

    /// The upgrade request itself carries no body, so the downstream half sends a single
    /// `Body(None, true)`. If the upstream's 101 is read before that task is consumed, the task
    /// arrives after the session is already upgraded; treating it as the end of the tunnel tore
    /// the connection down right after a successful upgrade.
    #[tokio::test]
    async fn upgraded_request_survives_body_end_queued_before_the_101() {
        let proxy = HttpProxy::new(ResponseFilter101, Arc::new(ServerConf::default()));

        let (mut upstream_peer, upstream_stream) = tokio::io::duplex(4096);
        let mut client_session =
            HttpSessionV1::new(Box::new(upstream_stream) as pingora_core::protocols::Stream);

        let mut req = RequestHeader::build("GET", b"/socket", None).unwrap();
        req.insert_header("Host", "example.com").unwrap();
        req.insert_header("Connection", "Upgrade").unwrap();
        req.insert_header("Upgrade", "websocket").unwrap();
        client_session
            .write_request_header(Box::new(req))
            .await
            .expect("test request should be written");

        // The upstream accepts the upgrade right away, so its 101 wins the race against the
        // proxy's own processing of the (empty) request body.
        upstream_peer
            .write_all(
                b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\n",
            )
            .await
            .expect("test response should be written");

        let (tx_upstream, _rx_upstream) = mpsc::channel(10);
        let (tx_downstream, rx_downstream) = mpsc::channel(10);

        tokio::spawn(async move {
            // The end of the upgrade request's own body, queued while the 101 was being read.
            tokio::time::sleep(Duration::from_millis(50)).await;
            let _ = tx_downstream.send(HttpTask::Body(None, true)).await;
            // The client's first frame on the established tunnel.
            tokio::time::sleep(Duration::from_millis(50)).await;
            let _ = tx_downstream
                .send(HttpTask::UpgradedBody(
                    Some(Bytes::from_static(b"ping")),
                    false,
                ))
                .await;
            // The client closes its half of the tunnel, which does end it.
            tokio::time::sleep(Duration::from_millis(50)).await;
            drop(tx_downstream);
        });

        let pipe_state = Arc::new(AtomicU8::new(PipeState::Active as u8));
        timeout(
            Duration::from_secs(5),
            proxy.proxy_handle_upstream(
                &mut client_session,
                tx_upstream,
                rx_downstream,
                pipe_state,
            ),
        )
        .await
        .expect("proxy_handle_upstream should not hang")
        .expect("proxy_handle_upstream should not fail");

        let mut received = Vec::new();
        let mut buf = [0u8; 256];
        while let Ok(Ok(n)) =
            timeout(Duration::from_millis(200), upstream_peer.read(&mut buf)).await
        {
            if n == 0 {
                break;
            }
            received.extend_from_slice(&buf[..n]);
            if received.ends_with(b"ping") {
                break;
            }
        }
        let received = String::from_utf8_lossy(&received).into_owned();
        assert!(
            received.ends_with("ping"),
            "the tunnel should still forward the client's frames, upstream saw: {received:?}"
        );
    }

    #[tokio::test]
    async fn h1_response_filter_rejects_filter_created_101_with_upgrade_mismatch() {
        let proxy = HttpProxy::new(ResponseFilter101, Arc::new(ServerConf::default()));
        let mut session = upgrade_request_session().await;
        session.h1_upgrade_request_status = H1UpgradeRequestStatus {
            upstream: Some(false),
        };

        let mut ctx = ();
        let mut serve_from_cache = ServeFromCache::new();
        let mut range_body_filter = RangeBodyFilter::new();
        let task = HttpTask::Header(Box::new(ResponseHeader::build(200, Some(0)).unwrap()), true);

        let err = proxy
            .h1_response_filter(
                &mut session,
                task,
                &mut ctx,
                &mut serve_from_cache,
                &mut range_body_filter,
                false,
            )
            .await
            .unwrap_err();

        assert_eq!(err.etype(), &InvalidHTTPHeader);
        assert_eq!(err.esource(), &ErrorSource::Internal);
    }

    #[tokio::test]
    async fn h1_response_filter_rejects_upstream_101_upgrade_mismatch_as_upstream() {
        let proxy = HttpProxy::new(ResponseFilter101, Arc::new(ServerConf::default()));
        let mut session = upgrade_request_session().await;
        session.h1_upgrade_request_status = H1UpgradeRequestStatus {
            upstream: Some(false),
        };

        let mut ctx = ();
        let mut serve_from_cache = ServeFromCache::new();
        let mut range_body_filter = RangeBodyFilter::new();
        let mut response =
            ResponseHeader::build(http::StatusCode::SWITCHING_PROTOCOLS, Some(0)).unwrap();
        response.set_version(Version::HTTP_11);
        let task = HttpTask::Header(Box::new(response), true);

        let err = proxy
            .h1_response_filter(
                &mut session,
                task,
                &mut ctx,
                &mut serve_from_cache,
                &mut range_body_filter,
                false,
            )
            .await
            .unwrap_err();

        assert_eq!(err.etype(), &InvalidHTTPHeader);
        assert_eq!(err.esource(), &ErrorSource::Upstream);
    }

    #[tokio::test]
    async fn abandoned_downstream_does_not_wait_for_or_write_custom_messages() {
        let outcome = run_custom_sender_while_client_leaves(
            "/h1-held-custom-message",
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
            "/h1-custom-message-after-admission",
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
            "/h1-custom-message-after-admission-ungated",
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
        let (request_tx, _request_rx) = mpsc::channel(TASK_BUFFER_SIZE);
        let (response_tx, response_rx) = mpsc::channel(TASK_BUFFER_SIZE);
        let mut ctx = ();

        let proxy_loop = proxy.proxy_handle_downstream(
            &mut session,
            request_tx,
            response_rx,
            &mut ctx,
            &mut custom_writer,
            &mut custom_reader,
            Arc::new(AtomicU8::new(PipeState::Active as u8)),
        );
        let drive = drive_custom_sender_while_client_leaves(
            custom_sender,
            client,
            response_tx,
            abandonment,
            false,
            handles_abandonment,
            &observed,
        );

        let (result, (late_message_accepted, _held_sender)) =
            tokio::time::timeout(Duration::from_secs(5), async {
                tokio::join!(proxy_loop, drive)
            })
            .await
            .expect("H1 transport loop stayed open for the application's custom-message sender");

        CustomSenderOutcome {
            result,
            late_message_accepted,
            custom_writes: calls.writes(),
            custom_finishes: calls.finishes(),
            abandonments: observed.abandonments(),
            released_writers: WRITER_DEMAND_CACHE.released_writers(path),
        }
    }

    // A cancellation requested once the abandoned fill is parked on its upstream read wakes the loop.
    #[tokio::test]
    async fn abandoned_fill_is_cancelled_without_another_upstream_task() {
        assert_cancellation_ends_admitted_fill(
            "/h1-cancel-after-abandonment",
            AbandonmentTiming::AfterAdmission,
            false,
        )
        .await;
    }

    // A cancellation requested before the attempt starts ends the fill as soon as the writer exists.
    #[tokio::test]
    async fn fill_cancelled_before_the_attempt_ends_at_admission() {
        assert_cancellation_ends_admitted_fill(
            "/h1-cancel-before-attempt",
            AbandonmentTiming::BeforeAttempt,
            false,
        )
        .await;
    }

    #[tokio::test]
    async fn detached_fill_cancellation_interrupts_body_pacing() {
        assert_cancellation_ends_admitted_fill(
            "/h1-cancel-during-detached-pacing",
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
        let (request_tx, _request_rx) = mpsc::channel(TASK_BUFFER_SIZE);
        let (response_tx, response_rx) = mpsc::channel(TASK_BUFFER_SIZE);
        let mut ctx = ();

        let proxy_loop = proxy.proxy_handle_downstream(
            &mut session,
            request_tx,
            response_rx,
            &mut ctx,
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
        .expect("H1 transport loop kept waiting for upstream after the cancellation");

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
    #[tokio::test]
    async fn batched_complete_response_with_pacing_commits_despite_a_pending_cancellation() {
        let header = ResponseHeader::build(200, None).unwrap();
        let body = HttpTask::Body(Some(Bytes::from_static(b"complete")), true);
        assert_batched_response_commits(
            "/h1-cancel-before-batched-header-and-body",
            vec![HttpTask::Header(Box::new(header), false), body],
        )
        .await;

        let header = ResponseHeader::build(204, None).unwrap();
        assert_batched_response_commits(
            "/h1-cancel-before-batched-bodyless-header",
            vec![HttpTask::Header(Box::new(header), true)],
        )
        .await;
    }

    /// Run the transport loop for an abandoned request with a pending cancellation whose whole
    /// upstream response is already queued, so the writer is created and finished in one
    /// `process_upstream_tasks` call.
    async fn assert_batched_response_commits(path: &str, response: Vec<HttpTask>) {
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
        let (request_tx, _request_rx) = mpsc::channel(TASK_BUFFER_SIZE);
        let (response_tx, response_rx) = mpsc::channel(TASK_BUFFER_SIZE);
        for task in response {
            response_tx.try_send(task).unwrap();
        }
        drop(response_tx);
        let mut ctx = ();

        let result = tokio::time::timeout(
            Duration::from_secs(5),
            proxy.proxy_handle_downstream(
                &mut session,
                request_tx,
                response_rx,
                &mut ctx,
                &mut custom_writer,
                &mut custom_reader,
                Arc::new(AtomicU8::new(PipeState::Active as u8)),
            ),
        )
        .await
        .expect("H1 transport loop did not finish the queued response");

        assert!(
            !result.unwrap(),
            "a complete batched response must end the loop normally"
        );
        assert!(
            CANCELLATION_WATCHED_STORAGE.is_committed(path).await,
            "a response that was already complete was discarded for an older cancellation"
        );
    }

    // The final task can arrive while the loop is mid-batch: after an earlier poll has already
    // taken the header out of the channel, a later poll finds a new final task and a pending
    // cancellation ready at the same time. `should_write_response` gates every downstream write
    // on the downstream not being abandoned, so a cancellation is only honored at all once the
    // downstream is abandoned - this drives no output to `client`; a capacity-1 response channel
    // supplies the synchronisation instead of a socket read.
    #[tokio::test]
    async fn queued_final_task_wins_a_cancellation_ready_in_the_same_poll() {
        let path = "/h1-cancel-races-a-mid-batch-final-task";
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
        assert!(!cancellation.is_cancelled(), "{path} was not fresh");
        let proxy = HttpProxy::new(
            AbandonmentSignalProxy::default(),
            Arc::new(ServerConf::default()),
        );
        let mut custom_writer = None;
        let mut custom_reader = None;
        let (request_tx, _request_rx) = mpsc::channel(TASK_BUFFER_SIZE);
        // Capacity 1: the second `send` below can only complete once the loop has taken the
        // header out of the channel, proving the loop is past the first batch and back at
        // `recv_upstream` for the next poll.
        let (response_tx, response_rx) = mpsc::channel(1);
        let mut ctx = ();

        let proxy_loop = proxy.proxy_handle_downstream(
            &mut session,
            request_tx,
            response_rx,
            &mut ctx,
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

            // Blocks until the loop consumes the header, freeing the one buffer slot.
            response_tx
                .send(HttpTask::Body(Some(Bytes::from_static(b"complete")), true))
                .await
                .unwrap();

            // No `.await` between the send above resolving and this call: the final task is
            // already in the channel before the cancellation becomes ready. `recv_upstream`
            // counts the queue when it first observes the cancellation, so the task sits
            // inside that snapshot and the loop delivers it instead of failing the fill.
            assert!(cancellation.cancel());

            response_tx
        };

        let (result, _response_tx) = tokio::time::timeout(Duration::from_secs(5), async {
            tokio::join!(proxy_loop, drive)
        })
        .await
        .expect("H1 transport loop did not finish the mid-batch response");

        assert!(
            !result.unwrap(),
            "a task queued before the loop's next poll must win over a cancellation ready at the same time"
        );
        assert!(
            CANCELLATION_WATCHED_STORAGE.is_committed(path).await,
            "a response whose final task beat the cancellation to the same poll was discarded"
        );
    }

    // Without the opt-in the proxy never watches for a cancellation: an application that
    // cancels anyway sees its fill run to completion, as before cancellation existed.
    #[tokio::test]
    async fn cancellation_is_never_watched_without_the_opt_in() {
        let path = "/h1-cancel-without-opt-in";
        let (mut session, mut client) = cache_miss_session_on(
            path,
            &AbandonmentTiming::AfterAdmission,
            true,
            &*CANCELLATION_WATCHED_STORAGE,
        )
        .await;
        session.set_handles_downstream_abandonment(false);
        assert!(
            !CANCELLATION_WATCHED_STORAGE.is_committed(path).await,
            "{path} was not fresh"
        );
        let cancellation = CANCELLATION_WATCHED_STORAGE.cancellation_for(path);
        assert!(!cancellation.is_cancelled(), "{path} was not fresh");
        let inner = AbandonmentSignalProxy::default();
        let abandoned = Arc::clone(&inner.abandoned);
        let proxy = HttpProxy::new(inner, Arc::new(ServerConf::default()));
        let mut custom_writer = None;
        let mut custom_reader = None;
        let (request_tx, _request_rx) = mpsc::channel(TASK_BUFFER_SIZE);
        let (response_tx, response_rx) = mpsc::channel(TASK_BUFFER_SIZE);
        let mut ctx = ();

        let proxy_loop = proxy.proxy_handle_downstream(
            &mut session,
            request_tx,
            response_rx,
            &mut ctx,
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
            let mut response = Vec::new();
            while !response.ends_with(b"\r\n\r\n") {
                response.push(client.read_u8().await.unwrap());
            }
            drop(client);
            assert!(cancellation.cancel());
            response_tx
                .send(HttpTask::Body(Some(Bytes::from_static(b"complete")), true))
                .await
                .unwrap();
            // The upstream side closes its channel after the final task, like a finished
            // upstream: without the opt-in the loop waits for that once the downstream failed.
            drop(response_tx);
        };

        let (result, ()) = tokio::time::timeout(Duration::from_secs(5), async {
            tokio::join!(proxy_loop, drive)
        })
        .await
        .expect("H1 transport loop did not finish a fill that did not opt in");

        if let Err(error) = &result {
            assert_ne!(
                error.etype(),
                &ERR_CACHE_FILL_CANCELLED,
                "a request that did not opt in was cancelled"
            );
        }
        assert!(!session.downstream_abandoned_during_cache_fill());
        assert!(
            tokio::time::timeout(Duration::from_millis(10), abandoned.notified())
                .await
                .is_err(),
            "the abandonment hook ran for a request that did not opt in"
        );
        assert!(
            CANCELLATION_WATCHED_STORAGE.is_committed(path).await,
            "a fill that did not opt in was discarded for a cancellation"
        );
    }
}
