// Copyright 2025 Cloudflare, Inc.
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
use http::header::{CONTENT_LENGTH, CONTENT_TYPE};
use http::{Method, StatusCode};
use pingora_cache::key::CacheHashKey;
use pingora_cache::lock::LockStatus;
use pingora_cache::max_file_size::ERR_RESPONSE_TOO_LARGE;
use pingora_cache::{ForcedInvalidationKind, HitStatus, RespCacheable::*};
use pingora_core::protocols::http::conditional_filter::to_304;
use pingora_core::protocols::http::v1::common::header_value_content_length;
use pingora_core::ErrorType;
use range_filter::RangeBodyFilter;
use std::time::SystemTime;

impl<SV> HttpProxy<SV> {
    // return bool: server_session can be reused, and error if any
    pub(crate) async fn proxy_cache(
        self: &Arc<Self>,
        session: &mut Session,
        ctx: &mut SV::CTX,
    ) -> Option<(bool, Option<Box<Error>>)>
    // None: continue to proxy, Some: return
    where
        SV: ProxyHttp + Send + Sync + 'static,
        SV::CTX: Send + Sync,
    {
        // Cache logic request phase
        if let Err(e) = self.inner.request_cache_filter(session, ctx) {
            // TODO: handle this error
            warn!(
                "Fail to request_cache_filter: {e}, {}",
                self.inner.request_summary(session, ctx)
            );
        }

        // cache key logic, should this be part of request_cache_filter?
        if session.cache.enabled() {
            match self.inner.cache_key_callback(session, ctx) {
                Ok(key) => {
                    session.cache.set_cache_key(key);
                }
                Err(e) => {
                    // TODO: handle this error
                    session.cache.disable(NoCacheReason::StorageError);
                    warn!(
                        "Fail to cache_key_callback: {e}, {}",
                        self.inner.request_summary(session, ctx)
                    );
                }
            }
        }

        // cache purge logic: PURGE short-circuits rest of request
        if self.inner.is_purge(session, ctx) {
            return self.proxy_purge(session, ctx).await;
        }

        // bypass cache lookup if we predict to be uncacheable
        if session.cache.enabled() && !session.cache.cacheable_prediction() {
            session.cache.bypass();
        }

        if !session.cache.enabled() {
            return None;
        }

        // cache lookup logic
        loop {
            // for cache lock, TODO: cap the max number of loops
            match session.cache.cache_lookup().await {
                Ok(res) => {
                    let mut hit_status_opt = None;
                    if let Some((mut meta, mut handler)) = res {
                        // Vary logic
                        // Because this branch can be called multiple times in a loop, and we only
                        // need to update the vary once, check if variance is already set to
                        // prevent unnecessary vary lookups.
                        let cache_key = session.cache.cache_key();
                        if let Some(variance) = cache_key.variance_bin() {
                            // We've looked up a secondary slot.
                            // Adhoc double check that the variance found is the variance we want.
                            if Some(variance) != meta.variance() {
                                warn!("Cache variance mismatch, {variance:?}, {cache_key:?}");
                                session.cache.disable(NoCacheReason::InternalError);
                                break None;
                            }
                        } else {
                            // Basic cache key; either variance is off, or this is the primary slot.
                            let req_header = session.req_header();
                            let variance = self.inner.cache_vary_filter(&meta, ctx, req_header);
                            if let Some(variance) = variance {
                                // Variance is on. This is the primary slot.
                                if !session.cache.cache_vary_lookup(variance, &meta) {
                                    // This wasn't the desired variant. Updated cache key variance, cause another
                                    // lookup to get the desired variant, which would be in a secondary slot.
                                    continue;
                                }
                            } // else: vary is not in use
                        }

                        // Either no variance, or the current handler targets the correct variant.

                        // hit
                        // TODO: maybe round and/or cache now()
                        let is_fresh = meta.is_fresh(SystemTime::now());
                        // check if we should force expire or force miss
                        let hit_status = match self
                            .inner
                            .cache_hit_filter(session, &meta, &mut handler, is_fresh, ctx)
                            .await
                        {
                            Err(e) => {
                                error!(
                                    "Failed to filter cache hit: {e}, {}",
                                    self.inner.request_summary(session, ctx)
                                );
                                // this return value will cause us to fetch from upstream
                                HitStatus::FailedHitFilter
                            }
                            Ok(None) => {
                                if is_fresh {
                                    HitStatus::Fresh
                                } else {
                                    HitStatus::Expired
                                }
                            }
                            Ok(Some(ForcedInvalidationKind::ForceExpired)) => {
                                // force expired asset should not be serve as stale
                                // because force expire is usually to remove data
                                meta.disable_serve_stale();
                                HitStatus::ForceExpired
                            }
                            Ok(Some(ForcedInvalidationKind::ForceMiss)) => HitStatus::ForceMiss,
                        };

                        hit_status_opt = Some(hit_status);

                        // init cache for hit / stale
                        session.cache.cache_found(meta, handler, hit_status);
                    }

                    if hit_status_opt.map_or(true, HitStatus::is_treated_as_miss) {
                        // cache miss
                        if session.cache.is_cache_locked() {
                            // Another request is filling the cache; try waiting til that's done and retry.
                            let lock_status = session.cache.cache_lock_wait().await;
                            if self.handle_lock_status(session, ctx, lock_status) {
                                continue;
                            } else {
                                break None;
                            }
                        } else {
                            self.inner.cache_miss(session, ctx);
                            break None;
                        }
                    }

                    // Safe because an empty hit status would have broken out
                    // in the block above
                    let hit_status = hit_status_opt.expect("None case handled as miss");

                    if !hit_status.is_fresh() {
                        // expired or force expired asset
                        if session.cache.is_cache_locked() {
                            // first if this is the sub request for the background cache update
                            if let Some(write_lock) = session
                                .subrequest_ctx
                                .as_mut()
                                .and_then(|ctx| ctx.take_write_lock())
                            {
                                // Put the write lock in the request
                                session.cache.set_write_lock(write_lock);
                                session.cache.tag_as_subrequest();
                                // and then let it go to upstream
                                break None;
                            }
                            let will_serve_stale = session.cache.can_serve_stale_updating()
                                && self.inner.should_serve_stale(session, ctx, None);
                            if !will_serve_stale {
                                let lock_status = session.cache.cache_lock_wait().await;
                                if self.handle_lock_status(session, ctx, lock_status) {
                                    continue;
                                } else {
                                    break None;
                                }
                            }
                            // else continue to serve stale
                            session.cache.set_stale_updating();
                        } else if session.cache.is_cache_lock_writer() {
                            // stale while revalidate logic for the writer
                            let will_serve_stale = session.cache.can_serve_stale_updating()
                                && self.inner.should_serve_stale(session, ctx, None);
                            if will_serve_stale {
                                // create a background thread to do the actual update
                                // the subrequest handle is only None by this phase in unit tests
                                // that don't go through process_new_http
                                let (permit, cache_lock) = session.cache.take_write_lock();
                                SubrequestSpawner::new(self.clone()).spawn_background_subrequest(
                                    session.as_ref(),
                                    subrequest::Ctx::builder()
                                        .cache_write_lock(
                                            cache_lock,
                                            session.cache.cache_key().clone(),
                                            permit,
                                        )
                                        .build(),
                                );
                                // continue to serve stale for this request
                                session.cache.set_stale_updating();
                            } else {
                                // return to fetch from upstream
                                break None;
                            }
                        } else {
                            // return to fetch from upstream
                            break None;
                        }
                    }

                    let (reuse, err) = self.proxy_cache_hit(session, ctx).await;
                    if let Some(e) = err.as_ref() {
                        error!(
                            "Fail to serve cache: {e}, {}",
                            self.inner.request_summary(session, ctx)
                        );
                    }
                    // responses is served from cache, exit
                    break Some((reuse, err));
                }
                Err(e) => {
                    // Allow cache miss to fill cache even if cache lookup errors
                    // this is mostly to support backward incompatible metadata update
                    // TODO: check error types
                    // session.cache.disable();
                    self.inner.cache_miss(session, ctx);
                    warn!(
                        "Fail to cache lookup: {e}, {}",
                        self.inner.request_summary(session, ctx)
                    );
                    break None;
                }
            }
        }
    }

