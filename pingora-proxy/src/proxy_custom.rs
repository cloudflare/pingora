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
use proxy_common::{DownstreamStateMachine, PipeState, ResponseStateMachine};
use tokio::sync::oneshot;

use super::*;

impl<SV, C> HttpProxy<SV, C>
where
    C: custom::Connector,
{
    /// Proxy to a custom protocol upstream.
    /// Returns (reuse_server, error)
    pub(crate) async fn proxy_to_custom_upstream(
        &self,
        session: &mut Session,
        client_session: &mut C::Session,
        reused: bool,
        peer: &HttpPeer,
        ctx: &mut SV::CTX,
    ) -> (bool, Option<Box<Error>>)
    where
        SV: ProxyHttp + Send + Sync,
        SV::CTX: Send + Sync,
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
            .custom_proxy_down_to_up(session, client_session, peer, ctx)
            .await;

        // Parity with H1/H2: custom upstreams don't report payload bytes; record 0.
        session.set_upstream_body_bytes_received(0);

        (server_session_reuse, error)
    }

    /// Handle custom protocol proxying from downstream to upstream.
    /// Returns (reuse_server, error)
    async fn custom_proxy_down_to_up(
        &self,
        session: &mut Session,
        client_session: &mut C::Session,
        peer: &HttpPeer,
        ctx: &mut SV::CTX,
    ) -> (bool, Option<Box<Error>>)
    where
        SV: ProxyHttp + Send + Sync,
        SV::CTX: Send + Sync,
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

        let Some(mut upstream_custom_message_reader) = client_session.take_custom_message_reader()
        else {
            return (
                false,
                Some(Error::explain(
                    ReadError,
                    "can't extract custom reader from upstream",
                )),
            );
        };

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
        // `_cancel_upstream_reader_tx` is unused (prefixed with _) - it will be dropped at the
        // end of this scope, which automatically signals cancellation to the upstream reader.
        let (cancel_downstream_reader_tx, cancel_downstream_reader_rx) = oneshot::channel();
        let (_cancel_upstream_reader_tx, cancel_upstream_reader_rx) = oneshot::channel();

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
        };

        let downstream_custom_message_forwarder = CustomMessageForwarder {
            ctx: "up_to_down".into(),
            reader: &mut upstream_custom_message_reader,
            writer: &mut downstream_custom_message_writer,
            filter: downstream_custom_message_filter_tx,
            inject: downstream_custom_message_inject_rx,
            cancel: cancel_upstream_reader_rx,
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
        session: &mut Session,
        ctx: &mut SV::CTX,
        initial_task: HttpTask,
        rx: &mut mpsc::Receiver<HttpTask>,
        serve_from_cache: &mut ServeFromCache,
        range_body_filter: &mut proxy_cache::range_filter::RangeBodyFilter,
        response_state: &mut ResponseStateMachine,
    ) -> Result<Option<bool>>
    where
        SV: ProxyHttp + Send + Sync,
        SV::CTX: Send + Sync,
    {
        if serve_from_cache.should_discard_upstream() {
            // Serving the cached response and discarding the upstream one; nothing
            // is written downstream this round, so return None and let the caller
            // continue.
            return Ok(None);
        }

        // Batch: pull as many tasks as we can from rx
        let mut tasks = Vec::with_capacity(TASK_BUFFER_SIZE);
        tasks.push(initial_task);
        while let Ok(task) = rx.try_recv() {
            tasks.push(task);
        }

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
            if !serve_from_cache.should_send_to_downstream() {
                if let HttpTask::Failed(e) = t {
                    return Err(e);
                }
            }
            filtered_tasks.push(
                self.custom_response_filter(
                    session,
                    t,
                    ctx,
                    serve_from_cache,
                    range_body_filter,
                    false,
                )
                .await?,
            );
            if serve_from_cache.is_miss_header() {
                response_state.enable_cached_response();
            }
        }

        if !serve_from_cache.should_send_to_downstream() {
            // TODO: need to derive response_done from filtered_tasks in case downstream failed already
            return Ok(None);
        }

        let response_done = session.write_response_tasks(filtered_tasks).await?;

        Ok(Some(response_done))
    }

    // TODO: pre-existing inconsistency with proxy_h1/proxy_h2 to address in a follow-up:
    // upstream task rx.recv() branch is missing
    // downstream_state.maybe_finished(session.is_body_done()) after processing. proxy_h1 has
    // this because upgrade responses can force the body done — since custom upstreams can
    // serve H1 downstreams that support upgrades, the same may be needed here.
    // Returns whether server (downstream) session can be reused
    #[allow(clippy::too_many_arguments)]
    async fn custom_bidirection_down_to_up(
        &self,
        session: &mut Session,
        client_body: &mut Box<dyn BodyWrite>,
        mut rx: mpsc::Receiver<HttpTask>,
        ctx: &mut SV::CTX,
        mut upstream_custom_message_filter_rx: mpsc::Receiver<(
            Bytes,
            oneshot::Sender<Option<Bytes>>,
        )>,
        mut downstream_custom_message_filter_rx: mpsc::Receiver<(
            Bytes,
            oneshot::Sender<Option<Bytes>>,
        )>,
        downstream_custom_final_hop: bool,
        cancel_downstream_reader_tx: oneshot::Sender<()>,
        pipe_state: Arc<AtomicU8>,
    ) -> Result<bool>
    where
        SV: ProxyHttp + Send + Sync,
        SV::CTX: Send + Sync,
    {
        let mut cancel_downstream_reader_tx = Some(cancel_downstream_reader_tx);

        let mut downstream_state = DownstreamStateMachine::new(session.as_mut().is_body_done());

        // retry, send buffer if it exists
        if let Some(buffer) = session.as_mut().get_retry_buffer() {
            self.send_body_to_custom(
                session,
                Some(buffer),
                downstream_state.is_done(),
                client_body,
                ctx,
            )
            .await?;
        }

        let mut response_state = ResponseStateMachine::new();

        // these two below can be wrapped into an internal ctx
        // use cache when upstream revalidates (or TODO: error)
        let mut serve_from_cache = ServeFromCache::new();
        let mut range_body_filter = proxy_cache::range_filter::RangeBodyFilter::new();

        let mut next_upstream_task: Option<HttpTask> = None;

        let mut upstream_custom = true;
        let mut downstream_custom = true;

        /* duplex mode
         * see the Same function for h1 for more comments
         */
        while !downstream_state.is_done()
            || !response_state.is_done()
            || upstream_custom
            || downstream_custom
        {
            // partial read support, this check will also be false if cache is disabled.
            let support_cache_partial_read =
                session.cache.support_streaming_partial_write() == Some(true);
            let upgraded = session.was_upgraded();

            tokio::select! {
                body = session.downstream_session.read_body_or_idle(downstream_state.is_done()), if downstream_state.can_poll() => {
                    let body = match body {
                        Ok(b) => b,
                        Err(e) => {
                            let wait_for_cache_fill = (!serve_from_cache.is_on() && support_cache_partial_read)
                                || serve_from_cache.is_miss();
                            if wait_for_cache_fill {
                                // ignore downstream error so that upstream can continue to write cache
                                downstream_state.to_errored();
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

                    match self.send_body_to_custom(session, body, is_body_done, client_body, ctx).await {
                        Ok(request_done) =>  {
                            downstream_state.maybe_finished(request_done);
                        },
                        Err(e) => {
                            // mark request done, attempt to drain receive
                            warn!("body send error: {e}");

                            // upstream is what actually errored but we don't want to continue
                            // polling the downstream body
                            downstream_state.to_errored();

                            // downstream still trying to send something, but the upstream is already stooped
                            // cancel the custom downstream to upstream coroutine, because the proxy will not see EOS.
                            let _ = cancel_downstream_reader_tx.take().expect("cancel must be set and called once").send(());
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
                        ).await? else {
                            // nothing sent downstream e.g. serve_from_cache
                            continue;
                        };
                        response_state.maybe_set_upstream_done(response_done);
                    } else {
                        debug!("empty upstream event");
                        response_state.maybe_set_upstream_done(true);
                    }
                },

                task = rx.recv(), if !response_state.upstream_done() && next_upstream_task.is_none() => {
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
                        response_state.maybe_set_upstream_done(response_done);
                    } else {
                        debug!("empty upstream event");
                        response_state.maybe_set_upstream_done(true);
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
                                downstream_state.to_errored();
                                response_state.maybe_set_cache_done(true);
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
                                    downstream_state.to_errored();
                                    response_state.maybe_set_cache_done(true);
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
                        upstream_task = rx.recv(), if !response_state.upstream_done() && serve_from_cache.is_on() && next_upstream_task.is_none() => {
                            if let Some(t) = upstream_task {
                                next_upstream_task = Some(t);
                                continue;
                            } else {
                                response_state.maybe_set_upstream_done(true);
                            }
                        }
                    }
                }

                ret = upstream_custom_message_filter_rx.recv(), if upstream_custom => {
                    let Some(msg) = ret else {
                        debug!("upstream_custom_message_filter_rx: custom downstream to upstream exited on reading");
                        upstream_custom = false;
                        continue;
                    };

                    let (data, callback) = msg;

                    let new_msg = self.inner
                        .downstream_custom_message_proxy_filter(session, data, ctx, false)  // false because the upstream is custom
                        .await?;

                    if callback.send(new_msg).is_err() {
                        debug!("upstream_custom_message_incoming_rx: custom downstream to upstream exited on callback");
                        upstream_custom = false;
                        continue;
                    };
                },

                ret = downstream_custom_message_filter_rx.recv(), if downstream_custom => {
                    let Some(msg) = ret else {
                        debug!("downstream_custom_message_filter_rx: custom upstream to downstream exited on reading");
                        downstream_custom = false;
                        continue;
                    };

                    let (data, callback) = msg;

                    let new_msg = self.inner
                        .upstream_custom_message_proxy_filter(session, data, ctx, downstream_custom_final_hop)
                        .await?;

                    if callback.send(new_msg).is_err() {
                        debug!("downstream_custom_message_filter_rx: custom upstream to downstream exited on callback");
                        downstream_custom = false;
                        continue
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
        if downstream_state.is_errored() {
            let err = Error::e_explain(WriteError, "downstream_state is_errored");
            error!("custom_bidirection_down_to_up: downstream_state.is_errored",);
            return err;
        }

        client_body.cleanup().await?;

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

    async fn custom_response_filter(
        &self,
        session: &mut Session,
        mut task: HttpTask,
        ctx: &mut SV::CTX,
        serve_from_cache: &mut ServeFromCache,
        range_body_filter: &mut RangeBodyFilter,
        from_cache: bool, // are the task from cache already
    ) -> Result<HttpTask>
    where
        SV: ProxyHttp + Send + Sync,
        SV::CTX: Send + Sync,
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
                    session.cache.disable(NoCacheReason::StorageError);
                    if serve_from_cache.is_miss_body() {
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
            if !serve_from_cache.should_send_to_downstream() {
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
                    .response_body_filter(session, &mut data, eos, ctx)?
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
                    .response_body_filter(session, &mut data, eos, ctx)?
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
        session: &mut Session,
        mut data: Option<Bytes>,
        end_of_body: bool,
        client_body: &mut Box<dyn BodyWrite>,
        ctx: &mut SV::CTX,
    ) -> Result<bool>
    where
        SV: ProxyHttp + Send + Sync,
        SV::CTX: Send + Sync,
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
            return Ok(false);
        }

        if let Some(mut data) = data {
            client_body
                .write_all_buf(&mut data)
                .await
                .map_err(|e| e.into_up())?;
            if end_of_body {
                client_body.finish().await.map_err(|e| e.into_up())?;
            }
        } else {
            debug!("Read downstream body done");
            client_body
                .finish()
                .await
                .map_err(|e| {
                    Error::because(WriteError, "while shutdown send data stream on no data", e)
                })
                .map_err(|e| e.into_up())?;
        }

        Ok(end_of_body)
    }
}

/* Read response header, body and trailer from custom upstream and send them to tx */
async fn custom_pipe_up_to_down_response<S: CustomSession>(
    client: &mut S,
    tx: mpsc::Sender<HttpTask>,
    pipe_state: Arc<AtomicU8>,
) -> Result<()> {
    let mut is_informational = true;
    while is_informational {
        client
            .read_response_header()
            .await
            .map_err(|e| e.into_up())?;
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

struct CustomMessageForwarder<'a> {
    ctx: ImmutStr,
    writer: &'a mut Box<dyn CustomMessageWrite>,
    reader:
        &'a mut Box<dyn futures::Stream<Item = Result<Bytes, Box<Error>>> + Send + Sync + Unpin>,
    inject: mpsc::Receiver<Bytes>,
    filter: mpsc::Sender<(Bytes, oneshot::Sender<Option<Bytes>>)>,
    cancel: oneshot::Receiver<()>,
}

impl CustomMessageForwarder<'_> {
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

        tokio::select! {
            ret = &mut self.cancel => {
                debug!("{}: CustomMessageForwarder: canceled while waiting for new messages: {ret:?}", self.ctx);
                Ok(())
            },
            ret = forwarder => ret
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pingora_cache::{
        predictor::{CacheablePredictor, Predictor},
        CacheKey, MemCache,
    };
    use std::sync::{Arc, LazyLock};
    use tokio::io::AsyncWriteExt;

    static CACHE_STORAGE: LazyLock<MemCache> = LazyLock::new(MemCache::new);
    static CACHE_PREDICTOR: LazyLock<Predictor<1>> = LazyLock::new(|| Predictor::new(10, None));

    struct TestProxy;

    #[async_trait]
    impl ProxyHttp for TestProxy {
        type CTX = ();

        fn new_ctx(&self) -> Self::CTX {}

        async fn upstream_peer(
            &self,
            _session: &mut Session,
            _ctx: &mut Self::CTX,
        ) -> Result<Box<HttpPeer>> {
            unreachable!("test calls custom_response_filter directly")
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
}
