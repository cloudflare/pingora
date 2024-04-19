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
use crate::proxy_cache::{range_filter::RangeBodyFilter, ServeFromCache};
use crate::proxy_common::*;
use pingora_core::protocols::http::v2::client::{write_body, Http2Session};

// add scheme and authority as required by h2 lib
fn update_h2_scheme_authority(header: &mut http::request::Parts, raw_host: &[u8]) -> Result<()> {
    let authority = if let Ok(s) = std::str::from_utf8(raw_host) {
        if s.starts_with('[') {
            // don't mess with ipv6 host
            s
        } else if let Some(colon) = s.find(':') {
            if s.len() == colon + 1 {
                // colon is the last char, ignore
                s
            } else if let Some(another_colon) = s[colon + 1..].find(':') {
                // try to get rid of extra port numbers
                &s[..colon + 1 + another_colon]
            } else {
                s
            }
        } else {
            s
        }
    } else {
        return Error::e_explain(
            InvalidHTTPHeader,
            format!("invalid authority from host {:?}", raw_host),
        );
    };

    let uri = http::uri::Builder::new()
        .scheme("https")
        .authority(authority)
        .path_and_query(header.uri.path_and_query().as_ref().unwrap().as_str())
        .build();
    match uri {
        Ok(uri) => {
            header.uri = uri;
            Ok(())
        }
        Err(_) => Error::e_explain(
            InvalidHTTPHeader,
            format!("invalid authority from host {}", authority),
        ),
    }
}