    // return bool: server_session can be reused, and error if any
    pub(crate) async fn proxy_cache_hit(
        &self,
        session: &mut Session,
        ctx: &mut SV::CTX,
    ) -> (bool, Option<Box<Error>>)
    where
        SV: ProxyHttp + Send + Sync,
        SV::CTX: Send + Sync,
    {
        use range_filter::*;

        let seekable = session.cache.hit_handler().can_seek();
        let mut header = cache_hit_header(&session.cache);

        let req = session.req_header();

        let not_modified = match self.inner.cache_not_modified_filter(session, &header, ctx) {
            Ok(not_modified) => not_modified,
            Err(e) => {
                // fail open if cache_not_modified_filter errors,
                // just return the whole original response
                warn!(
                    "Failed to run cache not modified filter: {e}, {}",
                    self.inner.request_summary(session, ctx)
                );
                false
            }
        };
        if not_modified {
            to_304(&mut header);
        }
        let header_only = not_modified || req.method == http::method::Method::HEAD;

        // process range header if the cache storage supports seek
        let range_type = if seekable && !session.ignore_downstream_range {
            self.inner.range_header_filter(session, &mut header, ctx)
        } else {
            RangeType::None
        };

        // return a 416 with an empty body for simplicity
        let header_only = header_only || matches!(range_type, RangeType::Invalid);

        // TODO: use ProxyUseCache to replace the logic below
        match self.inner.response_filter(session, &mut header, ctx).await {
            Ok(_) => {
                if let Err(e) = session
                    .downstream_modules_ctx
                    .response_header_filter(&mut header, header_only)
                    .await
                {
                    error!(
                        "Failed to run downstream modules response header filter in hit: {e}, {}",
                        self.inner.request_summary(session, ctx)
                    );
                    session
                        .as_mut()
                        .respond_error(500)
                        .await
                        .unwrap_or_else(|e| {
                            error!("failed to send error response to downstream: {e}");
                        });
                    // we have not write anything dirty to downstream, it is still reusable
                    return (true, Some(e));
                }

                if let Err(e) = session
                    .as_mut()
                    .write_response_header(header)
                    .await
                    .map_err(|e| e.into_down())
                {
                    // downstream connection is bad already
                    return (false, Some(e));
                }
            }
            Err(e) => {
                error!(
                    "Failed to run response filter in hit: {e}, {}",
                    self.inner.request_summary(session, ctx)
                );
                session
                    .as_mut()
                    .respond_error(500)
                    .await
                    .unwrap_or_else(|e| {
                        error!("failed to send error response to downstream: {e}");
                    });
                // we have not write anything dirty to downstream, it is still reusable
                return (true, Some(e));
            }
        }
        debug!("finished sending cached header to downstream");

        if !header_only {
            let mut maybe_range_filter = match &range_type {
                RangeType::Single(r) => {
                    if let Err(e) = session.cache.hit_handler().seek(r.start, Some(r.end)) {
                        return (false, Some(e));
                    }
                    None
                }
                RangeType::Multi(_) => {
                    // TODO: seek hit handler for multipart
                    let mut range_filter = RangeBodyFilter::new();
                    range_filter.set(range_type.clone());
                    Some(range_filter)
                }
                RangeType::Invalid => unreachable!(),
                RangeType::None => None,
            };
            loop {
                match session.cache.hit_handler().read_body().await {
                    Ok(raw_body) => {
                        let end = raw_body.is_none();

                        let mut body = if let Some(range_filter) = maybe_range_filter.as_mut() {
                            range_filter.filter_body(raw_body)
                        } else {
                            raw_body
                        };

                        match self
                            .inner
                            .response_body_filter(session, &mut body, end, ctx)
                        {
                            Ok(Some(duration)) => {
                                trace!("delaying response for {duration:?}");
                                time::sleep(duration).await;
                            }
                            Ok(None) => { /* continue */ }
                            Err(e) => {
                                // body is being sent, don't treat downstream as reusable
                                return (false, Some(e));
                            }
                        }

                        if let Err(e) = session
                            .downstream_modules_ctx
                            .response_body_filter(&mut body, end)
                        {
                            // body is being sent, don't treat downstream as reusable
                            return (false, Some(e));
                        }

                        if !end && body.as_ref().map_or(true, |b| b.is_empty()) {
                            // Don't write empty body which will end session,
                            // still more hit handler bytes to read
                            continue;
                        }

                        // write to downstream
                        let b = body.unwrap_or_default();
                        if let Err(e) = session
                            .as_mut()
                            .write_response_body(b, end)
                            .await
                            .map_err(|e| e.into_down())
                        {
                            return (false, Some(e));
                        }
                        if end {
                            break;
                        }
                    }
                    Err(e) => return (false, Some(e)),
                }
            }
        }

        if let Err(e) = session.cache.finish_hit_handler().await {
            warn!("Error during finish_hit_handler: {}", e);
        }

        match session.as_mut().finish_body().await {
            Ok(_) => {
                debug!("finished sending cached body to downstream");
                (true, None)
            }
            Err(e) => (false, Some(e)),
        }
    }

    /* Downstream revalidation, only needed when cache is on because otherwise origin
     * will handle it */
    pub(crate) fn downstream_response_conditional_filter(
        &self,
        use_cache: &mut ServeFromCache,
        session: &Session,
        resp: &mut ResponseHeader,
        ctx: &mut SV::CTX,
    ) where
        SV: ProxyHttp,
    {
        // TODO: range
        let req = session.req_header();

        let not_modified = match self.inner.cache_not_modified_filter(session, resp, ctx) {
            Ok(not_modified) => not_modified,
            Err(e) => {
                // fail open if cache_not_modified_filter errors,
                // just return the whole original response
                warn!(
                    "Failed to run cache not modified filter: {e}, {}",
                    self.inner.request_summary(session, ctx)
                );
                false
            }
        };

        if not_modified {
            to_304(resp);
        }
        let header_only = not_modified || req.method == http::method::Method::HEAD;
        if header_only && use_cache.is_on() {
            // tell cache to stop serving downstream after yielding header
            // (misses will continue to allow admitting upstream into cache)
            use_cache.enable_header_only();
        }
    }

    // TODO: cache upstream header filter to add/remove headers

    pub(crate) async fn cache_http_task(
        &self,
        session: &mut Session,
        task: &HttpTask,
        ctx: &mut SV::CTX,
        serve_from_cache: &mut ServeFromCache,
    ) -> Result<()>
    where
        SV: ProxyHttp + Send + Sync,
        SV::CTX: Send + Sync,
    {
        if !session.cache.enabled() && !session.cache.bypassing() {
            return Ok(());
        }

        match task {
            HttpTask::Header(header, end_stream) => {
                // decide if cacheable and create cache meta
                // for now, skip 1xxs (should not affect response cache decisions)
                // However 101 is an exception because it is the final response header
                if header.status.is_informational()
                    && header.status != StatusCode::SWITCHING_PROTOCOLS
                {
                    return Ok(());
                }
                match self.inner.response_cache_filter(session, header, ctx)? {
                    Cacheable(meta) => {
                        let mut fill_cache = true;
                        if session.cache.bypassing() {
                            // The cache might have been bypassed because the response exceeded the
                            // maximum cacheable asset size. If that looks like the case (there
                            // is a maximum file size configured and we don't know the content
                            // length up front), attempting to re-enable the cache now would cause
                            // the request to fail when the chunked response exceeds the maximum
                            // file size again.
                            if session.cache.max_file_size_bytes().is_some()
                                && !meta.headers().contains_key(header::CONTENT_LENGTH)
                            {
                                session
                                    .cache
                                    .disable(NoCacheReason::PredictedResponseTooLarge);
                                return Ok(());
                            }

                            session.cache.response_became_cacheable();

                            if session.req_header().method == Method::GET
                                && meta.response_header().status == StatusCode::OK
                            {
                                self.inner.cache_miss(session, ctx);
                            } else {
                                // we've allowed caching on the next request,
                                // but do not cache _this_ request if bypassed and not 200
                                // (We didn't run upstream request cache filters to strip range or condition headers,
                                // so this could be an uncacheable response e.g. 206 or 304 or HEAD.
                                // Exclude all non-200/GET for simplicity, may expand allowable codes in the future.)
                                fill_cache = false;
                                session.cache.disable(NoCacheReason::Deferred);
                            }
                        }

                        // If the Content-Length is known, and a maximum asset size has been configured
                        // on the cache, validate that the response does not exceed the maximum asset size.
                        if session.cache.enabled() {
                            if let Some(max_file_size) = session.cache.max_file_size_bytes() {
                                let content_length_hdr = meta.headers().get(header::CONTENT_LENGTH);
                                if let Some(content_length) =
                                    header_value_content_length(content_length_hdr)
                                {
                                    if content_length > max_file_size {
                                        fill_cache = false;
                                        session.cache.response_became_uncacheable(
                                            NoCacheReason::ResponseTooLarge,
                                        );
                                        session.cache.disable(NoCacheReason::ResponseTooLarge);
                                        // too large to cache, disable ranging
                                        session.ignore_downstream_range = true;
                                    }
                                }
                                // if the content-length header is not specified, the miss handler
                                // will count the response size on the fly, aborting the request
                                // mid-transfer if the max file size is exceeded
                            }
                        }
                        if fill_cache {
                            let req_header = session.req_header();
                            // Update the variance in the meta via the same callback,
                            // cache_vary_filter(), used in cache lookup for consistency.
                            // Future cache lookups need a matching variance in the meta
                            // with the cache key to pick up the correct variance
                            let variance = self.inner.cache_vary_filter(&meta, ctx, req_header);
                            session.cache.set_cache_meta(meta);
                            session.cache.update_variance(variance);
                            // this sends the meta and header
                            session.cache.set_miss_handler().await?;
                            if session.cache.miss_body_reader().is_some() {
                                serve_from_cache.enable_miss();
                            }
                            if *end_stream {
                                session
                                    .cache
                                    .miss_handler()
                                    .unwrap() // safe, it is set above
                                    .write_body(Bytes::new(), true)
                                    .await?;
                                session.cache.finish_miss_handler().await?;
                            }
                        }
                    }
                    Uncacheable(reason) => {
                        if !session.cache.bypassing() {
                            // mark as uncacheable, so we bypass cache next time
                            session.cache.response_became_uncacheable(reason);
                        }
                        session.cache.disable(reason);
                    }
                }
            }
            HttpTask::Body(data, end_stream) => match data {
                Some(d) => {
                    if session.cache.enabled() {
                        // TODO: do this async
                        // fail if writing the body would exceed the max_file_size_bytes
                        let body_size_allowed =
                            session.cache.track_body_bytes_for_max_file_size(d.len());
                        if !body_size_allowed {
                            debug!("chunked response exceeded max cache size, remembering that it is uncacheable");
                            session
                                .cache
                                .response_became_uncacheable(NoCacheReason::ResponseTooLarge);

                            return Error::e_explain(
                                ERR_RESPONSE_TOO_LARGE,
                                format!(
                                    "writing data of size {} bytes would exceed max file size of {} bytes",
                                    d.len(),
                                    session.cache.max_file_size_bytes().expect("max file size bytes must be set to exceed size")
                                ),
                            );
                        }

                        // this will panic if more data is sent after we see end_stream
                        // but should be impossible in real world
                        let miss_handler = session.cache.miss_handler().unwrap();

                        miss_handler.write_body(d.clone(), *end_stream).await?;
                        if *end_stream {
                            session.cache.finish_miss_handler().await?;
                        }
                    }
                }
                None => {
                    if session.cache.enabled() && *end_stream {
                        session.cache.finish_miss_handler().await?;
                    }
                }
            },
            HttpTask::Trailer(_) => {} // h1 trailer is not supported yet
            HttpTask::Done => {
                if session.cache.enabled() {
                    session.cache.finish_miss_handler().await?;
                }
            }
            HttpTask::Failed(_) => {
                // TODO: handle this failure: delete the temp files?
            }
        }
        Ok(())
    }

