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

use futures::StreamExt;
use pingora_cache::CachePhase;
use pingora_core::{
    protocols::http::{
        custom::{
            client::Session as CustomSession, is_informational_except_101, BodyWrite,
            CustomMessageWrite, CUSTOM_MESSAGE_QUEUE_SIZE,
        },
        v1::common::is_upgrade_req as is_h1_upgrade_req,
    },
    ImmutStr,
};
use proxy_cache::{range_filter::RangeBodyFilter, ServeFromCache};
use proxy_common::{
    downstream_closed_before_cache_admission, downstream_read_error_means_client_left,
    ends_upstream_task_stream, CacheFillCancellationState, DownstreamStateMachine, PipeState,
    ResponseStateMachine,
};
use tokio::sync::oneshot;

use super::*;

/// A custom message that a [`CustomMessageForwarder`] hands to the transport loop to filter,
/// with the sender that returns the filtered message, or `None` to drop it.
type CustomMessageFilterRequest = (Bytes, oneshot::Sender<Option<Bytes>>);

/// The transport loop's ends of the channels to the custom-message forwarders of one custom
/// upstream attempt.
///
/// Each direction's [`CustomMessageForwarder`] passes its messages through a filter channel that
/// the loop serves until that direction stops, and ends once its cancel sender fires or is
/// dropped, so dropping this value also stops both forwarders. A forwarder whose filter channel
/// is dropped while it runs fails the attempt, so a channel is dropped only once its forwarder
/// is gone or together with cancelling it.
struct CustomMessageChannels {
    /// Filter channel for messages to the upstream, until the loop stops serving it.
    to_upstream: Option<mpsc::Receiver<CustomMessageFilterRequest>>,
    /// Filter channel for messages to the downstream, until the loop stops serving it.
    to_downstream: Option<mpsc::Receiver<CustomMessageFilterRequest>>,
    /// Stops the forwarder that reads the downstream's messages.
    cancel_downstream_reader: Option<oneshot::Sender<()>>,
    /// Stops the forwarder that reads the upstream's messages. Only held with downstream
    /// abandonment handling; see [`split_cancel_upstream_reader`].
    cancel_upstream_reader: Option<oneshot::Sender<()>>,
}

impl CustomMessageChannels {
    fn new(
        to_upstream: mpsc::Receiver<CustomMessageFilterRequest>,
        to_downstream: mpsc::Receiver<CustomMessageFilterRequest>,
        cancel_downstream_reader: oneshot::Sender<()>,
        cancel_upstream_reader: Option<oneshot::Sender<()>>,
    ) -> Self {
        Self {
            to_upstream: Some(to_upstream),
            to_downstream: Some(to_downstream),
            cancel_downstream_reader: Some(cancel_downstream_reader),
            cancel_upstream_reader,
        }
    }

    /// Whether the loop still serves either filter channel.
    fn is_open(&self) -> bool {
        self.to_upstream.is_some() || self.to_downstream.is_some()
    }

    /// Stop serving the filter channel for messages to the downstream and cancel the forwarder
    /// that reads the upstream's messages. Repeat calls do nothing.
    ///
    /// An abandoned downstream stops only this direction: the cache fill still uses the upstream,
    /// which keeps receiving custom messages.
    fn stop_to_downstream(&mut self) {
        self.to_downstream = None;
        if let Some(cancel) = self.cancel_upstream_reader.take() {
            let _ = cancel.send(());
        }
    }

    /// Cancel only the forwarder that reads the downstream's messages and keep serving both
    /// filter channels, which is how an upload failure is handled. Panics if that forwarder was
    /// already cancelled.
    fn cancel_downstream_reader_only(&mut self) {
        let _ = self
            .cancel_downstream_reader
            .take()
            .expect("cancel must be set and called once")
            .send(());
    }
}

/// Split the sender that cancels the forwarder reading the upstream's messages into the transport
/// loop's share and the attempt scope's share, as `(loop, scope)`.
///
/// With downstream abandonment handling the loop holds the sender, to stop forwarding once the
/// downstream is abandoned. Otherwise the attempt's scope holds it, and its drop at the end of
/// that scope cancels the forwarder.
fn split_cancel_upstream_reader(
    handles_downstream_abandonment: bool,
    cancel_upstream_reader: oneshot::Sender<()>,
) -> (Option<oneshot::Sender<()>>, Option<oneshot::Sender<()>>) {
    if handles_downstream_abandonment {
        (Some(cancel_upstream_reader), None)
    } else {
        (None, Some(cancel_upstream_reader))
    }
}

/// The outcome of sending one piece of the request body to a custom upstream.
///
/// Errors from the request body filters are returned as `Err` instead, because they fail the
/// request regardless of what the upstream responds.
enum CustomBodyWriteResult {
    /// The body piece was written, or there was nothing to write. The flag is whether the request
    /// body is now finished.
    Sent(bool),
    /// Writing or finishing the request body failed. The upstream may still send a complete
    /// response, so the caller stops sending the body instead of failing the request at once.
    UpstreamFailed(Box<Error>),
}

/// Record whether `task` completes the upstream response, and put a pending upload error in place
/// of a response failure, as the HTTP/1 path reports its upload error over a later read error.
fn received_upstream_task(
    task: HttpTask,
    response_state: &mut ResponseStateMachine,
    upload_error: &mut Option<Box<Error>>,
) -> HttpTask {
    // The custom response reader emits Done only after it has checked both the body and the
    // trailers. A closed task pipe is not equivalent.
    response_state.maybe_set_upstream_response_complete(matches!(task, HttpTask::Done));
    match task {
        HttpTask::Failed(e) => HttpTask::Failed(upload_error.take().unwrap_or(e)),
        task => task,
    }
}

/// An ordinary response can finish by downstream framing before the custom producer sends Done.
/// Channel closure and delivery state alone do not prove this. An admitted cache fill still needs
/// its producer's terminal result, even if the connected client already has the whole response.
fn upload_response_complete<DS: DownstreamSession>(
    session: &Session<DS>,
    response_state: &ResponseStateMachine,
) -> bool {
    response_state.upstream_response_complete()
        || (!session.downstream_abandoned_during_cache_fill()
            && !session.cache.is_cache_admission_active()
            && session.downstream_session.is_response_complete())
}