impl<SV> HttpProxy<SV> {
    pub(crate) async fn proxy_1to2(
        &self,
        session: &mut Session,
        client_session: &mut Http2Session,
        peer: &HttpPeer,
        ctx: &mut SV::CTX,
    ) -> (bool, Option<Box<Error>>)
    // (reuse_server, error)
    where
        SV: ProxyHttp + Send + Sync,
        SV::CTX: Send + Sync,
    {
        let mut req = session.req_header().clone();

        if req.version != Version::HTTP_2 {
            /* remove H1 specific headers */
            // https://github.com/hyperium/h2/blob/d3b9f1e36aadc1a7a6804e2f8e86d3fe4a244b4f/src/proto/streams/send.rs#L72
            req.remove_header(&http::header::TRANSFER_ENCODING);
            req.remove_header(&http::header::CONNECTION);
            req.remove_header(&http::header::UPGRADE);
            req.remove_header("keep-alive");
            req.remove_header("proxy-connection");
        }

        /* turn it into h2 */
        req.set_version(Version::HTTP_2);

        if session.cache.enabled() {
            if let Err(e) = pingora_cache::filters::upstream::request_filter(
                &mut req,
                session.cache.maybe_cache_meta(),
            ) {
                session.cache.disable(NoCacheReason::InternalError);
                warn!("cache upstream filter error {}, disabling cache", e);
            }
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

        // Remove H1 `Host` header, save it in order to add to :authority
        // We do this because certain H2 servers expect request not to have a host header.
        // The `Host` is removed after the upstream filters above for 2 reasons
        // 1. there is no API to change the :authority header
        // 2. the filter code needs to be aware of the host vs :authority across http versions otherwise
        let host = req.remove_header(&http::header::HOST);

        session.upstream_compression.request_filter(&req);
        let body_empty = session.as_mut().is_body_empty();

        let mut req: http::request::Parts = req.into();

        // H2 requires authority to be set, so copy that from H1 host if that is set
        if let Some(host) = host {
            if let Err(e) = update_h2_scheme_authority(&mut req, host.as_bytes()) {
                return (false, Some(e));
            }
        }

        debug!("Request to h2: {:?}", req);

        // don't send END_STREAM on HEADERS for no_header_eos
        let send_header_eos = !peer.options.no_header_eos && body_empty;

        let req = Box::new(RequestHeader::from(req));
        match client_session.write_request_header(req, send_header_eos) {
            Ok(v) => v,
            Err(e) => {
                return (false, Some(e.into_up()));
            }
        };

        // send END_STREAM on empty DATA frame for no_headers_eos
        if peer.options.no_header_eos && body_empty {
            match client_session.write_request_body(Bytes::new(), true) {
                Ok(()) => debug!("sent empty DATA frame to h2"),
                Err(e) => {
                    return (false, Some(e.into_up()));
                }
            };
        }

        client_session.read_timeout = peer.options.read_timeout;

        // take the body writer out of the client for easy duplex
        let mut client_body = client_session
            .take_request_body_writer()
            .expect("already send request header");

        let (tx, rx) = mpsc::channel::<HttpTask>(TASK_BUFFER_SIZE);

        session.as_mut().enable_retry_buffering();

        /* read downstream body and upstream response at the same time */

        let ret = tokio::try_join!(
            self.bidirection_1to2(session, &mut client_body, rx, ctx),
            pipe_2to1_response(client_session, tx)
        );

        match ret {
            Ok((_first, _second)) => (true, None),
            Err(e) => (false, Some(e)),
        }
    }

    pub(crate) async fn proxy_to_h2_upstream(
        &self,
        session: &mut Session,
        client_session: &mut Http2Session,
        reused: bool,
        peer: &HttpPeer,
        ctx: &mut SV::CTX,
    ) -> (bool, Option<Box<Error>>)
    where
        SV: ProxyHttp + Send + Sync,
        SV::CTX: Send + Sync,
    {
        if let Err(e) = self
            .inner
            .connected_to_upstream(
                session,
                reused,
                peer,
                client_session.fd(),
                client_session.digest(),
                ctx,
            )
            .await
        {
            return (false, Some(e));
        }

        let (server_session_reuse, error) =
            self.proxy_1to2(session, client_session, peer, ctx).await;

        (server_session_reuse, error)
    }

    async fn bidirection_1to2(
        &self,
        session: &mut Session,
        client_body: &mut h2::SendStream<bytes::Bytes>,
        mut rx: mpsc::Receiver<HttpTask>,
        ctx: &mut SV::CTX,
    ) -> Result<()>
    where
        SV: ProxyHttp + Send + Sync,
        SV::CTX: Send + Sync,
    {
        let mut downstream_state = DownstreamStateMachine::new(session.as_mut().is_body_done());

        // retry, send buffer if it exists
        if let Some(buffer) = session.as_mut().get_retry_buffer() {
            send_body_to2(Ok(Some(buffer)), downstream_state.is_done(), client_body)?;
        }

        let mut response_state = ResponseStateMachine::new();

        // these two below can be wrapped into an internal ctx
        // use cache when upstream revalidates (or TODO: error)
        let mut serve_from_cache = ServeFromCache::new();
        let mut range_body_filter = proxy_cache::range_filter::RangeBodyFilter::new();

        /* duplex mode
         * see the Same function for h1 for more comments
         */
        while !downstream_state.is_done() || !response_state.is_done() {
            // Similar logic in h1 need to reserve capacity first to avoid deadlock
            // But we don't need to do the same because the h2 client_body pipe is unbounded (never block)
            tokio::select! {
                // NOTE: cannot avoid this copy since h2 owns the buf
                body = session.downstream_session.read_body_or_idle(downstream_state.is_done()), if downstream_state.can_poll() => {
                    debug!("downstream event");
                    let body = match body {
                        Ok(b) => b,
                        Err(e) => {
                            if serve_from_cache.is_miss() {
                                // ignore downstream error so that upstream can continue to write cache
                                downstream_state.to_errored();
                                warn!(
                                    "Downstream Error ignored during caching: {}, {}",
                                    e,
                                    self.inner.request_summary(session, ctx)
                                );
                                continue;
                           } else {
                                return Err(e.into_down());
                           }
                        }
                    };
                    let request_done = send_body_to2(Ok(body), session.is_body_done(), client_body)?;
                    downstream_state.maybe_finished(request_done);
                },

                task = rx.recv(), if !response_state.upstream_done() => {
                    if let Some(t) = task {
                        debug!("upstream event: {:?}", t);
                        if serve_from_cache.should_discard_upstream() {
                            // just drain, do we need to do anything else?
                           continue;
                        }
                        // pull as many tasks as we can
                        let mut tasks = Vec::with_capacity(TASK_BUFFER_SIZE);
                        tasks.push(t);
                        while let Some(maybe_task) = rx.recv().now_or_never() {
                            if let Some(t) = maybe_task {
                                tasks.push(t);
                            } else {
                                break
                            }
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
                            session.upstream_compression.response_filter(&mut t);
                            // check error and abort
                            // otherwise the error is surfaced via write_response_tasks()
                            if !serve_from_cache.should_send_to_downstream() {
                                if let HttpTask::Failed(e) = t {
                                    return Err(e);
                                }
                            }
                            filtered_tasks.push(
                                self.h2_response_filter(session, t, ctx,
                                    &mut serve_from_cache,
                                    &mut range_body_filter, false).await?);
                            if serve_from_cache.is_miss_header() {
                                response_state.enable_cached_response();
                            }
                        }

                        if !serve_from_cache.should_send_to_downstream() {
                            // TODO: need to derive response_done from filtered_tasks in case downstream failed already
                            continue;
                        }

                        let response_done = session.write_response_tasks(filtered_tasks).await?;
                        response_state.maybe_set_upstream_done(response_done);
                    } else {
                        debug!("empty upstream event");
                        response_state.maybe_set_upstream_done(true);
                    }
                }

                task = serve_from_cache.next_http_task(&mut session.cache),
                    if !response_state.cached_done() && !downstream_state.is_errored() && serve_from_cache.is_on() => {
                    let task = self.h2_response_filter(session, task?, ctx,
                        &mut serve_from_cache,
                        &mut range_body_filter, true).await?;
                    match session.write_response_tasks(vec![task]).await {
                        Ok(b) => response_state.maybe_set_cache_done(b),
                        Err(e) => if serve_from_cache.is_miss() {
                            // give up writing to downstream but wait for upstream cache write to finish
                            downstream_state.to_errored();
                            response_state.maybe_set_cache_done(true);
                            warn!(
                                "Downstream Error ignored during caching: {}, {}",
                                e,
                                self.inner.request_summary(session, ctx)
                            );
                            continue;
                        } else {
                            return Err(e);
                        }
                    }
                    if response_state.cached_done() {
                        if let Err(e) = session.cache.finish_hit_handler().await {
                            warn!("Error during finish_hit_handler: {}", e);
                        }
                    }
                }

                else => {
                    break;
                }
            }
        }

        match session.as_mut().finish_body().await {
            Ok(_) => {
                debug!("finished sending body to downstream");
            }
            Err(e) => {
                error!("Error finish sending body to downstream: {}", e);
                // TODO: don't do downstream keepalive
            }
        }
        Ok(())
    }

    async fn h2_response_filter(
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
            self.upstream_filter(session, &mut task, ctx)?;

            // cache the original response before any downstream transformation
            // requests that bypassed cache still need to run filters to see if the response has become cacheable
            if session.cache.enabled() || session.cache.bypassing() {
                if let Err(e) = self
                    .cache_http_task(session, &task, ctx, serve_from_cache)
                    .await
                {
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

        match task {
            HttpTask::Header(mut header, eos) => {
                let req = session.req_header();

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
                        let range_type =
                            proxy_cache::range_filter::range_header_filter(req, &mut header);
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
        }
    }
}

pub(crate) fn send_body_to2(
    data: Result<Option<Bytes>>,
    end_of_body: bool,
    client_body: &mut h2::SendStream<bytes::Bytes>,
) -> Result<bool> {
    match data {
        Ok(res) => match res {
            Some(data) => {
                let data_len = data.len();
                debug!(
                    "Read {} bytes body from downstream, body end: {}",
                    data_len, end_of_body
                );
                if data_len == 0 && !end_of_body {
                    /* it is normal to get 0 bytes because of multi-chunk parsing */
                    return Ok(false);
                }
                write_body(client_body, data, end_of_body).map_err(|e| e.into_up())?;
                debug!("Write {} bytes body to h2 upstream", data_len);
                Ok(end_of_body)
            }
            None => {
                debug!("Read downstream body done");
                /* send a standalone END_STREAM flag */
                write_body(client_body, Bytes::new(), true).map_err(|e| e.into_up())?;
                debug!("Write END_STREAM to h2 upstream");
                Ok(true)
            }
        },
        Err(e) => e.into_down().into_err(),
    }
}

/* Read response header, body and trailer from h2 upstream and send them to tx */
pub(crate) async fn pipe_2to1_response(
    client: &mut Http2Session,
    tx: mpsc::Sender<HttpTask>,
) -> Result<()> {
    client
        .read_response_header()
        .await
        .map_err(|e| e.into_up())?; // should we send the error as an HttpTask?

    let resp_header = Box::new(client.response_header().expect("just read").clone());

    tx.send(HttpTask::Header(resp_header, client.response_finished()))
        .await
        .or_err(InternalError, "sending h2 headers to pipe")?;

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
                // Don't care if send fails: downstream already gone
                let _ = tx.send(HttpTask::Failed(e.into_up())).await;
                // Downstream should consume all remaining data and handle the error
                return Ok(());
            }
        };
        if data.is_empty() && !client.response_finished() {
            /* it is normal to get 0 bytes because of multi-chunk
             * don't write 0 bytes to downstream since it will be
             * misread as the terminating chunk */
            continue;
        }
        tx.send(HttpTask::Body(Some(data), client.response_finished()))
            .await
            .or_err(InternalError, "sending h2 body to pipe")?;
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
        tx.send(HttpTask::Trailer(trailers))
            .await
            .or_err(InternalError, "sending h2 trailer to pipe")?;
    }

    tx.send(HttpTask::Done)
        .await
        .unwrap_or_else(|_| debug!("h2 to h1 channel closed!"));

    Ok(())
}

#[test]
fn test_update_authority() {
    let mut parts = http::request::Builder::new()
        .body(())
        .unwrap()
        .into_parts()
        .0;
    update_h2_scheme_authority(&mut parts, b"example.com").unwrap();
    assert_eq!("example.com", parts.uri.authority().unwrap());
    update_h2_scheme_authority(&mut parts, b"example.com:456").unwrap();
    assert_eq!("example.com:456", parts.uri.authority().unwrap());
    update_h2_scheme_authority(&mut parts, b"example.com:").unwrap();
    assert_eq!("example.com:", parts.uri.authority().unwrap());
    update_h2_scheme_authority(&mut parts, b"example.com:123:345").unwrap();
    assert_eq!("example.com:123", parts.uri.authority().unwrap());
    update_h2_scheme_authority(&mut parts, b"[::1]").unwrap();
    assert_eq!("[::1]", parts.uri.authority().unwrap());
}