    // Decide if local cache can be used according to upstream http header
    // 1. when upstream returns 304, the local cache is refreshed and served fresh
    // 2. when upstream returns certain HTTP error status, the local cache is served stale
    // Return true if local cache should be used, false otherwise
    pub(crate) async fn revalidate_or_stale(
        &self,
        session: &mut Session,
        task: &mut HttpTask,
        ctx: &mut SV::CTX,
    ) -> bool
    where
        SV: ProxyHttp + Send + Sync,
        SV::CTX: Send + Sync,
    {
        if !session.cache.enabled() {
            return false;
        }

        match task {
            HttpTask::Header(resp, _eos) => {
                if resp.status == StatusCode::NOT_MODIFIED {
                    if session.cache.maybe_cache_meta().is_some() {
                        // run upstream response filters on upstream 304 first
                        if let Err(err) = self.inner.upstream_response_filter(session, resp, ctx) {
                            error!("upstream response filter error on 304: {err:?}");
                            session.cache.revalidate_uncacheable(
                                *resp.clone(),
                                NoCacheReason::InternalError,
                            );
                            // always serve from cache after receiving the 304
                            return true;
                        }
                        // 304 doesn't contain all the headers, merge 304 into cached 200 header
                        // in order for response_cache_filter to run correctly
                        let merged_header = session.cache.revalidate_merge_header(resp);
                        match self
                            .inner
                            .response_cache_filter(session, &merged_header, ctx)
                        {
                            Ok(Cacheable(mut meta)) => {
                                // For simplicity, ignore changes to variance over 304 for now.
                                // Note this means upstream can only update variance via 2xx
                                // (expired response).
                                //
                                // TODO: if we choose to respect changing Vary / variance over 304,
                                // then there are a few cases to consider. See `update_variance` in
                                // the `pingora-cache` module.
                                let old_meta = session.cache.maybe_cache_meta().unwrap(); // safe, checked above
                                if let Some(old_variance) = old_meta.variance() {
                                    meta.set_variance(old_variance);
                                }
                                if let Err(e) = session.cache.revalidate_cache_meta(meta).await {
                                    // Fail open: we can continue use the revalidated response even
                                    // if the meta failed to write to storage
                                    warn!("revalidate_cache_meta failed {e:?}");
                                }
                            }
                            Ok(Uncacheable(reason)) => {
                                // This response was once cacheable, and upstream tells us it has not changed
                                // but now we decided it is uncacheable!
                                // RFC 9111: still allowed to reuse stored response this time because
                                // it was "successfully validated"
                                // https://www.rfc-editor.org/rfc/rfc9111#constructing.responses.from.caches
                                // Serve the response, but do not update cache

                                // We also want to avoid poisoning downstream's cache with an unsolicited 304
                                // if we did not receive a conditional request from downstream
                                // (downstream may have a different cacheability assessment and could cache the 304)

                                //TODO: log more
                                warn!("Uncacheable {reason:?} 304 received");
                                session.cache.response_became_uncacheable(reason);
                                session.cache.revalidate_uncacheable(merged_header, reason);
                            }
                            Err(e) => {
                                // Error during revalidation, similarly to the reasons above
                                // (avoid poisoning downstream cache with passthrough 304),
                                // allow serving the stored response without updating cache
                                warn!("Error {e:?} response_cache_filter during revalidation");
                                session.cache.revalidate_uncacheable(
                                    merged_header,
                                    NoCacheReason::InternalError,
                                );
                                // Assume the next 304 may succeed, so don't mark uncacheable
                            }
                        }
                        // always serve from cache after receiving the 304
                        true
                    } else {
                        //TODO: log more
                        warn!("304 received without cached asset, disable caching");
                        let reason = NoCacheReason::Custom("304 on miss");
                        session.cache.response_became_uncacheable(reason);
                        session.cache.disable(reason);
                        false
                    }
                } else if resp.status.is_server_error() {
                    // stale if error logic, 5xx only for now

                    // this is response header filter, response_written should always be None?
                    if !session.cache.can_serve_stale_error()
                        || session.response_written().is_some()
                    {
                        return false;
                    }

                    // create an error to encode the http status code
                    let http_status_error = Error::create(
                        ErrorType::HTTPStatus(resp.status.as_u16()),
                        ErrorSource::Upstream,
                        None,
                        None,
                    );
                    if self
                        .inner
                        .should_serve_stale(session, ctx, Some(&http_status_error))
                    {
                        // no more need to keep the write lock
                        session
                            .cache
                            .release_write_lock(NoCacheReason::UpstreamError);
                        true
                    } else {
                        false
                    }
                } else {
                    false // not 304, not stale if error status code
                }
            }
            _ => false, // not header
        }
    }

    // None: no staled asset is used, Some(_): staled asset is sent to downstream
    // bool: can the downstream connection be reused
    pub(crate) async fn handle_stale_if_error(
        &self,
        session: &mut Session,
        ctx: &mut SV::CTX,
        error: &Error,
    ) -> Option<(bool, Option<Box<Error>>)>
    where
        SV: ProxyHttp + Send + Sync,
        SV::CTX: Send + Sync,
    {
        // the caller might already checked this as an optimization
        if !session.cache.can_serve_stale_error() {
            return None;
        }

        // the error happen halfway through a regular response to downstream
        // can't resend the response
        if session.response_written().is_some() {
            return None;
        }

        // check error types
        if !self.inner.should_serve_stale(session, ctx, Some(error)) {
            return None;
        }

        // log the original error
        warn!(
            "Fail to proxy: {}, serving stale, {}",
            error,
            self.inner.request_summary(session, ctx)
        );

        // no more need to hang onto the cache lock
        session
            .cache
            .release_write_lock(NoCacheReason::UpstreamError);

        Some(self.proxy_cache_hit(session, ctx).await)
    }