impl<SV, C, DS> HttpProxy<SV, C, DS>
where
    C: custom::Connector,
    DS: DownstreamSession,
{
    /// Proxy to a custom protocol upstream.
    /// Returns (reuse_server, error) and records whether writing the request was attempted.
    pub(crate) async fn proxy_to_custom_upstream(
        &self,
        session: &mut Session<DS>,
        client_session: &mut C::Session,
        reused: bool,
        peer: &HttpPeer,
        ctx: &mut <SV as ProxyHttp<DS>>::CTX,
        request_write_attempted: &mut bool,
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
            .custom_proxy_down_to_up(session, client_session, peer, ctx, request_write_attempted)
            .await;

        // Parity with H1/H2: custom upstreams don't report payload bytes; record 0.
        session.set_upstream_body_bytes_received(0);

        (server_session_reuse, error)
    }

    /// Handle custom protocol proxying from downstream to upstream.
    /// Returns (reuse_server, error)
    async fn custom_proxy_down_to_up(
        &self,
        session: &mut Session<DS>,
        client_session: &mut C::Session,
        peer: &HttpPeer,
        ctx: &mut <SV as ProxyHttp<DS>>::CTX,
        request_write_attempted: &mut bool,
    ) -> (bool, Option<Box<Error>>)
    where
        SV: ProxyHttp<DS> + Send + Sync,
        <SV as ProxyHttp<DS>>::CTX: Send + Sync,
    {
        client_session.set_read_timeout(peer.options.read_timeout);
        client_session.set_write_timeout(peer.options.write_timeout);

        let mut req = session.req_header().clone();

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

        session.set_upstream_h1_upgrade_request_status(is_h1_upgrade_req(&req));

        session.upstream_compression.request_filter(&req);
        let body_empty = session.as_mut().is_body_empty();

        debug!("Request to custom: {req:?}");

        let req = Box::new(req);
        *request_write_attempted = true;
        if let Err(e) = client_session.write_request_header(req, body_empty).await {
            return (false, Some(e.into_up()));
        }

        // take the body writer out of the client for easy duplex
        let mut client_body = client_session
            .take_request_body_writer()
            .expect("already send request header");

        let (tx, rx) = mpsc::channel::<HttpTask>(TASK_BUFFER_SIZE);

        session.as_mut().enable_retry_buffering();

        // Custom message logic

        let Some(upstream_custom_message_reader) = client_session.take_custom_message_reader()
        else {
            return (
                false,
                Some(Error::explain(
                    ReadError,
                    "can't extract custom reader from upstream",
                )),
            );
        };
        // Without downstream abandonment handling the reader stays here until this function
        // returns, as before that handling existed.
        let mut retained_upstream_custom_message_reader = None;

        let Some(mut upstream_custom_message_writer) = client_session.take_custom_message_writer()
        else {
            return (
                false,
                Some(Error::explain(
                    WriteError,
                    "custom upstream must have a custom message writer",
                )),
            );
        };

        // A channel to inject custom messages to upstream from server logic.
        let (upstream_custom_message_inject_tx, upstream_custom_message_inject_rx) =
            mpsc::channel(CUSTOM_MESSAGE_QUEUE_SIZE);

        // Downstream reader
        let mut downstream_custom_message_reader = match session.downstream_custom_message() {
            Ok(Some(rx)) => rx,
            Ok(None) => Box::new(futures::stream::empty::<Result<Bytes>>()),
            Err(err) => return (false, Some(err)),
        };

        // Downstream writer
        let (mut downstream_custom_message_writer, downstream_custom_final_hop): (
            Box<dyn CustomMessageWrite>,
            bool, // if this hop is final
        ) = if let Some(custom_session) = session.downstream_session.as_custom_mut() {
            (
                custom_session
                    .take_custom_message_writer()
                    .expect("custom downstream must have a custom message writer"),
                false,
            )
        } else {
            (Box::new(()), true)
        };

        // A channel to inject custom messages to downstream from server logic.
        let (downstream_custom_message_inject_tx, downstream_custom_message_inject_rx) =
            mpsc::channel(CUSTOM_MESSAGE_QUEUE_SIZE);

        // Filters for ProxyHttp trait
        let (upstream_custom_message_filter_tx, upstream_custom_message_filter_rx) =
            mpsc::channel(CUSTOM_MESSAGE_QUEUE_SIZE);
        let (downstream_custom_message_filter_tx, downstream_custom_message_filter_rx) =
            mpsc::channel(CUSTOM_MESSAGE_QUEUE_SIZE);

        // Cancellation channels for custom coroutines
        // The transmitters act as guards: when dropped, they signal the receivers to cancel.
        // `cancel_downstream_reader_tx` is held and later used to explicitly cancel.
        // `split_cancel_upstream_reader` decides whether the transport loop or this scope holds
        // the upstream reader's sender.
        let (cancel_downstream_reader_tx, cancel_downstream_reader_rx) = oneshot::channel();
        let (cancel_upstream_reader_tx, cancel_upstream_reader_rx) = oneshot::channel();
        let (loop_cancel_upstream_reader_tx, _scope_cancel_upstream_reader_tx) =
            split_cancel_upstream_reader(
                session.handles_downstream_abandonment(),
                cancel_upstream_reader_tx,
            );

        if let Err(e) = self
            .inner
            .custom_forwarding(
                session,
                ctx,
                Some(upstream_custom_message_inject_tx),
                downstream_custom_message_inject_tx,
            )
            .await
        {
            if let Some(custom_session) = session.downstream_session.as_custom_mut() {
                if let Err(restore_error) =
                    custom_session.restore_custom_message_writer(downstream_custom_message_writer)
                {
                    return (false, Some(restore_error));
                }

                if let Err(restore_error) =
                    custom_session.restore_custom_message_reader(downstream_custom_message_reader)
                {
                    return (false, Some(restore_error));
                }
            }
            return (false, Some(e));
        }

        let upstream_custom_message_forwarder = CustomMessageForwarder {
            ctx: "down_to_up".into(),
            reader: &mut downstream_custom_message_reader,
            writer: &mut upstream_custom_message_writer,
            filter: upstream_custom_message_filter_tx,
            inject: upstream_custom_message_inject_rx,
            cancel: cancel_downstream_reader_rx,
            cancel_first: false,
        };

        let downstream_custom_message_forwarder = CustomMessageForwarder {
            ctx: "up_to_down".into(),
            reader: if session.handles_downstream_abandonment() {
                // Own the reader so cancellation closes its queue before the response finishes.
                futures::future::Either::Left(upstream_custom_message_reader)
            } else {
                futures::future::Either::Right(
                    retained_upstream_custom_message_reader.insert(upstream_custom_message_reader),
                )
            },
            writer: &mut downstream_custom_message_writer,
            filter: downstream_custom_message_filter_tx,
            inject: downstream_custom_message_inject_rx,
            cancel: cancel_upstream_reader_rx,
            cancel_first: session.handles_downstream_abandonment(),
        };

        // Shared signal so the upstream half can distinguish an expected task-pipe
        // closure (the downstream half finished and dropped rx) from an unexpected one.
        let pipe_state = Arc::new(AtomicU8::new(PipeState::Active as u8));

        /* read downstream body and upstream response at the same time */
        let ret = tokio::try_join!(
            self.custom_bidirection_down_to_up(
                session,
                &mut client_body,
                rx,
                ctx,
                upstream_custom_message_filter_rx,
                downstream_custom_message_filter_rx,
                downstream_custom_final_hop,
                cancel_downstream_reader_tx,
                loop_cancel_upstream_reader_tx,
                pipe_state.clone(),
            ),
            custom_pipe_up_to_down_response(client_session, tx, pipe_state),
            upstream_custom_message_forwarder.proxy(),
            downstream_custom_message_forwarder.proxy(),
        );

        if let Some(custom_session) = session.downstream_session.as_custom_mut() {
            custom_session
                .restore_custom_message_writer(downstream_custom_message_writer)
                .expect("downstream restore_custom_message_writer should be empty");

            custom_session
                .restore_custom_message_reader(downstream_custom_message_reader)
                .expect("downstream restore_custom_message_reader should be empty");
        }

        match ret {
            Ok((downstream_can_reuse, _upstream, _custom_up_down, _custom_down_up)) => {
                (downstream_can_reuse, None)
            }
            Err(e) => (false, Some(e)),
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn process_upstream_tasks_custom(
        &self,
        session: &mut Session<DS>,
        ctx: &mut <SV as ProxyHttp<DS>>::CTX,
        initial_task: HttpTask,
        rx: &mut mpsc::Receiver<HttpTask>,
        serve_from_cache: &mut ServeFromCache,
        range_body_filter: &mut proxy_cache::range_filter::RangeBodyFilter,
        response_state: &mut ResponseStateMachine,
        upload_error: &mut Option<Box<Error>>,
    ) -> Result<Option<bool>>
    where
        SV: ProxyHttp<DS> + Send + Sync,
        <SV as ProxyHttp<DS>>::CTX: Send + Sync,
    {
        let initial_task = received_upstream_task(initial_task, response_state, upload_error);
        if serve_from_cache.should_discard_upstream() {
            response_state
                .maybe_set_upstream_producer_done(ends_upstream_task_stream(&initial_task));
            // As on H1, an already selected cached response wins over subsequent upstream
            // tasks, including Failed. A header-read failure reaches here only after an upload
            // failure; otherwise the producer returns it directly to cancel a live writer.
            // Nothing is written downstream this round; continue serving the cached response.
            return Ok(None);
        }

        // Batch: pull as many tasks as we can from rx
        let mut tasks = Vec::with_capacity(TASK_BUFFER_SIZE);
        tasks.push(initial_task);
        while let Ok(task) = rx.try_recv() {
            tasks.push(received_upstream_task(task, response_state, upload_error));
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
                .custom_response_filter(session, t, ctx, serve_from_cache, range_body_filter, false)
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

    // Returns whether server (downstream) session can be reused
    #[allow(clippy::too_many_arguments)]
    async fn custom_bidirection_down_to_up(
        &self,
        session: &mut Session<DS>,
        client_body: &mut Box<dyn BodyWrite>,
        mut rx: mpsc::Receiver<HttpTask>,
        ctx: &mut <SV as ProxyHttp<DS>>::CTX,
        upstream_custom_message_filter_rx: mpsc::Receiver<CustomMessageFilterRequest>,
        downstream_custom_message_filter_rx: mpsc::Receiver<CustomMessageFilterRequest>,
        downstream_custom_final_hop: bool,
        cancel_downstream_reader_tx: oneshot::Sender<()>,
        cancel_upstream_reader_tx: Option<oneshot::Sender<()>>,
        pipe_state: Arc<AtomicU8>,
    ) -> Result<bool>
    where
        SV: ProxyHttp<DS> + Send + Sync,
        <SV as ProxyHttp<DS>>::CTX: Send + Sync,
    {
        let mut custom_messages = CustomMessageChannels::new(
            upstream_custom_message_filter_rx,
            downstream_custom_message_filter_rx,
            cancel_downstream_reader_tx,
            cancel_upstream_reader_tx,
        );

        let request_body_finished = session.as_mut().is_body_done();
        let mut downstream_state = DownstreamStateMachine::for_proxy_attempt(
            request_body_finished,
            session.downstream_abandoned_during_cache_fill(),
        );

        // An upload failure is reported only if neither a complete response nor an upgraded
        // downstream half-close finishes the exchange. The error is taken when it is reported in
        // place of a response failure, while `upload_failed` keeps the failed writer from being
        // used again: body read after the failure is filtered and dropped.
        let mut upload_error = None;
        let mut upload_failed = false;
        let mut reads_since_upload_failure = 0;
        let mut upgraded_downstream_half_closed = false;

        // retry, send buffer if it exists
        if let Some(buffer) = session.as_mut().get_retry_buffer() {
            if let CustomBodyWriteResult::UpstreamFailed(error) = self
                .send_body_to_custom(
                    session,
                    Some(buffer),
                    request_body_finished,
                    client_body,
                    ctx,
                )
                .await?
            {
                warn!(
                    "body send error: {error}, {}",
                    self.inner.request_summary(session, ctx)
                );
                // The downstream state already says whether the body is finished. The loop
                // reads a bounded amount more, as after a failure in its read branch.
                upload_error = Some(error);
                upload_failed = true;
                pipe_state.store(PipeState::UploadFailed as u8, Ordering::Release);
                custom_messages.cancel_downstream_reader_only();
            }
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
            || (upload_error.is_some()
                && !upgraded_downstream_half_closed
                && !upload_response_complete(session, &response_state))
            || custom_messages.is_open()
        {
            // An abandoned downstream takes no more custom messages. Checking here once per
            // iteration covers every path that records the abandonment, including an earlier
            // attempt of this request.
            if session.downstream_abandoned_during_cache_fill() {
                custom_messages.stop_to_downstream();
            }
            // partial read support, this check will also be false if cache is disabled.
            let support_cache_partial_read =
                session.cache.support_streaming_partial_write() == Some(true);
            let upgraded = session.was_upgraded();
            fill_cancellation.start_watching(
                session.downstream_abandoned_during_cache_fill(),
                &session.cache,
            );
            // After an upload failure, read until the HTTP/1 task pipe would be full or closed.
            let read_after_upload_failure = reads_since_upload_failure < TASK_BUFFER_SIZE
                && !response_state.upstream_producer_done()
                && !upload_response_complete(session, &response_state);
            if upload_failed && !read_after_upload_failure {
                downstream_state.maybe_finished(true);
            }

            tokio::select! {
                body = async {
                    if upgraded
                        && downstream_state.is_done()
                        && !session.downstream_session.poll_after_upgrade()
                    {
                        // Preserve completed-upgrade suppression for H1 and subrequests.
                        // H1 idle reads can treat a valid request half-close as a disconnect.
                        std::future::pending().await
                    } else {
                        session.downstream_session.read_body_or_idle(downstream_state.is_done()).await
                    }
                }, if downstream_state.can_poll() && (!upload_failed || read_after_upload_failure) => {
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
                                continue;
                           } else {
                                return Err(e.into_down());
                           }
                        }
                    };
                    let is_body_done = session.is_body_done();
                    if body.is_none() && session.was_upgraded() {
                        upgraded_downstream_half_closed = true;
                        // Preserve Custom's existing full-duplex behavior when the upload
                        // succeeds. Only a failed upload follows H1's half-close policy.
                        response_state.maybe_set_upstream_delivery_done(upload_failed);
                    }

                    if upload_failed {
                        let (request_done, queued_for_h1) = self
                            .filter_body_after_upload_failure(session, body, is_body_done, ctx)
                            .await?;
                        if queued_for_h1 {
                            reads_since_upload_failure += 1;
                        }
                        downstream_state.maybe_finished(request_done);
                        continue;
                    }

                    match self.send_body_to_custom(session, body, is_body_done, client_body, ctx).await? {
                        CustomBodyWriteResult::Sent(request_done) =>  {
                            downstream_state.maybe_finished(request_done);
                        },
                        CustomBodyWriteResult::UpstreamFailed(e) => {
                            warn!("body send error: {e}, {}", self.inner.request_summary(session, ctx));
                            // The peer can reject an upload while returning a valid response.
                            // Stop forwarding request bytes but keep the downstream healthy:
                            // the loop reads a bounded amount more, as the HTTP/1 path does,
                            // and the normal finish path drains what is left of an HTTP/1
                            // body. An upgraded session without a downstream half-close keeps
                            // relaying the upstream until it ends, as the HTTP/1 path does.
                            upload_error = Some(e);
                            upload_failed = true;
                            pipe_state.store(PipeState::UploadFailed as u8, Ordering::Release);
                            // `finish` itself can fail after we observed the half-close.
                            response_state.maybe_set_upstream_delivery_done(upgraded_downstream_half_closed);
                            downstream_state.maybe_finished(is_body_done);
                            custom_messages.cancel_downstream_reader_only();
                        }
                    };
                },

                // Handle buffered upstream task from previous iteration
                task = async { next_upstream_task.take() }, if next_upstream_task.is_some() => {
                    debug!("buffered upstream event: {:?}", task);
                    if let Some(t) = task {
                        let Some(response_done) = self.process_upstream_tasks_custom(
                            session,
                            ctx,
                            t,
                            &mut rx,
                            &mut serve_from_cache,
                            &mut range_body_filter,
                            &mut response_state,
                            &mut upload_error,
                        ).await? else {
                            // nothing sent downstream e.g. serve_from_cache
                            continue;
                        };
                        response_state.maybe_set_upstream_delivery_done(response_done);
                        downstream_state.maybe_finished(session.is_body_done());
                    } else {
                        debug!("empty upstream event");
                        response_state.maybe_set_upstream_delivery_done(true);
                    }
                },

                task = fill_cancellation.recv_upstream(&mut rx), if (session.should_poll_upstream_tasks(&response_state)
                    || (upload_error.is_some() && !upgraded_downstream_half_closed && !upload_response_complete(session, &response_state)))
                    && next_upstream_task.is_none() => {
                    let task = task?;
                    debug!("upstream event: {:?}", task);
                    if let Some(t) = task {
                        let upgraded = session.was_upgraded();
                        let Some(response_done) = self.process_upstream_tasks_custom(
                            session,
                            ctx,
                            t,
                            &mut rx,
                            &mut serve_from_cache,
                            &mut range_body_filter,
                            &mut response_state,
                            &mut upload_error,
                        ).await? else {
                            // nothing sent downstream e.g. serve_from_cache
                            continue;
                        };
                        if !upgraded && session.was_upgraded() && downstream_state.can_poll() {
                            // just upgraded, the downstream state should be reset to continue to
                            // poll body
                            trace!("reset downstream state on upgrade");
                            downstream_state.reset();
                        }
                        response_state.maybe_set_upstream_delivery_done(response_done);
                        downstream_state.maybe_finished(session.is_body_done());
                    } else {
                        debug!("empty upstream event");
                        if session.downstream_abandoned_during_cache_fill() {
                            return Error::e_explain(
                                InternalError,
                                "upstream response task pipe closed before completion",
                            );
                        }
                        if !upload_response_complete(session, &response_state) {
                            if let Some(error) = upload_error.take() {
                                return Err(error);
                            }
                        }
                        response_state.maybe_set_upstream_delivery_done(true);
                    }
                },

                task = serve_from_cache.next_http_task(&mut session.cache, &mut range_body_filter, upgraded),
                    if !response_state.cached_done()
                        && !downstream_state.is_errored()
                        && serve_from_cache.is_on()
                        && !session.has_pending_downstream_tasks() => { // backpressure: don't queue if pending writes

                    let task = self.custom_response_filter(session, task?, ctx,
                        &mut serve_from_cache,
                        &mut range_body_filter, true).await?;

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

                ret = futures::future::OptionFuture::from(
                    custom_messages.to_upstream.as_mut().map(|rx| rx.recv())
                ), if custom_messages.to_upstream.is_some() => {
                    let Some((data, callback)) = ret.flatten() else {
                        debug!("upstream_custom_message_filter_rx: custom downstream to upstream exited on reading");
                        custom_messages.to_upstream = None;
                        continue;
                    };

                    let new_msg = self.inner
                        .downstream_custom_message_proxy_filter(session, data, ctx, false)  // false because the upstream is custom
                        .await?;

                    if callback.send(new_msg).is_err() {
                        debug!("upstream_custom_message_incoming_rx: custom downstream to upstream exited on callback");
                        custom_messages.to_upstream = None;
                        continue;
                    };
                },

                ret = futures::future::OptionFuture::from(
                    custom_messages.to_downstream.as_mut().map(|rx| rx.recv())
                ), if custom_messages.to_downstream.is_some() => {
                    let Some((data, callback)) = ret.flatten() else {
                        debug!("downstream_custom_message_filter_rx: custom upstream to downstream exited on reading");
                        custom_messages.to_downstream = None;
                        continue;
                    };

                    let new_msg = self.inner
                        .upstream_custom_message_proxy_filter(session, data, ctx, downstream_custom_final_hop)
                        .await?;

                    if callback.send(new_msg).is_err() {
                        debug!("downstream_custom_message_filter_rx: custom upstream to downstream exited on callback");
                        custom_messages.to_downstream = None;
                        continue;
                    };
                },

                else => {
                    break;
                }
            }
        }

        // Re-raise the error then the loop is finished. This returns without
        // signaling PipeState::DownstreamComplete, so the upstream half treats
        // the resulting task-pipe closure as unexpected and surfaces the error.
        if downstream_state.is_errored()
            && !(session.downstream_abandoned_during_cache_fill() && serve_from_cache.is_miss())
        {
            let err = Error::e_explain(WriteError, "downstream_state is_errored");
            error!("custom_bidirection_down_to_up: downstream_state.is_errored",);
            return err;
        }

        if let Some(error) = upload_error {
            // `101 Switching Protocols` is final; an upgraded downstream half-close can finish
            // delivery before the upstream response ends. A downstream abandoned during a cache
            // fill gets no response; the complete response went to the cache instead.
            let final_response_handled = session.downstream_abandoned_during_cache_fill()
                || session.response_written().is_some_and(|header| {
                    !is_informational_except_101(header.status.as_u16() as u32)
                });
            if !(upload_response_complete(session, &response_state)
                || upgraded_downstream_half_closed)
                || !final_response_handled
            {
                return Err(error);
            }
        }
        if !upload_failed {
            client_body.cleanup().await?;
        }
        // Otherwise abandon the failed upstream writer, as on the error path. Do not ask it to
        // finish the rejected upload again; the downstream can still follow its normal drain and
        // connection-reuse policy.

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
        pipe_state.store(
            if upload_failed {
                PipeState::DownstreamCompleteAfterUploadFailure as u8
            } else {
                PipeState::DownstreamComplete as u8
            },
            Ordering::Release,
        );
        Ok(reuse_downstream)
    }

    async fn custom_response_filter(
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
            self.upstream_filter(session, &mut task, ctx).await?;

            if let HttpTask::Header(header, _) = &task {
                reject_mismatched_h1_upgrade_101(session, header, "custom_upstream_filter")
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
            // skip the downstream filtering if these tasks are just for cache admission
            if !serve_from_cache
                .should_write_response(session.downstream_abandoned_during_cache_fill())
            {
                return Ok(task);
            }
        } // else: cached/local response, no need to trigger upstream filters and caching

        let track_max_cache_size = matches!(
            session.cache.phase(),
            CachePhase::Disabled(NoCacheReason::PredictedResponseTooLarge)
        );

        let res = match task {
            HttpTask::Header(mut header, eos) => {
                /* Downstream revalidation, only needed when cache is on because otherwise origin
                 * will handle it */
                // TODO: if cache is disabled during response phase, we should still do the filter
                if session.cache.enabled() {
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
                if !from_cache {
                    // Re-check after response_filter in case it changed the final status to 101.
                    reject_mismatched_h1_upgrade_101(session, &header, "custom_response_filter")
                        .map_err(|e| e.into_in())?;
                }

                // these status codes / method cannot have body, so no need to add chunked encoding
                let no_body = session.req_header().method == "HEAD"
                    || matches!(header.status.as_u16(), 204 | 304);

                /* Add chunked header to tell downstream to use chunked encoding
                 * during the absent of content-length */
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
                    trace!("delaying response for {duration:?}");
                    time::sleep(duration).await;
                }
                Ok(HttpTask::Body(data, eos))
            }
            HttpTask::UpgradedBody(mut data, eos) => {
                if track_max_cache_size {
                    session
                        .cache
                        .track_body_bytes_for_max_file_size(data.as_ref().map_or(0, |d| d.len()));
                }

                // range body filter doesn't apply to upgraded body
                if let Some(duration) = self
                    .inner
                    .response_body_filter(session, &mut data, eos, ctx)
                    .await?
                {
                    trace!("delaying upgraded response for {duration:?}");
                    time::sleep(duration).await;
                }
                Ok(HttpTask::UpgradedBody(data, eos))
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

    async fn send_body_to_custom(
        &self,
        session: &mut Session<DS>,
        mut data: Option<Bytes>,
        end_of_body: bool,
        client_body: &mut Box<dyn BodyWrite>,
        ctx: &mut <SV as ProxyHttp<DS>>::CTX,
    ) -> Result<CustomBodyWriteResult>
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

        if session.was_upgraded() {
            client_body.upgrade_body_writer();
        }

        /* it is normal to get 0 bytes because of multi-chunk parsing or request_body_filter.
         * Although there is no harm writing empty byte to custom, unlike h1, we ignore it
         * for consistency */
        if !end_of_body && data.as_ref().is_some_and(|d| d.is_empty()) {
            return Ok(CustomBodyWriteResult::Sent(false));
        }

        if let Some(mut data) = data {
            if let Err(error) = client_body.write_all_buf(&mut data).await {
                return Ok(CustomBodyWriteResult::UpstreamFailed(error.into_up()));
            }
            if end_of_body {
                if let Err(error) = client_body.finish().await {
                    return Ok(CustomBodyWriteResult::UpstreamFailed(error.into_up()));
                }
            }
        } else {
            debug!("Read downstream body done");
            if let Err(error) = client_body
                .finish()
                .await
                .map_err(|e| {
                    Error::because(WriteError, "while shutdown send data stream on no data", e)
                })
                .map_err(|e| e.into_up())
            {
                return Ok(CustomBodyWriteResult::UpstreamFailed(error));
            }
        }

        Ok(CustomBodyWriteResult::Sent(end_of_body))
    }

    /// Run the downstream modules' request body filters and
    /// [`ProxyHttp::request_body_filter`] on a piece of the body read after the upload to the
    /// custom upstream failed, then drop it. The HTTP/1 path filters such pieces too, before
    /// queuing them for an upstream that no longer reads them.
    ///
    /// Returns whether the request body is finished and whether HTTP/1 would queue this piece.
    async fn filter_body_after_upload_failure(
        &self,
        session: &mut Session<DS>,
        mut data: Option<Bytes>,
        end_of_body: bool,
        ctx: &mut <SV as ProxyHttp<DS>>::CTX,
    ) -> Result<(bool, bool)>
    where
        SV: ProxyHttp<DS> + Send + Sync,
        <SV as ProxyHttp<DS>>::CTX: Send + Sync,
    {
        let end_of_body = end_of_body || data.is_none();
        session
            .downstream_modules_ctx
            .request_body_filter(&mut data, end_of_body)
            .await?;
        self.inner
            .request_body_filter(session, &mut data, end_of_body, ctx)
            .await?;
        let upstream_end_of_body = end_of_body || data.is_none();
        let queued_for_h1 =
            upstream_end_of_body || data.as_ref().is_some_and(|piece| !piece.is_empty());
        Ok((end_of_body, queued_for_h1))
    }
}

/* Read response header, body and trailer from custom upstream and send them to tx */
async fn custom_pipe_up_to_down_response<S: CustomSession>(
    client: &mut S,
    tx: mpsc::Sender<HttpTask>,
    pipe_state: Arc<AtomicU8>,
) -> Result<()> {
    let response = read_custom_response(client, &tx, &pipe_state);
    tokio::pin!(response);
    tokio::select! {
        biased;
        result = &mut response => result,
        _ = tx.closed() => {
            // The downstream loop publishes this only after successful finalization. Cancelling
            // a read here releases a producer stalled on EOS or trailers after the response was
            // complete by downstream framing. Preserve the existing reader lifetime otherwise.
            if pipe_state.load(Ordering::Acquire)
                == PipeState::DownstreamCompleteAfterUploadFailure as u8
            {
                Ok(())
            } else {
                response.await
            }
        }
    }
}

async fn read_custom_response<S: CustomSession>(
    client: &mut S,
    tx: &mpsc::Sender<HttpTask>,
    pipe_state: &AtomicU8,
) -> Result<()> {
    let mut is_informational = true;
    while is_informational {
        if let Err(e) = client.read_response_header().await {
            // A live writer may be blocked indefinitely without a write timeout. Preserve the
            // direct error path so try_join! cancels it. Queue only after the upload has failed
            // and its writer is no longer polled, allowing the earlier upload error to win.
            if pipe_state.load(Ordering::Acquire) != PipeState::UploadFailed as u8 {
                return Err(e.into_up());
            }
            let _ = tx.send(HttpTask::Failed(e.into_up())).await;
            return Ok(());
        }
        let resp_header = Box::new(client.response_header().expect("just read").clone());
        // `101 Switching Protocols` is a response to the http1 Upgrade header and it's final response.
        // The WebSocket Protocol https://datatracker.ietf.org/doc/html/rfc6455
        is_informational = is_informational_except_101(resp_header.status.as_u16() as u32);

        match client.check_response_end_or_error(true).await {
            Ok(eos) => {
                tx.send(HttpTask::Header(resp_header, eos))
                    .await
                    .or_err(InternalError, "sending custom headers to pipe")?;
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
    }

    while let Some(chunk) = client
        .read_response_body()
        .await
        .map_err(|e| e.into_up())
        .transpose()
    {
        let data = match chunk {
            Ok(d) => d,
            Err(e) => {
                // Push the error to downstream and then quit
                let _ = tx.send(HttpTask::Failed(e.into_up())).await;
                // Downstream should consume all remaining data and handle the error
                return Ok(());
            }
        };

        match client.check_response_end_or_error(false).await {
            Ok(eos) => {
                let empty = data.is_empty();
                if empty && !eos {
                    /* it is normal to get 0 bytes because of multi-chunk
                     * don't write 0 bytes to downstream since it will be
                     * misread as the terminating chunk */
                    continue;
                }
                let body_task = if client.was_upgraded() {
                    HttpTask::UpgradedBody(Some(data), eos)
                } else {
                    HttpTask::Body(Some(data), eos)
                };
                // A send failure is benign only when the downstream half signaled it
                // completed (it finished the response by its own framing and dropped the
                // task pipe): stop reading the upstream. Otherwise the closure is
                // unexpected, so surface the original error.
                let send_result = tx.send(body_task).await;
                if send_result.is_err()
                    && PipeState::is_downstream_complete(pipe_state.load(Ordering::Acquire))
                {
                    return Ok(());
                }
                send_result.or_err(InternalError, "sending custom body to pipe")?;
            }
            Err(e) => {
                // Similar to above, push the error to downstream and then quit
                let _ = tx.send(HttpTask::Failed(e.into_up())).await;
                return Ok(());
            }
        }
    }

    // attempt to get trailers
    let trailers = match client.read_trailers().await {
        Ok(t) => t,
        Err(e) => {
            // Similar to above, push the error to downstream and then quit
            let _ = tx.send(HttpTask::Failed(e.into_up())).await;
            return Ok(());
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
        send_result.or_err(InternalError, "sending custom trailer to pipe")?;
    }

    let send_result = tx.send(HttpTask::Done).await;
    if send_result.is_err() && PipeState::is_downstream_complete(pipe_state.load(Ordering::Acquire))
    {
        debug!("custom channel closed!");
        return Ok(());
    }
    send_result.or_err(InternalError, "sending custom done to pipe")?;

    Ok(())
}

struct CustomMessageForwarder<'a, R> {
    ctx: ImmutStr,
    writer: &'a mut Box<dyn CustomMessageWrite>,
    reader: R,
    inject: mpsc::Receiver<Bytes>,
    filter: mpsc::Sender<(Bytes, oneshot::Sender<Option<Bytes>>)>,
    cancel: oneshot::Receiver<()>,
    /// Poll `cancel` before the messages, so that once the forwarder toward an abandoned
    /// downstream is cancelled, its custom write errors cannot abort a cache fill that continues
    /// without that downstream. Set only for that forwarder with downstream abandonment handling.
    cancel_first: bool,
}

impl<R> CustomMessageForwarder<'_, R>
where
    R: futures::Stream<Item = Result<Bytes>> + Unpin,
{
    async fn proxy(mut self) -> Result<()> {
        let forwarder = async {
            let mut injector_status = true;
            let mut reader_status = true;

            debug!("{}: CustomMessageForwarder: start", self.ctx);

            while injector_status || reader_status {
                let (data, proxied) = tokio::select! {
                    ret = self.inject.recv(), if injector_status => {
                        let Some(data) = ret else {
                            injector_status = false;
                            continue
                        };
                        (data, false)
                    },

                    ret = self.reader.next(), if reader_status  => {
                        let Some(data) = ret else {
                            reader_status = false;
                            continue
                        };

                        let data = match data {
                            Ok(data) => data,
                            Err(err) => {
                                reader_status = false;
                                warn!("{}: CustomMessageForwarder: reader returned err: {err:?}", self.ctx);
                                continue;
                            },
                        };
                        (data, true)
                    },
                };

                let (callback_tx, callback_rx) = oneshot::channel();

                // If data received from proxy send it to filter
                if proxied {
                    if self.filter.send((data, callback_tx)).await.is_err() {
                        debug!(
                            "{}: CustomMessageForwarder: filter receiver dropped",
                            self.ctx
                        );
                        return Error::e_explain(
                            WriteError,
                            "CustomMessageForwarder: main proxy thread exited on filter send",
                        );
                    };
                } else {
                    callback_tx
                        .send(Some(data))
                        .expect("sending from the same thread");
                }

                match callback_rx.await {
                    Ok(None) => continue, // message was filtered
                    Ok(Some(msg)) => {
                        self.writer.write_custom_message(msg).await?;
                    }
                    Err(err) => {
                        debug!(
                            "{}: CustomMessageForwarder: callback_rx return error: {err}",
                            self.ctx
                        );
                        return Error::e_because(
                            WriteError,
                            "CustomMessageForwarder: main proxy thread exited on callback_rx await",
                            err,
                        );
                    }
                };
            }

            debug!("{}: CustomMessageForwarder: exit loop", self.ctx);

            let ret = self.writer.finish_custom().await;
            if let Err(ref err) = ret {
                debug!(
                    "{}: CustomMessageForwarder: finish_custom return error: {err}",
                    self.ctx
                );
            };
            ret?;

            debug!(
                "{}: CustomMessageForwarder: exit loop successfully",
                self.ctx
            );

            Ok(())
        };

        // TODO: temporary. The unbiased select only keeps the behavior from before downstream
        // abandonment handling. Biasing it too should make little difference, so merge both into
        // the biased select and remove `cancel_first`.
        if self.cancel_first {
            tokio::select! {
                biased;
                ret = &mut self.cancel => {
                    debug!("{}: CustomMessageForwarder: canceled while waiting for new messages: {ret:?}", self.ctx);
                    Ok(())
                },
                ret = forwarder => ret
            }
        } else {
            tokio::select! {
                ret = &mut self.cancel => {
                    debug!("{}: CustomMessageForwarder: canceled while waiting for new messages: {ret:?}", self.ctx);
                    Ok(())
                },
                ret = forwarder => ret
            }
        }
    }
}

#[cfg(test)]
mod custom_session_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{
        cache_miss_session_on, AbandonmentSignalProxy, AbandonmentTiming,
        CANCELLATION_WATCHED_STORAGE,
    };
    use pingora_cache::fill_cancellation::ERR_CACHE_FILL_CANCELLED;
    use pingora_cache::{
        predictor::{CacheablePredictor, Predictor},
        CacheKey, MemCache,
    };
    use std::{
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc, LazyLock,
        },
        time::Duration,
    };
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    static CACHE_STORAGE: LazyLock<MemCache> = LazyLock::new(MemCache::new);
    static CACHE_PREDICTOR: LazyLock<Predictor<1>> = LazyLock::new(|| Predictor::new(10, None));

    mod cache_fill_abandonment;
    mod header_read_failure;
    mod upload_failure_downstreams;

    struct TestProxy;

    struct TestBodyWriter {
        cleanup_called: Arc<AtomicBool>,
        finish_notify: Option<Arc<tokio::sync::Notify>>,
    }

    /// Fails only when the upgraded downstream half-closes, rather than during a body write.
    struct FailedFinishBodyWriter(TestBodyWriter);

    #[async_trait]
    impl BodyWrite for FailedFinishBodyWriter {
        async fn write_all_buf(&mut self, data: &mut Bytes) -> Result<()> {
            self.0.write_all_buf(data).await
        }

        async fn finish(&mut self) -> Result<()> {
            self.0.finish().await?;
            Error::e_explain(WriteError, "upstream rejected the half-close")
        }

        async fn cleanup(&mut self) -> Result<()> {
            panic!("a failed finish must not be retried by cleanup")
        }

        fn upgrade_body_writer(&mut self) {}
    }

    struct CompletedResponse {
        result: Result<bool>,
        response: Vec<u8>,
        cleanup_called: bool,
        downstream_complete: bool,
    }

    struct StoppedUploadWriter(Arc<tokio::sync::Notify>);

    #[async_trait]
    impl BodyWrite for StoppedUploadWriter {
        async fn write_all_buf(&mut self, _data: &mut Bytes) -> Result<()> {
            self.0.notify_one();
            self.finish().await
        }

        async fn finish(&mut self) -> Result<()> {
            Error::e_explain(WriteError, "upstream stopped accepting the upload")
        }

        async fn cleanup(&mut self) -> Result<()> {
            panic!("a failed upstream writer must be abandoned, not finished again")
        }

        fn upgrade_body_writer(&mut self) {}
    }

    async fn upload_session() -> (Session, tokio::io::DuplexStream) {
        let (mut client, server) = tokio::io::duplex(4096);
        client
            .write_all(
                b"POST /upload HTTP/1.1\r\nHost: example.com\r\nContent-Length: 65536\r\n\r\n",
            )
            .await
            .unwrap();
        let mut session = Session::new_h1(Box::new(server));
        session.read_request().await.unwrap();
        session.set_keepalive(Some(10));
        session.set_close_on_response_before_downstream_finish(false);
        (session, client)
    }

    async fn proxy_upload(
        session: &mut Session,
        rx: mpsc::Receiver<HttpTask>,
        failed: Arc<tokio::sync::Notify>,
    ) -> Result<bool> {
        proxy_upload_with(session, rx, Box::new(StoppedUploadWriter(failed))).await
    }

    async fn proxy_upload_with(
        session: &mut Session,
        rx: mpsc::Receiver<HttpTask>,
        mut writer: Box<dyn BodyWrite>,
    ) -> Result<bool> {
        let proxy = HttpProxy::new(TestProxy, Arc::new(ServerConf::default()));
        // No custom messages remain in either direction.
        let up_rx = mpsc::channel(1).1;
        let down_rx = mpsc::channel(1).1;
        let (cancel_tx, _) = oneshot::channel();
        let state = Arc::new(AtomicU8::new(PipeState::Active as u8));
        let result = proxy
            .custom_bidirection_down_to_up(
                session,
                &mut writer,
                rx,
                &mut (),
                up_rx,
                down_rx,
                true,
                cancel_tx,
                None,
                state.clone(),
            )
            .await;
        assert_eq!(
            PipeState::is_downstream_complete(state.load(Ordering::Acquire)),
            result.is_ok()
        );
        result
    }

    fn response_head(status: u16, length: usize) -> HttpTask {
        let mut header = ResponseHeader::build(status, Some(1)).unwrap();
        header
            .insert_header("Content-Length", length.to_string())
            .unwrap();
        HttpTask::Header(Box::new(header), false)
    }

    fn rejection_tasks() -> Vec<HttpTask> {
        vec![
            response_head(413, 200),
            HttpTask::Body(Some(Bytes::from(vec![b'x'; 200])), false),
            HttpTask::Done,
        ]
    }

    async fn read_rejection(client: &mut tokio::io::DuplexStream, status: u16) {
        let mut head = Vec::new();
        while !head.ends_with(b"\r\n\r\n") {
            head.push(client.read_u8().await.unwrap());
        }
        assert!(head.starts_with(format!("HTTP/1.1 {status} ").as_bytes()));
        let mut body = [0; 200];
        client.read_exact(&mut body).await.unwrap();
        assert_eq!(body, [b'x'; 200]);
    }

    async fn h1_upload_response(
        response: &[u8],
        complete: bool,
    ) -> (
        Session,
        tokio::io::DuplexStream,
        bool,
        bool,
        Option<Box<Error>>,
    ) {
        let (mut session, mut downstream) = upload_session().await;
        downstream.write_all(b"u").await.unwrap();
        let mut request = session.req_header().clone();
        crate::proxy_common::finalize_h1_upstream_request_framing(&mut request, true).unwrap();
        let request_wire =
            pingora_core::protocols::http::v1::client::http_req_header_to_wire(&request).unwrap();
        let upstream_io = {
            let mut script = tokio_test::io::Builder::new();
            script.write(&request_wire);
            script.write_error(std::io::Error::from(std::io::ErrorKind::BrokenPipe));
            if !response.is_empty() {
                script.read(response);
            }
            if complete {
                // Completion must not require transport EOF after the Content-Length body.
                script.wait(Duration::from_secs(30));
            } else {
                script.read_error(std::io::Error::from(std::io::ErrorKind::UnexpectedEof));
            }
            script.build()
        };
        let mut upstream = HttpSessionV1::new(Box::new(upstream_io));
        let peer = HttpPeer::new(("127.0.0.1", 1), false, String::new());
        let proxy = HttpProxy::new(TestProxy, Arc::new(ServerConf::default()));
        let (downstream_reusable, upstream_reusable, error) = proxy
            .proxy_1to1(&mut session, &mut upstream, &peer, &mut ())
            .await;
        (
            session,
            downstream,
            downstream_reusable,
            upstream_reusable,
            error,
        )
    }

    #[tokio::test]
    async fn early_upload_failure_matches_h1_response_status() {
        tokio::time::timeout(Duration::from_secs(3), async {
            for status in [401, 413] {
                let mut response =
                    format!("HTTP/1.1 {status} Rejected\r\nContent-Length: 200\r\n\r\n")
                        .into_bytes();
                response.extend(std::iter::repeat_n(b'x', 200));
                let (_, mut downstream, downstream_reusable, upstream_reusable, error) =
                    h1_upload_response(&response, true).await;
                assert!(error.is_none(), "H1 completion: {error:?}");
                assert!(downstream_reusable);
                assert!(!upstream_reusable);
                read_rejection(&mut downstream, status).await;

                let (mut session, mut downstream) = upload_session().await;
                downstream.write_all(b"u").await.unwrap();
                let (tx, rx) = mpsc::channel(TASK_BUFFER_SIZE);
                tx.send(response_head(status, 200)).await.unwrap();
                tx.send(HttpTask::Body(Some(Bytes::from(vec![b'x'; 200])), false))
                    .await
                    .unwrap();
                tx.send(HttpTask::Done).await.unwrap();
                drop(tx);
                let result =
                    proxy_upload(&mut session, rx, Arc::new(tokio::sync::Notify::new())).await;
                assert!(matches!(result, Ok(true)), "custom completion: {result:?}");
                read_rejection(&mut downstream, status).await;
            }
        })
        .await
        .expect("both upstream paths must finish promptly");
    }

    async fn assert_failed_response_status(
        response: &[u8],
        tasks: Vec<HttpTask>,
        expected_wire_status: u16,
        expected_body: &[u8],
    ) {
        let (mut h1_session, mut h1_downstream, _, _, h1_error) =
            h1_upload_response(response, false).await;
        let h1_error = h1_error.expect("H1 must report an incomplete response");

        let (mut custom_session, mut custom_downstream) = upload_session().await;
        custom_downstream.write_all(b"u").await.unwrap();
        let (tx, rx) = mpsc::channel(TASK_BUFFER_SIZE);
        let failed = Arc::new(tokio::sync::Notify::new());
        let proxy = proxy_upload(&mut custom_session, rx, failed.clone());
        let producer = async {
            failed.notified().await;
            for task in tasks {
                tx.send(task).await.unwrap();
            }
            drop(tx);
        };
        let (custom_result, ()) = tokio::join!(proxy, producer);
        let custom_error = custom_result.expect_err("custom must report an incomplete response");

        for (session, downstream, error) in [
            (&mut h1_session, &mut h1_downstream, h1_error),
            (&mut custom_session, &mut custom_downstream, custom_error),
        ] {
            assert_eq!(
                error.etype(),
                &WriteError,
                "the upload error must win: {error}"
            );
            assert_eq!(error.esource, ErrorSource::Upstream);
            assert_eq!(
                session.response_written().map(|head| head.status.as_u16()),
                (!response.is_empty()).then_some(expected_wire_status)
            );
            assert_eq!(
                TestProxy
                    .fail_to_proxy(session, &error, &mut ())
                    .await
                    .error_code,
                502
            );
            let mut head = Vec::new();
            while !head.ends_with(b"\r\n\r\n") {
                head.push(downstream.read_u8().await.unwrap());
            }
            assert!(
                head.starts_with(format!("HTTP/1.1 {expected_wire_status} ").as_bytes()),
                "{head:?}"
            );
            let mut body = vec![0; expected_body.len()];
            downstream.read_exact(&mut body).await.unwrap();
            assert_eq!(body, expected_body);
        }
    }

    #[tokio::test]
    async fn failed_upload_without_response_matches_h1_bad_gateway() {
        tokio::time::timeout(
            Duration::from_secs(3),
            assert_failed_response_status(&[], vec![], 502, b""),
        )
        .await
        .expect("both upstream paths must reject missing responses promptly");
    }

    #[tokio::test]
    async fn failed_upload_with_partial_response_matches_h1_status_accounting() {
        let mut response =
            b"HTTP/1.1 413 Payload Too Large\r\nContent-Length: 200\r\n\r\n".to_vec();
        response.extend_from_slice(b"partial");
        let tasks = vec![
            response_head(413, 200),
            HttpTask::Body(Some(Bytes::from_static(b"partial")), false),
            HttpTask::Failed(Error::explain(ReadError, "response ended early").into_up()),
        ];
        tokio::time::timeout(
            Duration::from_secs(3),
            assert_failed_response_status(&response, tasks, 413, b"partial"),
        )
        .await
        .expect("both upstream paths must report partial responses promptly");
    }

    /// A client that leaves right after the upload failed, while the upstream sends nothing, ends
    /// the request with a downstream error on both paths.
    #[tokio::test(start_paused = true)]
    async fn client_leaving_after_upload_failure_matches_h1_downstream_error() {
        let h1_error = tokio::time::timeout(Duration::from_secs(3), async {
            let (mut session, mut client) = upload_session().await;
            client.write_all(b"u").await.unwrap();
            let mut request = session.req_header().clone();
            crate::proxy_common::finalize_h1_upstream_request_framing(&mut request, true).unwrap();
            let request_wire =
                pingora_core::protocols::http::v1::client::http_req_header_to_wire(&request)
                    .unwrap();
            let upstream_io = tokio_test::io::Builder::new()
                .write(&request_wire)
                .write_error(std::io::Error::from(std::io::ErrorKind::BrokenPipe))
                .wait(Duration::from_secs(30))
                .build();
            let mut upstream = HttpSessionV1::new(Box::new(upstream_io));
            let peer = HttpPeer::new(("127.0.0.1", 1), false, String::new());
            let proxy = HttpProxy::new(TestProxy, Arc::new(ServerConf::default()));
            let mut ctx = ();
            let mut run = tokio_test::task::spawn(proxy.proxy_1to1(
                &mut session,
                &mut upstream,
                &peer,
                &mut ctx,
            ));
            // Once only the client or the upstream's wait can wake it, the upstream half has
            // taken the body piece from its pipe and failed to write it.
            loop {
                if let std::task::Poll::Ready(result) = run.poll() {
                    panic!("H1 ended before the client left: {result:?}");
                }
                if !run.is_woken() {
                    break;
                }
            }
            drop(client);
            run.await.2
        })
        .await
        .expect("H1 did not end the request after the client left")
        .expect("H1 must report the client leaving");

        let custom_error = tokio::time::timeout(Duration::from_secs(3), async {
            let (mut session, mut client) = upload_session().await;
            client.write_all(b"u").await.unwrap();
            // The upstream sends nothing; closing the pipe would report the upload error instead.
            let (_tx, rx) = mpsc::channel(TASK_BUFFER_SIZE);
            let failed = Arc::new(tokio::sync::Notify::new());
            let leave = async {
                failed.notified().await;
                drop(client);
            };
            let (result, ()) = tokio::join!(proxy_upload(&mut session, rx, failed.clone()), leave);
            result
        })
        .await
        .expect("custom did not end the request after the client left")
        .expect_err("custom must report the client leaving");

        for error in [&h1_error, &custom_error] {
            assert_eq!(error.esource, ErrorSource::Downstream, "{error}");
        }
        assert_eq!(h1_error.etype(), custom_error.etype());
    }

    async fn early_response_case(response_first: bool, close_early: bool) {
        tokio::time::timeout(Duration::from_secs(3), async {
            let (mut session, mut client) = upload_session().await;
            session.set_close_on_response_before_downstream_finish(close_early);
            let drain_timeout = response_first.then_some(Duration::from_secs(1));
            session.set_total_drain_timeout(drain_timeout);
            let failed = Arc::new(tokio::sync::Notify::new());
            let (tx, rx) = mpsc::channel(TASK_BUFFER_SIZE);
            {
                let proxy = proxy_upload(&mut session, rx, failed.clone());
                tokio::pin!(proxy);
                if !response_first {
                    client.write_all(b"u").await.unwrap();
                    tokio::select! {
                        () = failed.notified() => {},
                        result = &mut proxy => panic!("proxy ended before the response: {result:?}"),
                    }
                }
                for task in rejection_tasks() {
                    tx.send(task).await.unwrap();
                }
                drop(tx);
                if response_first {
                    tokio::select! {
                        () = read_rejection(&mut client, 413) => {},
                        result = &mut proxy => panic!("proxy ended before the upload: {result:?}"),
                    }
                    client.write_all(b"u").await.unwrap();
                }
                let result = proxy.await;
                assert!(matches!(result, Ok(true)), "complete response must survive upload failure: {result:?}");
            }
            if !response_first {
                read_rejection(&mut client, 413).await;
            }
            assert_eq!(session.get_total_drain_timeout(), drain_timeout);
            if close_early {
                assert!(session.downstream_session.finish().await.unwrap().is_none());
                assert_eq!(client.read(&mut [0; 1]).await.unwrap(), 0);
                return;
            }
            // More than the 4 KiB duplex buffer: passing requires actual draining.
            let rest = vec![b'u'; 65535];
            let (reused, sent) = tokio::join!(session.downstream_session.finish(), client.write_all(&rest));
            sent.unwrap();
            let (stream, prefix) = reused.unwrap().expect("connection should be reusable").into_parts();
            assert!(prefix.is_none());
            client.write_all(b"GET /next HTTP/1.1\r\nHost: example.com\r\n\r\n").await.unwrap();
            let mut next = Session::new_h1(stream);
            next.read_request().await.unwrap();
            assert_eq!(next.req_header().uri.path(), "/next");
        }).await.expect("response and request drain must finish");
    }

    #[tokio::test]
    async fn custom_early_response_drains_after_upload_failure() {
        early_response_case(false, false).await;
    }

    #[tokio::test]
    async fn custom_early_response_drains_before_upload_failure() {
        early_response_case(true, false).await;
    }

    #[tokio::test]
    async fn custom_retry_buffer_upload_failure_preserves_complete_response() {
        tokio::time::timeout(Duration::from_secs(3), async {
            let (mut session, mut client) = upload_session().await;
            session.as_mut().enable_retry_buffering();
            client.write_all(b"u").await.unwrap();
            let buffered = session
                .downstream_session
                .read_request_body()
                .await
                .unwrap();
            assert_eq!(buffered, Some(Bytes::from_static(b"u")));
            assert_eq!(session.as_mut().get_retry_buffer(), buffered);

            let failed = Arc::new(tokio::sync::Notify::new());
            let (tx, rx) = mpsc::channel(TASK_BUFFER_SIZE);
            for task in rejection_tasks() {
                tx.send(task).await.unwrap();
            }
            drop(tx);
            let result = proxy_upload(&mut session, rx, failed.clone()).await;
            assert!(
                matches!(result, Ok(true)),
                "complete response must survive buffered upload failure: {result:?}"
            );
            failed.notified().await;
            read_rejection(&mut client, 413).await;

            let rest = vec![b'u'; 65535];
            let (reused, sent) =
                tokio::join!(session.downstream_session.finish(), client.write_all(&rest));
            sent.unwrap();
            let (stream, prefix) = reused
                .unwrap()
                .expect("connection should be reusable")
                .into_parts();
            assert!(prefix.is_none());
            client
                .write_all(b"GET /next HTTP/1.1\r\nHost: example.com\r\n\r\n")
                .await
                .unwrap();
            let mut next = Session::new_h1(stream);
            next.read_request().await.unwrap();
            assert_eq!(next.req_header().uri.path(), "/next");
        })
        .await
        .expect("buffered response and request drain must finish");
    }

    #[tokio::test]
    async fn custom_early_response_respects_close_policy() {
        early_response_case(false, true).await;
    }

    const SWITCHING_PROTOCOLS: &[u8] =
        b"HTTP/1.1 101 Switching Protocols\r\nConnection: upgrade\r\nUpgrade: websocket\r\n\r\n";

    /// How an upgraded exchange whose upload fails goes on.
    #[derive(Clone, Copy, Debug)]
    enum UpgradedUploadFailure {
        /// The upload fails after the `101`, then the upstream ends cleanly.
        AfterSwitch,
        /// The upload fails before the `101`, then the upstream ends cleanly.
        BeforeSwitch,
        /// The upload fails after the `101`, then reading the upstream fails.
        ThenResponseReadFails,
    }

    impl UpgradedUploadFailure {
        fn uploads_before_switch(self) -> bool {
            matches!(self, Self::BeforeSwitch)
        }

        fn response_completes(self) -> bool {
            !matches!(self, Self::ThenResponseReadFails)
        }
    }

    /// A session for an upgrade request with a one-byte body that the client already sent. With a
    /// body, the HTTP/1 upstream half has a request write to finish before it reads the `101`, so
    /// the scripted exchange happens in one order.
    async fn upgrade_session() -> (Session, tokio::io::DuplexStream) {
        // Spelled the way the HTTP/1 upstream request is sanitized, so the request arrives at the
        // scripted upstream unchanged.
        let request = b"POST /upgrade HTTP/1.1\r\nHost: example.com\r\nContent-Length: 1\r\nConnection: Upgrade\r\nupgrade: websocket\r\n\r\nu";
        let (mut client, server) = tokio::io::duplex(4096);
        client.write_all(request).await.unwrap();
        let mut session = Session::new_h1(Box::new(server));
        session.read_request().await.unwrap();
        (session, client)
    }

    /// Accepts the request body and fails every write after it, or fails from the first write.
    struct UpgradedUploadWriter {
        fails_first_write: bool,
        writes: usize,
        failed: Arc<tokio::sync::Notify>,
    }

    #[async_trait]
    impl BodyWrite for UpgradedUploadWriter {
        async fn write_all_buf(&mut self, data: &mut Bytes) -> Result<()> {
            self.writes += 1;
            if self.fails_first_write || self.writes > 1 {
                self.failed.notify_one();
                return Error::e_explain(WriteError, "upstream stopped accepting the upload");
            }
            *data = Bytes::new();
            Ok(())
        }

        async fn finish(&mut self) -> Result<()> {
            Ok(())
        }

        async fn cleanup(&mut self) -> Result<()> {
            panic!("a failed upstream writer must be abandoned, not finished again")
        }

        fn upgrade_body_writer(&mut self) {}
    }

    /// Read the `101`, send a frame once switched unless the upload already failed, and read the
    /// upstream's frame that follows the upload failure.
    async fn upgraded_client(client: &mut tokio::io::DuplexStream, case: UpgradedUploadFailure) {
        let mut head = Vec::new();
        while !head.ends_with(b"\r\n\r\n") {
            head.push(client.read_u8().await.unwrap());
        }
        assert!(head.starts_with(b"HTTP/1.1 101 "), "{head:?}");
        if !case.uploads_before_switch() {
            client.write_all(b"probe").await.unwrap();
        }
        let mut frame = [0; 6];
        client.read_exact(&mut frame).await.unwrap();
        assert_eq!(&frame, b"server");
    }

    async fn h1_upgraded_upload(case: UpgradedUploadFailure) -> Option<Box<Error>> {
        let (mut session, mut client) = upgrade_session().await;
        let mut request = session.req_header().clone();
        crate::proxy_common::finalize_h1_upstream_request_framing(&mut request, true).unwrap();
        let request_wire =
            pingora_core::protocols::http::v1::client::http_req_header_to_wire(&request).unwrap();
        let upstream_io = {
            let broken_pipe = || std::io::Error::from(std::io::ErrorKind::BrokenPipe);
            let mut script = tokio_test::io::Builder::new();
            script.write(&request_wire);
            if case.uploads_before_switch() {
                script.write_error(broken_pipe());
                script.read(SWITCHING_PROTOCOLS);
            } else {
                script.write(b"u");
                script.read(SWITCHING_PROTOCOLS);
                script.write_error(broken_pipe());
            }
            script.read(b"server");
            if !case.response_completes() {
                script.read_error(std::io::Error::from(std::io::ErrorKind::ConnectionReset));
            }
            script.build()
        };
        let mut upstream = HttpSessionV1::new(Box::new(upstream_io));
        let peer = HttpPeer::new(("127.0.0.1", 1), false, String::new());
        let proxy = HttpProxy::new(TestProxy, Arc::new(ServerConf::default()));
        let mut ctx = ();
        let ((_, _, error), ()) = tokio::join!(
            proxy.proxy_1to1(&mut session, &mut upstream, &peer, &mut ctx),
            upgraded_client(&mut client, case),
        );
        error
    }

    async fn custom_upgraded_upload(case: UpgradedUploadFailure) -> Result<bool> {
        let (mut session, mut client) = upgrade_session().await;
        session.set_upstream_h1_upgrade_request_status(true);
        let failed = Arc::new(tokio::sync::Notify::new());
        let writer = Box::new(UpgradedUploadWriter {
            fails_first_write: case.uploads_before_switch(),
            writes: 0,
            failed: failed.clone(),
        });
        let (tx, rx) = mpsc::channel(TASK_BUFFER_SIZE);
        let producer = async {
            let mut switching = ResponseHeader::build(101, None).unwrap();
            switching.insert_header("connection", "upgrade").unwrap();
            switching.insert_header("upgrade", "websocket").unwrap();
            if case.uploads_before_switch() {
                failed.notified().await;
            }
            let _ = tx.send(HttpTask::Header(Box::new(switching), false)).await;
            if !case.uploads_before_switch() {
                failed.notified().await;
            }
            let frame = HttpTask::UpgradedBody(Some(Bytes::from_static(b"server")), false);
            let _ = tx.send(frame).await;
            let end = if case.response_completes() {
                HttpTask::Done
            } else {
                HttpTask::Failed(
                    Error::explain(ReadError, "upgraded response read failed").into_up(),
                )
            };
            let _ = tx.send(end).await;
            drop(tx);
        };
        let (result, (), ()) = tokio::join!(
            proxy_upload_with(&mut session, rx, writer),
            producer,
            upgraded_client(&mut client, case),
        );
        result
    }

    async fn assert_upgraded_upload_failure_matches_h1(case: UpgradedUploadFailure) {
        let h1_error = tokio::time::timeout(Duration::from_secs(3), h1_upgraded_upload(case))
            .await
            .unwrap_or_else(|_| panic!("H1 did not finish the upgraded exchange: {case:?}"));
        let custom_result =
            tokio::time::timeout(Duration::from_secs(3), custom_upgraded_upload(case))
                .await
                .unwrap_or_else(|_| {
                    panic!("custom did not finish the upgraded exchange: {case:?}")
                });
        if case.response_completes() {
            assert!(h1_error.is_none(), "H1 {case:?}: {h1_error:?}");
            assert!(custom_result.is_ok(), "custom {case:?}: {custom_result:?}");
            return;
        }
        let h1_error = h1_error.expect("H1 must report the failed exchange");
        let custom_error = custom_result.expect_err("custom must report the failed exchange");
        for error in [h1_error, custom_error] {
            assert_eq!(
                error.etype(),
                &WriteError,
                "the upload error must win: {error}"
            );
            assert_eq!(error.esource, ErrorSource::Upstream);
        }
    }

    #[tokio::test]
    async fn upgraded_upload_failure_after_switch_relays_until_upstream_ends() {
        assert_upgraded_upload_failure_matches_h1(UpgradedUploadFailure::AfterSwitch).await;
    }

    #[tokio::test]
    async fn upgraded_upload_failure_before_switch_relays_until_upstream_ends() {
        assert_upgraded_upload_failure_matches_h1(UpgradedUploadFailure::BeforeSwitch).await;
    }

    #[tokio::test]
    async fn upgraded_upload_failure_is_reported_when_upstream_read_fails() {
        assert_upgraded_upload_failure_matches_h1(UpgradedUploadFailure::ThenResponseReadFails)
            .await;
    }

    #[tokio::test(start_paused = true)]
    async fn upgraded_downstream_half_close_after_upload_failure_matches_h1() {
        tokio::time::timeout(Duration::from_secs(3), async {
            let (mut h1_session, mut h1_client) = upgrade_session().await;
            let mut request = h1_session.req_header().clone();
            crate::proxy_common::finalize_h1_upstream_request_framing(&mut request, true).unwrap();
            let request_wire =
                pingora_core::protocols::http::v1::client::http_req_header_to_wire(&request)
                    .unwrap();
            let upstream_io = tokio_test::io::Builder::new()
                .write(&request_wire)
                .write(b"u")
                .read(SWITCHING_PROTOCOLS)
                .write_error(std::io::Error::from(std::io::ErrorKind::BrokenPipe))
                .read(b"server")
                .wait(Duration::from_millis(100))
                .read_error(std::io::Error::from(std::io::ErrorKind::ConnectionReset))
                .build();
            let mut upstream = HttpSessionV1::new(Box::new(upstream_io));
            let peer = HttpPeer::new(("127.0.0.1", 1), false, String::new());
            let proxy = HttpProxy::new(TestProxy, Arc::new(ServerConf::default()));
            let mut ctx = ();
            let ((_, _, h1_error), ()) = tokio::join!(
                proxy.proxy_1to1(&mut h1_session, &mut upstream, &peer, &mut ctx),
                async {
                    upgraded_client(&mut h1_client, UpgradedUploadFailure::AfterSwitch).await;
                    h1_client.shutdown().await.unwrap();
                },
            );
            assert!(h1_error.is_none(), "H1: {h1_error:?}");

            let (mut custom_session, mut custom_client) = upgrade_session().await;
            custom_session.set_upstream_h1_upgrade_request_status(true);
            let failed = Arc::new(tokio::sync::Notify::new());
            let writer = Box::new(UpgradedUploadWriter {
                fails_first_write: false,
                writes: 0,
                failed: failed.clone(),
            });
            let (tx, rx) = mpsc::channel(TASK_BUFFER_SIZE);
            let (closed_tx, closed_rx) = oneshot::channel();
            let producer = async {
                let mut switching = ResponseHeader::build(101, None).unwrap();
                switching.insert_header("connection", "upgrade").unwrap();
                switching.insert_header("upgrade", "websocket").unwrap();
                tx.send(HttpTask::Header(Box::new(switching), false))
                    .await
                    .unwrap();
                failed.notified().await;
                tx.send(HttpTask::UpgradedBody(
                    Some(Bytes::from_static(b"server")),
                    false,
                ))
                .await
                .unwrap();
                let _ = closed_rx.await;
                tokio::time::sleep(Duration::from_millis(100)).await;
                let _ = tx
                    .send(HttpTask::Failed(
                        Error::explain(ReadError, "upgraded response read failed").into_up(),
                    ))
                    .await;
            };
            let (custom_result, (), ()) = tokio::join!(
                proxy_upload_with(&mut custom_session, rx, writer),
                producer,
                async {
                    upgraded_client(&mut custom_client, UpgradedUploadFailure::AfterSwitch).await;
                    custom_client.shutdown().await.unwrap();
                    let _ = closed_tx.send(());
                },
            );
            assert!(
                matches!(custom_result, Ok(true)),
                "custom: {custom_result:?}"
            );
        })
        .await
        .expect("upgraded half-close must survive a later upstream read failure");
    }

    async fn upload_result(tasks: Vec<HttpTask>, reject_body: bool) -> Result<bool> {
        let (mut session, mut client) = upload_session().await;
        if reject_body {
            session
                .req_header_mut()
                .insert_header("x-test-reject-body", "1")
                .unwrap();
        }
        client.write_all(b"u").await.unwrap();
        let failed = Arc::new(tokio::sync::Notify::new());
        let (tx, rx) = mpsc::channel(TASK_BUFFER_SIZE);
        let producer = async {
            if !reject_body {
                failed.notified().await;
            }
            for task in tasks {
                let wait_for_body =
                    matches!(&task, HttpTask::Body(Some(data), true) if data.len() == 200);
                if tx.send(task).await.is_err() {
                    break;
                }
                if wait_for_body {
                    // Publish trailer failure only after the full body arrived.
                    read_rejection(&mut client, 413).await;
                }
            }
            drop(tx);
        };
        let (result, ()) = tokio::time::timeout(Duration::from_secs(1), async {
            tokio::join!(proxy_upload(&mut session, rx, failed.clone()), producer)
        })
        .await
        .expect("failed response must fail promptly");
        result
    }

    #[tokio::test]
    async fn custom_early_response_requires_complete_response() {
        for tasks in [
            vec![],
            vec![response_head(100, 0)],
            vec![
                response_head(413, 200),
                HttpTask::Body(Some(Bytes::from_static(b"partial")), false),
            ],
        ] {
            let error = upload_result(tasks, false)
                .await
                .expect_err("incomplete response must fail");
            assert!(
                error
                    .to_string()
                    .contains("upstream stopped accepting the upload"),
                "{error}"
            );
            assert_eq!(error.esource, ErrorSource::Upstream);
        }
    }

    #[tokio::test]
    async fn custom_early_response_reports_upload_error_over_response_error() {
        let error = upload_result(
            vec![
                response_head(413, 200),
                HttpTask::Body(Some(Bytes::from_static(b"partial")), false),
                HttpTask::Failed(Error::explain(ReadError, "response failed").into_up()),
            ],
            false,
        )
        .await
        .expect_err("incomplete response must fail");
        assert_eq!(error.etype(), &WriteError);
        assert!(
            error
                .to_string()
                .contains("upstream stopped accepting the upload"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn custom_observed_response_failure_is_not_hidden_by_complete_body() {
        // These tasks are queued together, so the failure is observed before delivery completes.
        let error = upload_result(
            vec![
                response_head(413, 200),
                HttpTask::Body(Some(Bytes::from(vec![b'x'; 200])), false),
                HttpTask::Failed(Error::explain(ReadError, "response failed").into_up()),
            ],
            false,
        )
        .await
        .expect_err("an observed response failure must not be hidden");
        assert_eq!(error.etype(), &WriteError);
    }

    #[tokio::test]
    async fn custom_completed_response_does_not_wait_for_late_trailer_failure() {
        let result = upload_result(
            vec![
                response_head(413, 200),
                HttpTask::Body(Some(Bytes::from(vec![b'x'; 200])), true),
                HttpTask::Failed(Error::explain(ReadError, "late trailer failure").into_up()),
            ],
            false,
        )
        .await;
        assert!(matches!(result, Ok(true)), "{result:?}");
    }

    #[tokio::test]
    async fn custom_early_response_preserves_request_filter_error() {
        let error = upload_result(vec![response_head(200, 0), HttpTask::Done], true)
            .await
            .expect_err("request filter rejection must fail");
        assert_eq!(error.etype, HTTPStatus(413));
        assert!(
            error.to_string().contains("request body policy rejected"),
            "{error}"
        );
    }

    #[async_trait]
    impl BodyWrite for TestBodyWriter {
        async fn write_all_buf(&mut self, data: &mut Bytes) -> Result<()> {
            *data = Bytes::new();
            Ok(())
        }

        async fn finish(&mut self) -> Result<()> {
            if let Some(notify) = self.finish_notify.as_ref() {
                notify.notify_one();
            }
            Ok(())
        }

        async fn cleanup(&mut self) -> Result<()> {
            self.cleanup_called.store(true, Ordering::Release);
            Ok(())
        }

        fn upgrade_body_writer(&mut self) {}
    }

    #[async_trait]
    impl ProxyHttp for TestProxy {
        type CTX = ();

        fn new_ctx(&self) -> Self::CTX {}

        async fn request_body_filter(
            &self,
            session: &mut Session,
            _body: &mut Option<Bytes>,
            _end: bool,
            _ctx: &mut (),
        ) -> Result<()> {
            if session
                .req_header()
                .headers
                .contains_key("x-test-reject-body")
            {
                Error::e_explain(HTTPStatus(413), "request body policy rejected the upload")
            } else {
                Ok(())
            }
        }

        async fn upstream_peer(
            &self,
            _session: &mut Session,
            _ctx: &mut Self::CTX,
        ) -> Result<Box<HttpPeer>> {
            unreachable!("test does not select an upstream peer")
        }
    }

    async fn predicted_too_large_session(key: CacheKey, max_file_size: usize) -> Session {
        let (mut client, server) = tokio::io::duplex(1024);
        client
            .write_all(b"GET / HTTP/1.1\r\nHost: example.com\r\n\r\n")
            .await
            .expect("test request should be written");

        let mut session = Session::new_h1(Box::new(server) as pingora_core::protocols::Stream);
        session
            .read_request()
            .await
            .expect("test request should parse");
        session
            .cache
            .enable(&*CACHE_STORAGE, None, Some(&*CACHE_PREDICTOR), None, None);
        session.cache.set_cache_key(key.clone());
        session.cache.set_max_file_size_bytes(max_file_size);
        CACHE_PREDICTOR.mark_uncacheable(&key, NoCacheReason::OriginNotCache);
        session
            .cache
            .disable(NoCacheReason::PredictedResponseTooLarge);
        session
    }

    async fn filter_task(session: &mut Session, task: HttpTask) {
        let proxy = HttpProxy::new(TestProxy, Arc::new(ServerConf::default()));
        proxy
            .custom_response_filter(
                session,
                task,
                &mut (),
                &mut ServeFromCache::new(),
                &mut RangeBodyFilter::new(),
                false,
            )
            .await
            .expect("response task should pass filters");
    }

    async fn run_completed_response(upgraded: bool) -> CompletedResponse {
        let (mut client, server) = tokio::io::duplex(4096);
        let request: &[u8] = if upgraded {
            b"GET / HTTP/1.1\r\nHost: example.com\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\n"
        } else {
            b"GET / HTTP/1.1\r\nHost: example.com\r\n\r\n"
        };
        client
            .write_all(request)
            .await
            .expect("test request should be written");

        let mut session = Session::new_h1(Box::new(server) as pingora_core::protocols::Stream);
        session
            .read_request()
            .await
            .expect("test request should parse");
        session.set_upstream_h1_upgrade_request_status(upgraded);

        let proxy = HttpProxy::new(TestProxy, Arc::new(ServerConf::default()));
        let cleanup_called = Arc::new(AtomicBool::new(false));
        let mut client_body: Box<dyn BodyWrite> = Box::new(TestBodyWriter {
            cleanup_called: cleanup_called.clone(),
            finish_notify: None,
        });

        let (response_tx, response_rx) = mpsc::channel(TASK_BUFFER_SIZE);
        let status = if upgraded {
            http::StatusCode::SWITCHING_PROTOCOLS
        } else {
            http::StatusCode::OK
        };
        let mut response_header =
            ResponseHeader::build(status, Some(2)).expect("response should build");
        if upgraded {
            response_header
                .insert_header(http::header::UPGRADE, "websocket")
                .expect("upgrade header should be valid");
            response_header
                .insert_header(http::header::CONNECTION, "Upgrade")
                .expect("connection header should be valid");
        }
        response_tx
            .send(HttpTask::Header(Box::new(response_header), !upgraded))
            .await
            .expect("response header should be queued");
        if upgraded {
            response_tx
                .send(HttpTask::UpgradedBody(
                    Some(Bytes::from_static(b"hello")),
                    true,
                ))
                .await
                .expect("upgraded body should be queued");
        }
        drop(response_tx);

        let (upstream_custom_tx, upstream_custom_rx) = mpsc::channel(1);
        let (downstream_custom_tx, downstream_custom_rx) = mpsc::channel(1);
        let (release_custom_tx, release_custom_rx) = oneshot::channel();
        let response_marker: &[u8] = if upgraded { b"hello" } else { b"\r\n\r\n" };
        let client_task = tokio::spawn(async move {
            let mut response = Vec::new();
            let mut buf = [0; 1024];
            while !response
                .windows(response_marker.len())
                .any(|part| part == response_marker)
            {
                let read = client
                    .read(&mut buf)
                    .await
                    .expect("response should be readable");
                assert!(read > 0, "response ended before expected data arrived");
                response.extend_from_slice(&buf[..read]);
            }

            client
                .write_all(b"late")
                .await
                .expect("late downstream bytes should be written");
            if !upgraded {
                let _ = release_custom_rx.await;
            }
            drop(upstream_custom_tx);
            drop(downstream_custom_tx);
            response
        });

        let (cancel_downstream_reader_tx, _cancel_downstream_reader_rx) = oneshot::channel();
        let pipe_state = Arc::new(AtomicU8::new(PipeState::Active as u8));
        let result = tokio::time::timeout(
            Duration::from_secs(1),
            proxy.custom_bidirection_down_to_up(
                &mut session,
                &mut client_body,
                response_rx,
                &mut (),
                upstream_custom_rx,
                downstream_custom_rx,
                true,
                cancel_downstream_reader_tx,
                None,
                pipe_state.clone(),
            ),
        )
        .await
        .expect("custom proxy should finish after custom channels close");

        let _ = release_custom_tx.send(());
        let response = client_task.await.expect("client task should finish");
        let cleanup_called = cleanup_called.load(Ordering::Acquire);
        let downstream_complete =
            PipeState::is_downstream_complete(pipe_state.load(Ordering::Acquire));
        CompletedResponse {
            result,
            response,
            cleanup_called,
            downstream_complete,
        }
    }

    #[tokio::test]
    async fn upgraded_response_completion_stops_downstream_body_poll() {
        let CompletedResponse {
            result,
            response,
            cleanup_called,
            downstream_complete,
        } = run_completed_response(true).await;

        assert!(
            result.is_ok(),
            "completed upgraded response should not poll downstream again: {result:?}"
        );
        assert!(
            response.starts_with(b"HTTP/1.1 101 Switching Protocols\r\n"),
            "unexpected response: {:?}",
            String::from_utf8_lossy(&response)
        );
        assert!(cleanup_called);
        assert!(downstream_complete);
    }

    #[tokio::test]
    async fn upgraded_request_completion_stops_body_poll_while_response_open() {
        upgraded_request_completion_case(false).await;
    }

    #[tokio::test]
    async fn upgraded_request_finish_failure_stops_response_after_half_close() {
        upgraded_request_completion_case(true).await;
    }

    async fn upgraded_request_completion_case(fail_finish: bool) {
        let (mut client, server) = tokio::io::duplex(4096);
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
        session.set_upstream_h1_upgrade_request_status(true);

        let proxy = HttpProxy::new(TestProxy, Arc::new(ServerConf::default()));
        let cleanup_called = Arc::new(AtomicBool::new(false));
        let finish_notify = Arc::new(tokio::sync::Notify::new());
        let writer = TestBodyWriter {
            cleanup_called: cleanup_called.clone(),
            finish_notify: Some(finish_notify.clone()),
        };
        let mut client_body: Box<dyn BodyWrite> = if fail_finish {
            Box::new(FailedFinishBodyWriter(writer))
        } else {
            Box::new(writer)
        };

        let (response_tx, response_rx) = mpsc::channel(TASK_BUFFER_SIZE);
        let mut response_header =
            ResponseHeader::build(http::StatusCode::SWITCHING_PROTOCOLS, Some(2))
                .expect("upgrade response should build");
        response_header
            .insert_header(http::header::UPGRADE, "websocket")
            .expect("upgrade header should be valid");
        response_header
            .insert_header(http::header::CONNECTION, "Upgrade")
            .expect("connection header should be valid");
        response_tx
            .send(HttpTask::Header(Box::new(response_header), false))
            .await
            .expect("response header should be queued");

        let (upstream_custom_tx, upstream_custom_rx) = mpsc::channel(1);
        let (downstream_custom_tx, downstream_custom_rx) = mpsc::channel(1);
        let (cancel_downstream_reader_tx, _cancel_downstream_reader_rx) = oneshot::channel();
        let pipe_state = Arc::new(AtomicU8::new(PipeState::Active as u8));
        let mut ctx = ();
        let proxy_result = proxy.custom_bidirection_down_to_up(
            &mut session,
            &mut client_body,
            response_rx,
            &mut ctx,
            upstream_custom_rx,
            downstream_custom_rx,
            true,
            cancel_downstream_reader_tx,
            None,
            pipe_state.clone(),
        );
        tokio::pin!(proxy_result);

        let mut client_task = tokio::spawn(async move {
            let mut response = Vec::new();
            let mut buf = [0; 1024];
            while !response.windows(4).any(|part| part == b"\r\n\r\n") {
                let read = client
                    .read(&mut buf)
                    .await
                    .expect("response should be readable");
                assert!(read > 0, "response ended before headers arrived");
                response.extend_from_slice(&buf[..read]);
            }
            client
                .shutdown()
                .await
                .expect("downstream write side should close");
            client
        });

        let mut client = tokio::time::timeout(Duration::from_secs(1), async {
            tokio::select! {
                client = &mut client_task => client.expect("client task should finish"),
                result = &mut proxy_result => panic!("proxy ended before downstream half-close: {result:?}"),
            }
        })
        .await
        .expect("downstream should receive response headers");
        let request_finished = finish_notify.notified();
        tokio::pin!(request_finished);
        tokio::time::timeout(Duration::from_secs(1), async {
            tokio::select! {
                () = &mut request_finished => {},
                result = &mut proxy_result => panic!("proxy ended while finishing upgraded request: {result:?}"),
            }
        })
        .await
        .expect("custom request body writer should finish");

        match futures::poll!(&mut proxy_result) {
            std::task::Poll::Pending => {}
            std::task::Poll::Ready(result) => {
                panic!("proxy repolled completed upgraded request: {result:?}");
            }
        }

        response_tx
            .send(HttpTask::UpgradedBody(
                Some(Bytes::from_static(b"hello")),
                true,
            ))
            .await
            .expect("final upgraded body should be queued");
        drop(response_tx);
        drop(upstream_custom_tx);
        drop(downstream_custom_tx);

        let result = tokio::time::timeout(Duration::from_secs(1), &mut proxy_result)
            .await
            .expect("custom proxy should finish after response and custom channels close");
        assert!(
            result.is_ok(),
            "custom proxy should finish cleanly: {result:?}"
        );

        let mut body = [0; 5];
        if fail_finish {
            let read = client.read(&mut body);
            tokio::pin!(read);
            match futures::poll!(&mut read) {
                std::task::Poll::Pending | std::task::Poll::Ready(Ok(0)) => {}
                result => panic!("response continued after failed upgraded half-close: {result:?}"),
            }
        } else {
            tokio::time::timeout(Duration::from_secs(1), client.read_exact(&mut body))
                .await
                .expect("a successful upload must preserve the response after half-close")
                .unwrap();
            assert_eq!(&body, b"hello");
        }
        assert_eq!(cleanup_called.load(Ordering::Acquire), !fail_finish);
        assert!(PipeState::is_downstream_complete(
            pipe_state.load(Ordering::Acquire)
        ));
    }

    #[tokio::test]
    async fn completed_http_response_rejects_late_downstream_data() {
        let CompletedResponse {
            result,
            cleanup_called,
            downstream_complete,
            ..
        } = run_completed_response(false).await;
        let error = result.expect_err("late downstream data should remain an error");
        assert!(
            format!("{error:?}").contains("Sent data after end of body"),
            "unexpected error: {error:?}"
        );
        assert!(!cleanup_called);
        assert!(!downstream_complete);
    }

    #[tokio::test]
    async fn completed_response_under_limit_clears_predictor() {
        let key = CacheKey::new("/custom-under-limit", "");
        let mut session = predicted_too_large_session(key.clone(), 10).await;

        filter_task(
            &mut session,
            HttpTask::Body(Some(Bytes::from_static(b"small")), true),
        )
        .await;

        assert!(CACHE_PREDICTOR.cacheable_prediction(&key));
    }

    #[tokio::test]
    async fn completed_response_over_limit_keeps_predictor() {
        let key = CacheKey::new("/custom-over-limit", "");
        let mut session = predicted_too_large_session(key.clone(), 4).await;

        filter_task(
            &mut session,
            HttpTask::Body(Some(Bytes::from_static(b"large")), true),
        )
        .await;

        assert!(!CACHE_PREDICTOR.cacheable_prediction(&key));
    }

    #[tokio::test]
    async fn failed_response_keeps_predictor() {
        let key = CacheKey::new("/custom-failed", "");
        let mut session = predicted_too_large_session(key.clone(), 10).await;

        filter_task(
            &mut session,
            HttpTask::Body(Some(Bytes::from_static(b"small")), false),
        )
        .await;
        filter_task(
            &mut session,
            HttpTask::Failed(Error::explain(InternalError, "test failure")),
        )
        .await;

        assert!(!CACHE_PREDICTOR.cacheable_prediction(&key));
    }

    // A cancellation requested once the abandoned fill is parked on its upstream read wakes the
    // loop.
    #[tokio::test]
    async fn abandoned_fill_is_cancelled_without_another_upstream_task() {
        assert_cancellation_ends_admitted_fill(
            "/custom-cancel-after-abandonment",
            AbandonmentTiming::AfterAdmission,
        )
        .await;
    }

    // A cancellation requested before the attempt starts ends the fill as soon as the writer
    // exists.
    #[tokio::test]
    async fn fill_cancelled_before_the_attempt_ends_at_admission() {
        assert_cancellation_ends_admitted_fill(
            "/custom-cancel-before-attempt",
            AbandonmentTiming::BeforeAttempt,
        )
        .await;
    }

    async fn assert_cancellation_ends_admitted_fill(path: &str, abandonment: AbandonmentTiming) {
        let (mut session, mut client) =
            cache_miss_session_on(path, &abandonment, true, &*CANCELLATION_WATCHED_STORAGE).await;
        let cancellation = CANCELLATION_WATCHED_STORAGE.cancellation_for(path);
        assert!(!cancellation.is_cancelled(), "{path} was not fresh");
        let cancel_before_attempt = matches!(abandonment, AbandonmentTiming::BeforeAttempt);
        if cancel_before_attempt {
            assert!(cancellation.cancel());
        }
        let inner = AbandonmentSignalProxy::default();
        let abandoned = Arc::clone(&inner.abandoned);
        let proxy = HttpProxy::new(inner, Arc::new(ServerConf::default()));
        let mut client_body: Box<dyn BodyWrite> = Box::new(TestBodyWriter {
            cleanup_called: Arc::new(AtomicBool::new(false)),
            finish_notify: None,
        });
        let (_upstream_filter_tx, upstream_custom_message_filter_rx) = mpsc::channel(1);
        let (_downstream_filter_tx, downstream_custom_message_filter_rx) = mpsc::channel(1);
        let (cancel_downstream_reader_tx, _cancel_downstream_reader_rx) = oneshot::channel();
        let (cancel_upstream_reader_tx, _cancel_upstream_reader_rx) = oneshot::channel();
        let (response_tx, response_rx) = mpsc::channel(TASK_BUFFER_SIZE);
        let mut ctx = ();

        let proxy_loop = proxy.custom_bidirection_down_to_up(
            &mut session,
            &mut client_body,
            response_rx,
            &mut ctx,
            upstream_custom_message_filter_rx,
            downstream_custom_message_filter_rx,
            false,
            cancel_downstream_reader_tx,
            Some(cancel_upstream_reader_tx),
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
                assert!(cancellation.cancel());
            }
            // Keep the upstream pipe open: only the cancellation may end the loop.
            response_tx
        };

        let (result, _response_tx) = tokio::time::timeout(Duration::from_secs(5), async {
            tokio::join!(proxy_loop, drive)
        })
        .await
        .expect("custom transport loop kept waiting for upstream after the cancellation");

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
    // tests above (including the downstream-abandonment flag at the `start_watching` call site),
    // but with the whole response already queued before the loop's first poll.
    #[tokio::test]
    async fn batched_complete_response_commits_despite_a_pending_cancellation() {
        let path = "/custom-cancel-before-batched-response";
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
            AbandonmentSignalProxy::default(),
            Arc::new(ServerConf::default()),
        );
        let mut client_body: Box<dyn BodyWrite> = Box::new(TestBodyWriter {
            cleanup_called: Arc::new(AtomicBool::new(false)),
            finish_notify: None,
        });
        // Nothing sends custom messages to the upstream, so their forwarder has ended.
        let (_, upstream_custom_message_filter_rx) = mpsc::channel(1);
        let (_downstream_filter_tx, downstream_custom_message_filter_rx) = mpsc::channel(1);
        let (cancel_downstream_reader_tx, _cancel_downstream_reader_rx) = oneshot::channel();
        let (cancel_upstream_reader_tx, _cancel_upstream_reader_rx) = oneshot::channel();
        let (response_tx, response_rx) = mpsc::channel(TASK_BUFFER_SIZE);
        let header = ResponseHeader::build(200, None).unwrap();
        response_tx
            .try_send(HttpTask::Header(Box::new(header), false))
            .unwrap();
        response_tx
            .try_send(HttpTask::Body(Some(Bytes::from_static(b"complete")), true))
            .unwrap();
        response_tx.try_send(HttpTask::Done).unwrap();
        drop(response_tx);
        let mut ctx = ();

        let result = tokio::time::timeout(
            Duration::from_secs(5),
            proxy.custom_bidirection_down_to_up(
                &mut session,
                &mut client_body,
                response_rx,
                &mut ctx,
                upstream_custom_message_filter_rx,
                downstream_custom_message_filter_rx,
                false,
                cancel_downstream_reader_tx,
                Some(cancel_upstream_reader_tx),
                Arc::new(AtomicU8::new(PipeState::Active as u8)),
            ),
        )
        .await
        .expect("custom transport loop did not finish the queued response");

        assert!(
            !result.unwrap(),
            "a complete batched response must end the loop normally"
        );
        assert!(
            CANCELLATION_WATCHED_STORAGE.is_committed(path).await,
            "a response that was already complete was discarded for an older cancellation"
        );
    }

    // A custom upstream ends a response with `Done` after its end-of-stream body, and the
    // abandoned loop waits for that `Done`. A cancellation that arrives after the admission has
    // already committed, while the loop waits, must not fail the request.
    #[tokio::test]
    async fn cancellation_after_the_admission_committed_does_not_fail_the_request() {
        // Polls the loop once, which runs it until it waits on the upstream again: the storage
        // is in memory and the abandoned downstream is never polled, so nothing it awaits is
        // real I/O.
        async fn assert_loop_waits<F: Future<Output = Result<bool>>>(
            proxy_loop: &mut std::pin::Pin<&mut F>,
            why: &str,
        ) {
            if let std::task::Poll::Ready(result) = futures::poll!(proxy_loop.as_mut()) {
                panic!("{why}: {result:?}");
            }
        }

        let path = "/custom-cancel-after-commit";
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
        // The storage drops a fill's cancellation only once its writer's `finish()` succeeds.
        // `is_committed` cannot tell: it also finds the entry while the fill is in progress.
        let admission_committed = || {
            !Arc::ptr_eq(
                &cancellation,
                &CANCELLATION_WATCHED_STORAGE.cancellation_for(path),
            )
        };
        let proxy = HttpProxy::new(
            AbandonmentSignalProxy::default(),
            Arc::new(ServerConf::default()),
        );
        let mut client_body: Box<dyn BodyWrite> = Box::new(TestBodyWriter {
            cleanup_called: Arc::new(AtomicBool::new(false)),
            finish_notify: None,
        });
        // Nothing sends custom messages to the upstream, so their forwarder has ended.
        let (_, upstream_custom_message_filter_rx) = mpsc::channel(1);
        let (_downstream_filter_tx, downstream_custom_message_filter_rx) = mpsc::channel(1);
        let (cancel_downstream_reader_tx, _cancel_downstream_reader_rx) = oneshot::channel();
        let (cancel_upstream_reader_tx, _cancel_upstream_reader_rx) = oneshot::channel();
        let (response_tx, response_rx) = mpsc::channel(TASK_BUFFER_SIZE);
        let mut ctx = ();

        let proxy_loop = proxy.custom_bidirection_down_to_up(
            &mut session,
            &mut client_body,
            response_rx,
            &mut ctx,
            upstream_custom_message_filter_rx,
            downstream_custom_message_filter_rx,
            false,
            cancel_downstream_reader_tx,
            Some(cancel_upstream_reader_tx),
            Arc::new(AtomicU8::new(PipeState::Active as u8)),
        );
        tokio::pin!(proxy_loop);

        let header = ResponseHeader::build(200, None).unwrap();
        response_tx
            .try_send(HttpTask::Header(Box::new(header), false))
            .unwrap();
        response_tx
            .try_send(HttpTask::Body(Some(Bytes::from_static(b"first ")), false))
            .unwrap();
        // The admission is now active, so the loop watches its writer.
        assert_loop_waits(&mut proxy_loop, "the loop ended before the response did").await;
        assert!(
            !admission_committed(),
            "the admission committed before the end-of-stream body"
        );

        response_tx
            .try_send(HttpTask::Body(Some(Bytes::from_static(b"last")), true))
            .unwrap();
        assert_loop_waits(
            &mut proxy_loop,
            "the loop did not wait for the upstream's `Done`",
        )
        .await;
        assert!(
            admission_committed(),
            "the end-of-stream body did not commit the admission"
        );

        assert!(cancellation.cancel());
        assert_loop_waits(
            &mut proxy_loop,
            "a cancellation after the commit ended the loop",
        )
        .await;

        response_tx.try_send(HttpTask::Done).unwrap();
        let result = tokio::time::timeout(Duration::from_secs(5), &mut proxy_loop)
            .await
            .expect("custom transport loop did not finish the committed response");

        assert!(
            !result.expect("a cancellation after the commit failed the request"),
            "a committed response must end the loop normally"
        );
        assert!(CANCELLATION_WATCHED_STORAGE.is_committed(path).await);
    }
}