    // helper function to check when to continue to retry lock (true) or give up (false)
    fn handle_lock_status(
        &self,
        session: &mut Session,
        ctx: &SV::CTX,
        lock_status: LockStatus,
    ) -> bool
    where
        SV: ProxyHttp,
    {
        debug!("cache unlocked {lock_status:?}");
        match lock_status {
            // should lookup the cached asset again
            LockStatus::Done => true,
            // should compete to be a new writer
            LockStatus::TransientError => true,
            // the request is uncacheable, go ahead to fetch from the origin
            LockStatus::GiveUp => {
                // TODO: It will be nice for the writer to propagate the real reason
                session.cache.disable(NoCacheReason::CacheLockGiveUp);
                // not cacheable, just go to the origin.
                false
            }
            // treat this the same as TransientError
            LockStatus::Dangling => {
                // software bug, but request can recover from this
                warn!(
                    "Dangling cache lock, {}",
                    self.inner.request_summary(session, ctx)
                );
                true
            }
            /* We have 3 options when a lock is held too long
             * 1. release the lock and let every request complete for it again
             * 2. let every request cache miss
             * 3. let every request through while disabling cache
             * #1 could repeat the situation but protect the origin from load
             * #2 could amplify disk writes and storage for temp file
             * #3 is the simplest option for now */
            LockStatus::Timeout => {
                warn!(
                    "Cache lock timeout, {}",
                    self.inner.request_summary(session, ctx)
                );
                session.cache.disable(NoCacheReason::CacheLockTimeout);
                // not cacheable, just go to the origin.
                false
            }
            // software bug, this status should be impossible to reach
            LockStatus::Waiting => panic!("impossible LockStatus::Waiting"),
        }
    }
}

fn cache_hit_header(cache: &HttpCache) -> Box<ResponseHeader> {
    let mut header = Box::new(cache.cache_meta().response_header_copy());
    // convert cache response

    // these status codes / method cannot have body, so no need to add chunked encoding
    let no_body = matches!(header.status.as_u16(), 204 | 304);

    // https://www.rfc-editor.org/rfc/rfc9111#section-4:
    // When a stored response is used to satisfy a request without validation, a cache
    // MUST generate an Age header field
    if !cache.upstream_used() {
        let age = cache.cache_meta().age().as_secs();
        header.insert_header(http::header::AGE, age).unwrap();
    }

    /* Add chunked header to tell downstream to use chunked encoding
     * during the absent of content-length in h2 */
    if !no_body
        && !header.status.is_informational()
        && header.headers.get(http::header::CONTENT_LENGTH).is_none()
    {
        header
            .insert_header(http::header::TRANSFER_ENCODING, "chunked")
            .unwrap();
    }
    header
}

// https://datatracker.ietf.org/doc/html/rfc7233#section-3
pub mod range_filter {
    use super::*;
    use bytes::BytesMut;
    use http::header::*;
    use std::ops::Range;

    // parse bytes into usize, ignores specific error
    fn parse_number(input: &[u8]) -> Option<usize> {
        str::from_utf8(input).ok()?.parse().ok()
    }

    fn parse_range_header(range: &[u8], content_length: usize) -> RangeType {
        use regex::Regex;

        // Match individual range parts, (e.g. "0-100", "-5", "1-")
        static RE_SINGLE_RANGE_PART: Lazy<Regex> =
            Lazy::new(|| Regex::new(r"(?i)^\s*(?P<start>\d*)-(?P<end>\d*)\s*$").unwrap());

        // Convert bytes to UTF-8 string
        let range_str = match str::from_utf8(range) {
            Ok(s) => s,
            Err(_) => return RangeType::None,
        };

        // Split into "bytes=" and the actual range(s)
        let mut parts = range_str.splitn(2, "=");

        // Check if it starts with "bytes="
        let prefix = parts.next();
        if !prefix.is_some_and(|s| s.eq_ignore_ascii_case("bytes")) {
            return RangeType::None;
        }

        let Some(ranges_str) = parts.next() else {
            // No ranges provided
            return RangeType::None;
        };

        // Get the actual range string (e.g."100-200,300-400")
        let mut range_count = 0;
        for _ in ranges_str.split(',') {
            range_count += 1;
            // TODO: make configurable
            const MAX_RANGES: usize = 100;
            if range_count >= MAX_RANGES {
                // If we get more than MAX_RANGES ranges, return None for now to save parsing time
                return RangeType::None;
            }
        }
        let mut ranges: Vec<Range<usize>> = Vec::with_capacity(range_count);

        // Process each range
        let mut last_range_end = 0;
        for part in ranges_str.split(',') {
            let captured = match RE_SINGLE_RANGE_PART.captures(part) {
                Some(c) => c,
                None => {
                    return RangeType::None;
                }
            };

            let maybe_start = captured
                .name("start")
                .and_then(|s| s.as_str().parse::<usize>().ok());
            let end = captured
                .name("end")
                .and_then(|s| s.as_str().parse::<usize>().ok());

            let range = if let Some(start) = maybe_start {
                if start >= content_length {
                    // Skip the invalid range
                    continue;
                }
                // open-ended range should end at the last byte
                // over sized end is allowed but ignored
                // range end is inclusive
                let end = std::cmp::min(end.unwrap_or(content_length - 1), content_length - 1) + 1;
                if end <= start {
                    // Skip the invalid range
                    continue;
                }
                start..end
            } else {
                // start is empty, this changes the meaning of the value of `end`
                // Now it means to read the last `end` bytes
                if let Some(end) = end {
                    if content_length >= end {
                        (content_length - end)..content_length
                    } else {
                        // over sized end is allowed but ignored
                        0..content_length
                    }
                } else {
                    // No start or end, skip the invalid range
                    continue;
                }
            };
            // For now we stick to non-overlapping, ascending ranges for simplicity
            // and parity with nginx
            if range.start < last_range_end {
                return RangeType::None;
            }
            last_range_end = range.end;
            ranges.push(range);
        }

        // Note for future: we can technically coalesce multiple ranges for multipart
        //
        // https://www.rfc-editor.org/rfc/rfc9110#section-17.15
        // "Servers ought to ignore, coalesce, or reject egregious range
        // requests, such as requests for more than two overlapping ranges or
        // for many small ranges in a single set, particularly when the ranges
        // are requested out of order for no apparent reason. Multipart range
        // requests are not designed to support random access."

        if ranges.is_empty() {
            // We got some ranges, processed them but none were valid
            RangeType::Invalid
        } else if ranges.len() == 1 {
            RangeType::Single(ranges[0].clone()) // Only 1 index
        } else {
            RangeType::Multi(MultiRangeInfo::new(ranges))
        }
    }
    #[test]
    fn test_parse_range() {
        assert_eq!(
            parse_range_header(b"bytes=0-1", 10),
            RangeType::new_single(0, 2)
        );
        assert_eq!(
            parse_range_header(b"bYTes=0-9", 10),
            RangeType::new_single(0, 10)
        );
        assert_eq!(
            parse_range_header(b"bytes=0-12", 10),
            RangeType::new_single(0, 10)
        );
        assert_eq!(
            parse_range_header(b"bytes=0-", 10),
            RangeType::new_single(0, 10)
        );
        assert_eq!(parse_range_header(b"bytes=2-1", 10), RangeType::Invalid);
        assert_eq!(parse_range_header(b"bytes=10-11", 10), RangeType::Invalid);
        assert_eq!(
            parse_range_header(b"bytes=-2", 10),
            RangeType::new_single(8, 10)
        );
        assert_eq!(
            parse_range_header(b"bytes=-12", 10),
            RangeType::new_single(0, 10)
        );
        assert_eq!(parse_range_header(b"bytes=-", 10), RangeType::Invalid);
        assert_eq!(parse_range_header(b"bytes=", 10), RangeType::None);
    }

    // Add some tests for multi-range too
    #[test]
    fn test_parse_range_header_multi() {
        assert_eq!(
            parse_range_header(b"bytes=0-1,4-5", 10)
                .get_multirange_info()
                .expect("Should have multipart info for Multipart range request")
                .ranges,
            (vec![Range { start: 0, end: 2 }, Range { start: 4, end: 6 }])
        );
        // Last range is invalid because the content-length is too small
        assert_eq!(
            parse_range_header(b"bytEs=0-99,200-299,400-499", 320)
                .get_multirange_info()
                .expect("Should have multipart info for Multipart range request")
                .ranges,
            (vec![
                Range { start: 0, end: 100 },
                Range {
                    start: 200,
                    end: 300
                }
            ])
        );
        // Same as above but appropriate content length
        assert_eq!(
            parse_range_header(b"bytEs=0-99,200-299,400-499", 500)
                .get_multirange_info()
                .expect("Should have multipart info for Multipart range request")
                .ranges,
            vec![
                Range { start: 0, end: 100 },
                Range {
                    start: 200,
                    end: 300
                },
                Range {
                    start: 400,
                    end: 500
                },
            ]
        );
        // Looks like a range request but it is continuous, we decline to range
        assert_eq!(parse_range_header(b"bytes=0-,-2", 10), RangeType::None,);
        // Should not have multirange info set
        assert!(parse_range_header(b"bytes=0-,-2", 10)
            .get_multirange_info()
            .is_none());
        // Overlapping ranges, these ranges are currently declined
        assert_eq!(parse_range_header(b"bytes=0-3,2-5", 10), RangeType::None,);
        assert!(parse_range_header(b"bytes=0-3,2-5", 10)
            .get_multirange_info()
            .is_none());

        // Content length is 2, so only range is 0-2.
        assert_eq!(
            parse_range_header(b"bytes=0-5,10-", 2),
            RangeType::new_single(0, 2)
        );
        assert!(parse_range_header(b"bytes=0-5,10-", 2)
            .get_multirange_info()
            .is_none());

        // We should ignore the last incorrect range and return the other acceptable ranges
        assert_eq!(
            parse_range_header(b"bytes=0-5, 10-20, 30-18", 200)
                .get_multirange_info()
                .expect("Should have multipart info for Multipart range request")
                .ranges,
            vec![Range { start: 0, end: 6 }, Range { start: 10, end: 21 },]
        );
        // All invalid ranges
        assert_eq!(
            parse_range_header(b"bytes=5-0, 20-15, 30-25", 200),
            RangeType::Invalid
        );

        // Helper function to generate a large number of ranges for the next test
        fn generate_range_header(count: usize) -> Vec<u8> {
            let mut s = String::from("bytes=");
            for i in 0..count {
                let start = i * 4;
                let end = start + 1;
                if i > 0 {
                    s.push(',');
                }
                s.push_str(&start.to_string());
                s.push('-');
                s.push_str(&end.to_string());
            }
            s.into_bytes()
        }

        // Test 100 range limit for parsing.
        let ranges = generate_range_header(101);
        assert_eq!(parse_range_header(&ranges, 1000), RangeType::None)
    }

    // For Multipart Requests, we need to know the boundary, content length and type across
    // the headers and the body. So let us store this information as part of the range
    #[derive(Debug, Eq, PartialEq, Clone)]
    pub struct MultiRangeInfo {
        pub ranges: Vec<Range<usize>>,
        pub boundary: String,
        total_length: usize,
        content_type: Option<String>,
    }

    impl MultiRangeInfo {
        // Create a new MultiRangeInfo, when we just have the ranges
        pub fn new(ranges: Vec<Range<usize>>) -> Self {
            Self {
                ranges,
                // Directly create boundary string on initialization
                boundary: Self::generate_boundary(),
                total_length: 0,
                content_type: None,
            }
        }
        pub fn set_content_type(&mut self, content_type: String) {
            self.content_type = Some(content_type)
        }
        pub fn set_total_length(&mut self, total_length: usize) {
            self.total_length = total_length;
        }
        // Per [RFC 9110](https://www.rfc-editor.org/rfc/rfc9110.html#multipart.byteranges),
        // we need generate a boundary string for each body part.
        // Per [RFC 2046](https://www.rfc-editor.org/rfc/rfc2046#section-5.1.1), the boundary should be no longer than 70 characters
        // and it must not match the body content.
        fn generate_boundary() -> String {
            use rand::Rng;
            let mut rng: rand::prelude::ThreadRng = rand::thread_rng();
            format!("{:016x}", rng.gen::<u64>())
        }
        fn calculate_multipart_length(&self) -> usize {
            let mut total_length = 0;
            let content_type = self.content_type.as_ref();
            for range in self.ranges.clone() {
                // Each part should have
                // \r\n--boundary\r\n                         --> 4 + boundary.len() (16) + 2 = 20
                // Content-Type: original-content-type\r\n    --> 14 + content_type.len() + 2
                // Content-Range: bytes start-end/total\r\n   --> Variable +2
                // \r\n                                       --> 2
                // [data]                                     --> data.len()
                total_length += 4 + self.boundary.len() + 2;
                total_length += content_type.map_or(0, |ct| 14 + ct.len() + 2);
                total_length += format!(
                    "Content-Range: bytes {}-{}/{}",
                    range.start,
                    range.end - 1,
                    self.total_length
                )
                .len()
                    + 2;
                total_length += 2;
                total_length += range.end - range.start;
            }
            // Final boundary: "\r\n--<boundary>--\r\n"
            total_length += 4 + self.boundary.len() + 4;
            total_length
        }
    }
    #[derive(Debug, Eq, PartialEq, Clone)]
    pub enum RangeType {
        None,
        Single(Range<usize>),
        Multi(MultiRangeInfo),
        Invalid,
    }

    impl RangeType {
        // Helper functions for tests
        #[allow(dead_code)]
        fn new_single(start: usize, end: usize) -> Self {
            RangeType::Single(Range { start, end })
        }
        #[allow(dead_code)]
        pub fn new_multi(ranges: Vec<Range<usize>>) -> Self {
            RangeType::Multi(MultiRangeInfo::new(ranges))
        }
        #[allow(dead_code)]
        fn get_multirange_info(&self) -> Option<&MultiRangeInfo> {
            match self {
                RangeType::Multi(multi_range_info) => Some(multi_range_info),
                _ => None,
            }
        }
        #[allow(dead_code)]
        fn update_multirange_info(&mut self, content_length: usize, content_type: Option<String>) {
            if let RangeType::Multi(multipart_range_info) = self {
                multipart_range_info.content_type = content_type;
                multipart_range_info.set_total_length(content_length);
            }
        }
    }

    // Handles both single-range and multipart-range requests
    pub fn range_header_filter(req: &RequestHeader, resp: &mut ResponseHeader) -> RangeType {
        // The Range header field is evaluated after evaluating the precondition
        // header fields defined in [RFC7232], and only if the result in absence
        // of the Range header field would be a 200 (OK) response
        if resp.status != StatusCode::OK {
            return RangeType::None;
        }

        // "A server MUST ignore a Range header field received with a request method other than GET."
        if req.method != http::Method::GET && req.method != http::Method::HEAD {
            return RangeType::None;
        }

        let Some(range_header) = req.headers.get(RANGE) else {
            return RangeType::None;
        };

        // Content-Length is not required by RFC but it is what nginx does and easier to implement
        // with this header present.
        let Some(content_length_bytes) = resp.headers.get(CONTENT_LENGTH) else {
            return RangeType::None;
        };
        // bail on invalid content length
        let Some(content_length) = parse_number(content_length_bytes.as_bytes()) else {
            return RangeType::None;
        };

        // if-range wants to understand if the Last-Modified / ETag value matches exactly for use
        // with resumable downloads.
        // https://datatracker.ietf.org/doc/html/rfc9110#name-if-range
        // Note that the RFC wants strong validation, and suggests that
        // "A valid entity-tag can be distinguished from a valid HTTP-date
        // by examining the first three characters for a DQUOTE,"
        // but this current etag matching behavior most closely mirrors nginx.
        if let Some(if_range) = req.headers.get(IF_RANGE) {
            let ir = if_range.as_bytes();
            let matches = if ir.len() >= 2 && ir.last() == Some(&b'"') {
                resp.headers.get(ETAG).is_some_and(|etag| etag == if_range)
            } else if let Some(last_modified) = resp.headers.get(LAST_MODIFIED) {
                last_modified == if_range
            } else {
                false
            };
            if !matches {
                return RangeType::None;
            }
        }

        // TODO: we can also check Accept-Range header from resp. Nginx gives uses the option
        // see proxy_force_ranges

        let mut range_type = parse_range_header(range_header.as_bytes(), content_length);

        match &mut range_type {
            RangeType::None => { /* nothing to do*/ }
            RangeType::Single(r) => {
                // 206 response
                resp.set_status(StatusCode::PARTIAL_CONTENT).unwrap();
                resp.insert_header(&CONTENT_LENGTH, r.end - r.start)
                    .unwrap();
                resp.insert_header(
                    &CONTENT_RANGE,
                    format!("bytes {}-{}/{content_length}", r.start, r.end - 1), // range end is inclusive
                )
                .unwrap()
            }

            RangeType::Multi(multi_range_info) => {
                let content_type = resp
                    .headers
                    .get(CONTENT_TYPE)
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("application/octet-stream");
                // Update multipart info
                multi_range_info.set_total_length(content_length);
                multi_range_info.set_content_type(content_type.to_string());

                let total_length = multi_range_info.calculate_multipart_length();

                resp.set_status(StatusCode::PARTIAL_CONTENT).unwrap();
                resp.insert_header(CONTENT_LENGTH, total_length).unwrap();
                resp.insert_header(
                    CONTENT_TYPE,
                    format!(
                        "multipart/byteranges; boundary={}",
                        multi_range_info.boundary
                    ), // RFC 2046
                )
                .unwrap();
                resp.remove_header(&CONTENT_RANGE);
            }
            RangeType::Invalid => {
                // 416 response
                resp.set_status(StatusCode::RANGE_NOT_SATISFIABLE).unwrap();
                // empty body for simplicity
                resp.insert_header(&CONTENT_LENGTH, HeaderValue::from_static("0"))
                    .unwrap();
                // TODO: remove other headers like content-encoding
                resp.remove_header(&CONTENT_TYPE);
                resp.insert_header(&CONTENT_RANGE, format!("bytes */{content_length}"))
                    .unwrap()
            }
        }

        range_type
    }

    #[test]
    fn test_range_filter_single() {
        fn gen_req() -> RequestHeader {
            RequestHeader::build(http::Method::GET, b"/", Some(1)).unwrap()
        }
        fn gen_resp() -> ResponseHeader {
            let mut resp = ResponseHeader::build(200, Some(1)).unwrap();
            resp.append_header("Content-Length", "10").unwrap();
            resp
        }

        // no range
        let req = gen_req();
        let mut resp = gen_resp();
        assert_eq!(RangeType::None, range_header_filter(&req, &mut resp));
        assert_eq!(resp.status.as_u16(), 200);

        // regular range
        let mut req = gen_req();
        req.insert_header("Range", "bytes=0-1").unwrap();
        let mut resp = gen_resp();
        assert_eq!(
            RangeType::new_single(0, 2),
            range_header_filter(&req, &mut resp)
        );
        assert_eq!(resp.status.as_u16(), 206);
        assert_eq!(resp.headers.get("content-length").unwrap().as_bytes(), b"2");
        assert_eq!(
            resp.headers.get("content-range").unwrap().as_bytes(),
            b"bytes 0-1/10"
        );

        // bad range
        let mut req = gen_req();
        req.insert_header("Range", "bytes=1-0").unwrap();
        let mut resp = gen_resp();
        assert_eq!(RangeType::Invalid, range_header_filter(&req, &mut resp));
        assert_eq!(resp.status.as_u16(), 416);
        assert_eq!(resp.headers.get("content-length").unwrap().as_bytes(), b"0");
        assert_eq!(
            resp.headers.get("content-range").unwrap().as_bytes(),
            b"bytes */10"
        );
    }

    // Multipart Tests
    #[test]
    fn test_range_filter_multipart() {
        fn gen_req() -> RequestHeader {
            let mut req: RequestHeader =
                RequestHeader::build(http::Method::GET, b"/", Some(1)).unwrap();
            req.append_header("Range", "bytes=0-1,3-4,6-7").unwrap();
            req
        }
        fn gen_req_overlap_range() -> RequestHeader {
            let mut req: RequestHeader =
                RequestHeader::build(http::Method::GET, b"/", Some(1)).unwrap();
            req.append_header("Range", "bytes=0-3,2-5,7-8").unwrap();
            req
        }
        fn gen_resp() -> ResponseHeader {
            let mut resp = ResponseHeader::build(200, Some(1)).unwrap();
            resp.append_header("Content-Length", "10").unwrap();
            resp
        }

        // valid multipart range
        let req = gen_req();
        let mut resp = gen_resp();
        let result = range_header_filter(&req, &mut resp);
        let mut boundary_str = String::new();

        assert!(matches!(result, RangeType::Multi(_)));
        if let RangeType::Multi(multi_part_info) = result {
            assert_eq!(multi_part_info.ranges.len(), 3);
            assert_eq!(multi_part_info.ranges[0], Range { start: 0, end: 2 });
            assert_eq!(multi_part_info.ranges[1], Range { start: 3, end: 5 });
            assert_eq!(multi_part_info.ranges[2], Range { start: 6, end: 8 });
            // Verify that multipart info has been set
            assert!(multi_part_info.content_type.is_some());
            assert_eq!(multi_part_info.total_length, 10);
            assert!(!multi_part_info.boundary.is_empty());
            boundary_str = multi_part_info.boundary;
        }
        assert_eq!(resp.status.as_u16(), 206);
        // Verify that boundary is the same in header and in multipartinfo
        assert_eq!(
            resp.headers.get("content-type").unwrap().to_str().unwrap(),
            format!("multipart/byteranges; boundary={boundary_str}")
        );
        assert!(resp.headers.get("content_length").is_none());

        // overlapping range, multipart range is declined
        let req = gen_req_overlap_range();
        let mut resp = gen_resp();
        let result = range_header_filter(&req, &mut resp);

        assert!(matches!(result, RangeType::None));
        assert_eq!(resp.status.as_u16(), 200);
        assert!(resp.headers.get("content-type").is_none());

        // bad multipart range
        let mut req = gen_req();
        req.insert_header("Range", "bytes=1-0, 12-9, 50-40")
            .unwrap();
        let mut resp = gen_resp();
        let result = range_header_filter(&req, &mut resp);
        assert!(matches!(result, RangeType::Invalid));
        assert_eq!(resp.status.as_u16(), 416);
    }

    #[test]
    fn test_if_range() {
        const DATE: &str = "Fri, 07 Jul 2023 22:03:29 GMT";
        const ETAG: &str = "\"1234\"";

        fn gen_req() -> RequestHeader {
            let mut req = RequestHeader::build(http::Method::GET, b"/", Some(1)).unwrap();
            req.append_header("Range", "bytes=0-1").unwrap();
            req
        }
        fn get_multipart_req() -> RequestHeader {
            let mut req = RequestHeader::build(http::Method::GET, b"/", Some(1)).unwrap();
            _ = req.append_header("Range", "bytes=0-1,3-4,6-7");
            req
        }
        fn gen_resp() -> ResponseHeader {
            let mut resp = ResponseHeader::build(200, Some(1)).unwrap();
            resp.append_header("Content-Length", "10").unwrap();
            resp.append_header("Last-Modified", DATE).unwrap();
            resp.append_header("ETag", ETAG).unwrap();
            resp
        }

        // matching Last-Modified date
        let mut req = gen_req();
        req.insert_header("If-Range", DATE).unwrap();
        let mut resp = gen_resp();
        assert_eq!(
            RangeType::new_single(0, 2),
            range_header_filter(&req, &mut resp)
        );

        // non-matching date
        let mut req = gen_req();
        req.insert_header("If-Range", "Fri, 07 Jul 2023 22:03:25 GMT")
            .unwrap();
        let mut resp = gen_resp();
        assert_eq!(RangeType::None, range_header_filter(&req, &mut resp));

        // match ETag
        let mut req = gen_req();
        req.insert_header("If-Range", ETAG).unwrap();
        let mut resp = gen_resp();
        assert_eq!(
            RangeType::new_single(0, 2),
            range_header_filter(&req, &mut resp)
        );

        // non-matching ETags do not result in range
        let mut req = gen_req();
        req.insert_header("If-Range", "\"4567\"").unwrap();
        let mut resp = gen_resp();
        assert_eq!(RangeType::None, range_header_filter(&req, &mut resp));

        let mut req = gen_req();
        req.insert_header("If-Range", "1234").unwrap();
        let mut resp = gen_resp();
        assert_eq!(RangeType::None, range_header_filter(&req, &mut resp));

        // multipart range with If-Range
        let mut req = get_multipart_req();
        req.insert_header("If-Range", DATE).unwrap();
        let mut resp = gen_resp();
        let result = range_header_filter(&req, &mut resp);
        assert!(matches!(result, RangeType::Multi(_)));
        assert_eq!(resp.status.as_u16(), 206);

        // multipart with matching ETag
        let req = get_multipart_req();
        let mut resp = gen_resp();
        assert!(matches!(
            range_header_filter(&req, &mut resp),
            RangeType::Multi(_)
        ));

        // multipart with non-matching If-Range
        let mut req = get_multipart_req();
        req.insert_header("If-Range", "\"wrong\"").unwrap();
        let mut resp = gen_resp();
        assert_eq!(RangeType::None, range_header_filter(&req, &mut resp));
        assert_eq!(resp.status.as_u16(), 200);
    }

    pub struct RangeBodyFilter {
        pub range: RangeType,
        current: usize,
        multipart_idx: Option<usize>,
    }

    impl Default for RangeBodyFilter {
        fn default() -> Self {
            Self::new()
        }
    }

    impl RangeBodyFilter {
        pub fn new() -> Self {
            RangeBodyFilter {
                range: RangeType::None,
                current: 0,
                multipart_idx: None,
            }
        }

        pub fn set(&mut self, range: RangeType) {
            self.range = range.clone();
            if let RangeType::Multi(_) = self.range {
                self.multipart_idx = Some(0);
            }
        }

        // Emit final boundary footer for multipart requests
        pub fn finalize(&self, boundary: &String) -> Option<Bytes> {
            if let RangeType::Multi(_) = self.range {
                Some(Bytes::from(format!("\r\n--{boundary}--\r\n")))
            } else {
                None
            }
        }

        pub fn filter_body(&mut self, data: Option<Bytes>) -> Option<Bytes> {
            match &self.range {
                RangeType::None => data,
                RangeType::Invalid => None,
                RangeType::Single(r) => {
                    let current = self.current;
                    self.current += data.as_ref().map_or(0, |d| d.len());
                    data.and_then(|d| Self::filter_range_data(r.start, r.end, current, d))
                }

                RangeType::Multi(_) => {
                    let data = data?;
                    let current = self.current;
                    let data_len = data.len();
                    self.current += data_len;
                    self.filter_multi_range_body(data, current, data_len)
                }
            }
        }

        fn filter_range_data(
            start: usize,
            end: usize,
            current: usize,
            data: Bytes,
        ) -> Option<Bytes> {
            if current + data.len() < start || current >= end {
                // if the current data is out side the desired range, just drop the data
                None
            } else if current >= start && current + data.len() <= end {
                // all data is within the slice
                Some(data)
            } else {
                // data:  current........current+data.len()
                // range: start...........end
                let slice_start = start.saturating_sub(current);
                let slice_end = std::cmp::min(data.len(), end - current);
                Some(data.slice(slice_start..slice_end))
            }
        }

        // Returns the multipart header for a given range
        fn build_multipart_header(
            &self,
            range: &Range<usize>,
            boundary: &str,
            total_length: &usize,
            content_type: Option<&str>,
        ) -> Bytes {
            Bytes::from(format!(
                "\r\n--{}\r\n{}Content-Range: bytes {}-{}/{}\r\n\r\n",
                boundary,
                content_type.map_or(String::new(), |ct| format!("Content-Type: {ct}\r\n")),
                range.start,
                range.end - 1,
                total_length
            ))
        }

        // Return true if chunk includes the start of the given range
        fn current_chunk_includes_range_start(
            &self,
            range: &Range<usize>,
            current: usize,
            data_len: usize,
        ) -> bool {
            range.start >= current && range.start < current + data_len
        }

        // Return true if chunk includes the end of the given range
        fn current_chunk_includes_range_end(
            &self,
            range: &Range<usize>,
            current: usize,
            data_len: usize,
        ) -> bool {
            range.end > current && range.end <= current + data_len
        }

        fn filter_multi_range_body(
            &mut self,
            data: Bytes,
            current: usize,
            data_len: usize,
        ) -> Option<Bytes> {
            let mut result = BytesMut::new();

            let RangeType::Multi(multi_part_info) = &self.range else {
                return None;
            };

            let multipart_idx = self.multipart_idx.expect("must be set on multirange");
            let final_range = multi_part_info.ranges.last()?;

            let (_, remaining_ranges) = multi_part_info.ranges.as_slice().split_at(multipart_idx);
            // NOTE: current invariant is that the multipart info ranges are disjoint ascending
            // this code is invalid if this invariant is not upheld
            for range in remaining_ranges {
                if let Some(sliced) =
                    Self::filter_range_data(range.start, range.end, current, data.clone())
                {
                    if self.current_chunk_includes_range_start(range, current, data_len) {
                        result.extend_from_slice(&self.build_multipart_header(
                            range,
                            multi_part_info.boundary.as_ref(),
                            &multi_part_info.total_length,
                            multi_part_info.content_type.as_deref(),
                        ));
                    }
                    // Emit the actual data bytes
                    result.extend_from_slice(&sliced);
                    if self.current_chunk_includes_range_end(range, current, data_len) {
                        // If this was the last range, we should emit the final footer too
                        if range == final_range {
                            if let Some(final_chunk) = self.finalize(&multi_part_info.boundary) {
                                result.extend_from_slice(&final_chunk);
                            }
                        }
                        // done with this range
                        self.multipart_idx = Some(self.multipart_idx.expect("must be set") + 1);
                    }
                } else {
                    // no part of the data was within this range,
                    // so lower bound of this range (and remaining ranges) must be
                    // > current + data_len
                    break;
                }
            }
            if result.is_empty() {
                None
            } else {
                Some(result.freeze())
            }
        }
    }

    #[test]
    fn test_range_body_filter_single() {
        let mut body_filter = RangeBodyFilter::new();
        assert_eq!(body_filter.filter_body(Some("123".into())).unwrap(), "123");

        let mut body_filter = RangeBodyFilter::new();
        body_filter.set(RangeType::Invalid);
        assert!(body_filter.filter_body(Some("123".into())).is_none());

        let mut body_filter = RangeBodyFilter::new();
        body_filter.set(RangeType::new_single(0, 1));
        assert_eq!(body_filter.filter_body(Some("012".into())).unwrap(), "0");
        assert!(body_filter.filter_body(Some("345".into())).is_none());

        let mut body_filter = RangeBodyFilter::new();
        body_filter.set(RangeType::new_single(4, 6));
        assert!(body_filter.filter_body(Some("012".into())).is_none());
        assert_eq!(body_filter.filter_body(Some("345".into())).unwrap(), "45");
        assert!(body_filter.filter_body(Some("678".into())).is_none());

        let mut body_filter = RangeBodyFilter::new();
        body_filter.set(RangeType::new_single(1, 7));
        assert_eq!(body_filter.filter_body(Some("012".into())).unwrap(), "12");
        assert_eq!(body_filter.filter_body(Some("345".into())).unwrap(), "345");
        assert_eq!(body_filter.filter_body(Some("678".into())).unwrap(), "6");
    }

    #[test]
    fn test_range_body_filter_multipart() {
        // Test #1 - Test multipart ranges from 1 chunk
        let data = Bytes::from("0123456789");
        let ranges = vec![0..3, 6..9];
        let content_length = data.len();
        let mut body_filter = RangeBodyFilter::new();
        body_filter.set(RangeType::new_multi(ranges.clone()));

        body_filter
            .range
            .update_multirange_info(content_length, None);

        let multi_range_info = body_filter
            .range
            .get_multirange_info()
            .cloned()
            .expect("Multipart Ranges should have MultiPartInfo struct");

        // Pass the whole body in one chunk
        let output = body_filter.filter_body(Some(data)).unwrap();
        let footer = body_filter.finalize(&multi_range_info.boundary).unwrap();

        // Convert to String so that we can inspect whole response
        let output_str = str::from_utf8(&output).unwrap();
        let final_boundary = str::from_utf8(&footer).unwrap();
        let boundary = &multi_range_info.boundary;

        // Check part headers
        for (i, range) in ranges.iter().enumerate() {
            let header = &format!(
                "--{}\r\nContent-Range: bytes {}-{}/{}\r\n\r\n",
                boundary,
                range.start,
                range.end - 1,
                content_length
            );
            assert!(
                output_str.contains(header),
                "Missing part header {} in multipart body",
                i
            );
            // Check body matches
            let expected_body = &"0123456789"[range.clone()];
            assert!(
                output_str.contains(expected_body),
                "Missing body {} for range {:?}",
                expected_body,
                range
            )
        }
        // Check the final boundary footer
        assert_eq!(final_boundary, format!("\r\n--{}--\r\n", boundary));

        // Test #2 - Test multipart ranges from multiple chunks
        let full_body = b"0123456789";
        let ranges = vec![0..2, 4..6, 8..9];
        let content_length = full_body.len();
        let content_type = "text/plain".to_string();
        let mut body_filter = RangeBodyFilter::new();
        body_filter.set(RangeType::new_multi(ranges.clone()));

        body_filter
            .range
            .update_multirange_info(content_length, Some(content_type.clone()));

        let multi_range_info = body_filter
            .range
            .get_multirange_info()
            .cloned()
            .expect("Multipart Ranges should have MultiPartInfo struct");

        // Split the body into 4 chunks
        let chunk1 = Bytes::from_static(b"012");
        let chunk2 = Bytes::from_static(b"345");
        let chunk3 = Bytes::from_static(b"678");
        let chunk4 = Bytes::from_static(b"9");

        let mut collected_bytes = BytesMut::new();
        for chunk in [chunk1, chunk2, chunk3, chunk4] {
            if let Some(filtered) = body_filter.filter_body(Some(chunk)) {
                collected_bytes.extend_from_slice(&filtered);
            }
        }
        if let Some(final_boundary) = body_filter.finalize(&multi_range_info.boundary) {
            collected_bytes.extend_from_slice(&final_boundary);
        }

        let output_str = str::from_utf8(&collected_bytes).unwrap();
        let boundary = multi_range_info.boundary;

        for (i, range) in ranges.iter().enumerate() {
            let header = &format!(
                "--{}\r\nContent-Type: {}\r\nContent-Range: bytes {}-{}/{}\r\n\r\n",
                boundary,
                content_type,
                range.start,
                range.end - 1,
                content_length
            );
            let expected_body = &full_body[range.clone()];
            let expected_output = format!("{}{}", header, str::from_utf8(expected_body).unwrap());

            assert!(
                output_str.contains(&expected_output),
                "Missing or malformed part {} in multipart body. \n Expected: \n{}\n Got: \n{}",
                i,
                expected_output,
                output_str
            )
        }

        assert!(
            output_str.ends_with(&format!("\r\n--{}--\r\n", boundary)),
            "Missing final boundary"
        );

        // Test #3 - Test multipart ranges from multiple chunks, with ranges spanning chunks
        let full_body = b"abcdefghijkl";
        let ranges = vec![2..7, 9..11];
        let content_length = full_body.len();
        let content_type = "application/octet-stream".to_string();
        let mut body_filter = RangeBodyFilter::new();
        body_filter.set(RangeType::new_multi(ranges.clone()));

        body_filter
            .range
            .update_multirange_info(content_length, Some(content_type.clone()));

        let multi_range_info = body_filter
            .range
            .clone()
            .get_multirange_info()
            .cloned()
            .expect("Multipart Ranges should have MultiPartInfo struct");

        // Split the body into 4 chunks
        let chunk1 = Bytes::from_static(b"abc");
        let chunk2 = Bytes::from_static(b"def");
        let chunk3 = Bytes::from_static(b"ghi");
        let chunk4 = Bytes::from_static(b"jkl");

        let mut collected_bytes = BytesMut::new();
        for chunk in [chunk1, chunk2, chunk3, chunk4] {
            if let Some(filtered) = body_filter.filter_body(Some(chunk)) {
                collected_bytes.extend_from_slice(&filtered);
            }
        }
        if let Some(final_boundary) = body_filter.finalize(&multi_range_info.boundary) {
            collected_bytes.extend_from_slice(&final_boundary);
        }

        let output_str = str::from_utf8(&collected_bytes).unwrap();
        let boundary = &multi_range_info.boundary;

        let header1 = &format!(
            "--{}\r\nContent-Type: {}\r\nContent-Range: bytes {}-{}/{}\r\n\r\n",
            boundary,
            content_type,
            ranges[0].start,
            ranges[0].end - 1,
            content_length
        );
        let header2 = &format!(
            "--{}\r\nContent-Type: {}\r\nContent-Range: bytes {}-{}/{}\r\n\r\n",
            boundary,
            content_type,
            ranges[1].start,
            ranges[1].end - 1,
            content_length
        );

        assert!(output_str.contains(header1));
        assert!(output_str.contains(header2));

        let expected_body_slices = ["cdefg", "jk"];

        assert!(
            output_str.contains(expected_body_slices[0]),
            "Missing expected sliced body {}",
            expected_body_slices[0]
        );

        assert!(
            output_str.contains(expected_body_slices[1]),
            "Missing expected sliced body {}",
            expected_body_slices[1]
        );

        assert!(
            output_str.ends_with(&format!("\r\n--{}--\r\n", boundary)),
            "Missing final boundary"
        );
    }
}

// a state machine for proxy logic to tell when to use cache in the case of
// miss/revalidation/error.
#[derive(Debug)]
pub(crate) enum ServeFromCache {
    // not using cache
    Off,
    // should serve cache header
    CacheHeader,
    // should serve cache header only
    CacheHeaderOnly,
    // should serve cache header only but upstream response should be admitted to cache
    CacheHeaderOnlyMiss,
    // should serve cache body with a bool to indicate if it has already called seek on the hit handler
    CacheBody(bool),
    // should serve cache header but upstream response should be admitted to cache
    // This is the starting state for misses, which go to CacheBodyMiss or
    // CacheHeaderOnlyMiss before ending at DoneMiss
    CacheHeaderMiss,
    // should serve cache body but upstream response should be admitted to cache, bool to indicate seek status
    CacheBodyMiss(bool),
    // done serving cache body
    Done,
    // done serving cache body, but upstream response should continue to be admitted to cache
    DoneMiss,
}

impl ServeFromCache {
    pub fn new() -> Self {
        Self::Off
    }

    pub fn is_on(&self) -> bool {
        !matches!(self, Self::Off)
    }

    pub fn is_miss(&self) -> bool {
        matches!(
            self,
            Self::CacheHeaderMiss
                | Self::CacheHeaderOnlyMiss
                | Self::CacheBodyMiss(_)
                | Self::DoneMiss
        )
    }

    pub fn is_miss_header(&self) -> bool {
        // NOTE: this check is for checking if miss was just enabled, so it is excluding
        // HeaderOnlyMiss
        matches!(self, Self::CacheHeaderMiss)
    }

    pub fn is_miss_body(&self) -> bool {
        matches!(self, Self::CacheBodyMiss(_))
    }

    pub fn should_discard_upstream(&self) -> bool {
        self.is_on() && !self.is_miss()
    }

    pub fn should_send_to_downstream(&self) -> bool {
        !self.is_on()
    }

    pub fn enable(&mut self) {
        *self = Self::CacheHeader;
    }

    pub fn enable_miss(&mut self) {
        if !self.is_on() {
            *self = Self::CacheHeaderMiss;
        }
    }

    pub fn enable_header_only(&mut self) {
        match self {
            Self::CacheBody(_) => *self = Self::Done, // TODO: make sure no body is read yet
            Self::CacheBodyMiss(_) => *self = Self::DoneMiss,
            _ => {
                if self.is_miss() {
                    *self = Self::CacheHeaderOnlyMiss;
                } else {
                    *self = Self::CacheHeaderOnly;
                }
            }
        }
    }

    // This function is (best effort) cancel-safe to be used in select
    pub async fn next_http_task(
        &mut self,
        cache: &mut HttpCache,
        range: &mut RangeBodyFilter,
    ) -> Result<HttpTask> {
        if !cache.enabled() {
            // Cache is disabled due to internal error
            // TODO: if nothing is sent to eyeball yet, figure out a way to recovery by
            // fetching from upstream
            return Error::e_explain(InternalError, "Cache disabled");
        }
        match self {
            Self::Off => panic!("ProxyUseCache not enabled"),
            Self::CacheHeader => {
                *self = Self::CacheBody(true);
                Ok(HttpTask::Header(cache_hit_header(cache), false)) // false for now
            }
            Self::CacheHeaderMiss => {
                *self = Self::CacheBodyMiss(true);
                Ok(HttpTask::Header(cache_hit_header(cache), false)) // false for now
            }
            Self::CacheHeaderOnly => {
                *self = Self::Done;
                Ok(HttpTask::Header(cache_hit_header(cache), true))
            }
            Self::CacheHeaderOnlyMiss => {
                *self = Self::DoneMiss;
                Ok(HttpTask::Header(cache_hit_header(cache), true))
            }
            Self::CacheBody(should_seek) => {
                if *should_seek {
                    self.maybe_seek_hit_handler(cache, range)?;
                }
                if let Some(b) = cache.hit_handler().read_body().await? {
                    Ok(HttpTask::Body(Some(b), false)) // false for now
                } else {
                    *self = Self::Done;
                    Ok(HttpTask::Done)
                }
            }
            Self::CacheBodyMiss(should_seek) => {
                if *should_seek {
                    self.maybe_seek_miss_handler(cache, range)?;
                }
                // safety: called of enable_miss() call it only if the async_body_reader exist
                if let Some(b) = cache.miss_body_reader().unwrap().read_body().await? {
                    Ok(HttpTask::Body(Some(b), false)) // false for now
                } else {
                    *self = Self::DoneMiss;
                    Ok(HttpTask::Done)
                }
            }
            Self::Done => Ok(HttpTask::Done),
            Self::DoneMiss => Ok(HttpTask::Done),
        }
    }

    fn maybe_seek_miss_handler(
        &mut self,
        cache: &mut HttpCache,
        range_filter: &mut RangeBodyFilter,
    ) -> Result<()> {
        if let RangeType::Single(range) = &range_filter.range {
            // safety: called only if the async_body_reader exists
            if cache.miss_body_reader().unwrap().can_seek() {
                cache
                    .miss_body_reader()
                    // safety: called only if the async_body_reader exists
                    .unwrap()
                    .seek(range.start, Some(range.end))
                    .or_err(InternalError, "cannot seek miss handler")?;
                // Because the miss body reader is seeking, we no longer need the
                // RangeBodyFilter's help to return the requested byte range.
                range_filter.range = RangeType::None;
            }
        }
        *self = Self::CacheBodyMiss(false);
        Ok(())
    }

    fn maybe_seek_hit_handler(
        &mut self,
        cache: &mut HttpCache,
        range_filter: &mut RangeBodyFilter,
    ) -> Result<()> {
        match &range_filter.range {
            RangeType::Single(range) => {
                if cache.hit_handler().can_seek() {
                    cache
                        .hit_handler()
                        .seek(range.start, Some(range.end))
                        .or_err(InternalError, "cannot seek hit handler")?;
                    // Because the hit handler is seeking, we no longer need the
                    // RangeBodyFilter's help to return the requested byte range.
                    range_filter.range = RangeType::None;
                }
            }
            RangeType::Multi(_) => {
                // For multipart ranges, we will handle the seeking in
                // the body filter per part for now.
                // TODO: implement seek for multipart range
            }
            _ => {}
        }
        *self = Self::CacheBody(false);
        Ok(())
    }
}
