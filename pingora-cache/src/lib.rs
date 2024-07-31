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

//! The HTTP caching layer for proxies.

#![allow(clippy::new_without_default)]

use http::{method::Method, request::Parts as ReqHeader, response::Parts as RespHeader};
use key::{CacheHashKey, HashBinary};
use lock::WritePermit;
use pingora_error::Result;
use pingora_http::ResponseHeader;
use std::time::{Duration, Instant, SystemTime};
use trace::CacheTraceCTX;

pub mod cache_control;
pub mod eviction;
pub mod filters;
pub mod hashtable;
pub mod key;
pub mod lock;
pub mod max_file_size;
mod memory;
pub mod meta;
pub mod predictor;
pub mod put;
pub mod storage;
pub mod trace;
mod variance;

use crate::max_file_size::MaxFileSizeMissHandler;
pub use key::CacheKey;
use lock::{CacheLock, LockStatus, Locked};
pub use memory::MemCache;
pub use meta::{CacheMeta, CacheMetaDefaults};
pub use storage::{HitHandler, MissHandler, PurgeType, Storage};
pub use variance::VarianceBuilder;

pub mod prelude {}

/// The state machine for http caching
///
/// This object is used to handle the state and transitions for HTTP caching through the life of a
/// request.
pub struct HttpCache {
    phase: CachePhase,
    // Box the rest so that a disabled HttpCache struct is small
    inner: Option<Box<HttpCacheInner>>,
}

/// This reflects the phase of HttpCache during the lifetime of a request
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CachePhase {
    /// Cache disabled, with reason (NeverEnabled if never explicitly used)
    Disabled(NoCacheReason),
    /// Cache enabled but nothing is set yet
    Uninit,
    /// Cache was enabled, the request decided not to use it
    // HttpCache.inner is kept
    Bypass,
    /// Awaiting the cache key to be generated
    CacheKey,
    /// Cache hit
    Hit,
    /// No cached asset is found
    Miss,
    /// A staled (expired) asset is found
    Stale,
    /// A staled (expired) asset was found, so a fresh one was fetched
    Expired,
    /// A staled (expired) asset was found, and it was revalidated to be fresh
    Revalidated,
    /// Revalidated, but deemed uncacheable, so we do not freshen it
    RevalidatedNoCache(NoCacheReason),
}

impl CachePhase {
    /// Convert [CachePhase] as `str`, for logging and debugging.
    pub fn as_str(&self) -> &'static str {
        match self {
            CachePhase::Disabled(_) => "disabled",
            CachePhase::Uninit => "uninitialized",
            CachePhase::Bypass => "bypass",
            CachePhase::CacheKey => "key",
            CachePhase::Hit => "hit",
            CachePhase::Miss => "miss",
            CachePhase::Stale => "stale",
            CachePhase::Expired => "expired",
            CachePhase::Revalidated => "revalidated",
            CachePhase::RevalidatedNoCache(_) => "revalidated-nocache",
        }
    }
}

/// The possible reasons for not caching
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum NoCacheReason {
    /// Caching is not enabled to begin with
    NeverEnabled,
    /// Origin directives indicated this was not cacheable
    OriginNotCache,
    /// Response size was larger than the cache's configured maximum asset size
    ResponseTooLarge,
    /// Due to internal caching storage error
    StorageError,
    /// Due to other types of internal issues
    InternalError,
    /// will be cacheable but skip cache admission now
    ///
    /// This happens when the cache predictor predicted that this request is not cacheable, but
    /// the response turns out to be OK to cache. However, it might be too large to re-enable caching
    /// for this request.
    Deferred,
    /// The writer of the cache lock sees that the request is not cacheable (Could be OriginNotCache)
    CacheLockGiveUp,
    /// This request waited too long for the writer of the cache lock to finish, so this request will
    /// fetch from the origin without caching
    CacheLockTimeout,
    /// Other custom defined reasons
    Custom(&'static str),
}

impl NoCacheReason {
    /// Convert [NoCacheReason] as `str`, for logging and debugging.
    pub fn as_str(&self) -> &'static str {
        use NoCacheReason::*;
        match self {
            NeverEnabled => "NeverEnabled",
            OriginNotCache => "OriginNotCache",
            ResponseTooLarge => "ResponseTooLarge",
            StorageError => "StorageError",
            InternalError => "InternalError",
            Deferred => "Deferred",
            CacheLockGiveUp => "CacheLockGiveUp",
            CacheLockTimeout => "CacheLockTimeout",
            Custom(s) => s,
        }
    }
}

/// Response cacheable decision
///
///
#[derive(Debug)]
pub enum RespCacheable {
    Cacheable(CacheMeta),
    Uncacheable(NoCacheReason),
}

impl RespCacheable {
    /// Whether it is cacheable
    #[inline]
    pub fn is_cacheable(&self) -> bool {
        matches!(*self, Self::Cacheable(_))
    }

    /// Unwrap [RespCacheable] to get the [CacheMeta] stored
    /// # Panic
    /// Panic when this object is not cacheable. Check [Self::is_cacheable()] first.
    pub fn unwrap_meta(self) -> CacheMeta {
        match self {
            Self::Cacheable(meta) => meta,
            Self::Uncacheable(_) => panic!("expected Cacheable value"),
        }
    }
}

/// Freshness state of cache hit asset
///
///
#[derive(Debug, Copy, Clone)]
pub enum HitStatus {
    Expired,
    ForceExpired,
    FailedHitFilter,
    Fresh,
}

impl HitStatus {
    /// For displaying cache hit status
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Expired => "expired",
            Self::ForceExpired => "force_expired",
            Self::FailedHitFilter => "failed_hit_filter",
            Self::Fresh => "fresh",
        }
    }

    /// Whether cached asset can be served as fresh
    pub fn is_fresh(&self) -> bool {
        match self {
            Self::Expired | Self::ForceExpired | Self::FailedHitFilter => false,
            Self::Fresh => true,
        }
    }
}

struct HttpCacheInner {
    pub key: Option<CacheKey>,
    pub meta: Option<CacheMeta>,
    // when set, even if an asset exists, it would only be considered valid after this timestamp
    pub valid_after: Option<SystemTime>,
    // when set, an asset will be rejected from the cache if it exceeds this size in bytes
    pub max_file_size_bytes: Option<usize>,
    pub miss_handler: Option<MissHandler>,
    pub body_reader: Option<HitHandler>,
    pub storage: &'static (dyn storage::Storage + Sync), // static for now
    pub eviction: Option<&'static (dyn eviction::EvictionManager + Sync)>,
    pub predictor: Option<&'static (dyn predictor::CacheablePredictor + Sync)>,
    pub lock: Option<Locked>, // TODO: these 3 fields should come in 1 sub struct
    pub cache_lock: Option<&'static CacheLock>,
    pub lock_duration: Option<Duration>,
    // time spent in cache lookup and reading the header
    pub lookup_duration: Option<Duration>,
    pub traces: trace::CacheTraceCTX,
}

impl HttpCache {
    /// Create a new [HttpCache].
    ///
    /// Caching is not enabled by default.
    pub fn new() -> Self {
        HttpCache {
            phase: CachePhase::Disabled(NoCacheReason::NeverEnabled),
            inner: None,
        }
    }

    /// Whether the cache is enabled
    pub fn enabled(&self) -> bool {
        !matches!(self.phase, CachePhase::Disabled(_) | CachePhase::Bypass)
    }

    /// Whether the cache is being bypassed
    pub fn bypassing(&self) -> bool {
        matches!(self.phase, CachePhase::Bypass)
    }

    /// Return the [CachePhase]
    pub fn phase(&self) -> CachePhase {
        self.phase
    }

    /// Whether anything was fetched from the upstream
    ///
    /// This essentially checks all possible [CachePhase] who need to contact the upstream server
    pub fn upstream_used(&self) -> bool {
        use CachePhase::*;
        match self.phase {
            Disabled(_) | Bypass | Miss | Expired | Revalidated | RevalidatedNoCache(_) => true,
            Hit | Stale => false,
            Uninit | CacheKey => false, // invalid states for this call, treat them as false to keep it simple
        }
    }

    /// Check whether the backend storage is the type `T`.
    pub fn storage_type_is<T: 'static>(&self) -> bool {
        self.inner
            .as_ref()
            .and_then(|inner| inner.storage.as_any().downcast_ref::<T>())
            .is_some()
    }

    /// Disable caching
    pub fn disable(&mut self, reason: NoCacheReason) {
        use NoCacheReason::*;
        match self.phase {
            CachePhase::Disabled(_) => {
                // replace reason
                self.phase = CachePhase::Disabled(reason);
            }
            _ => {
                self.phase = CachePhase::Disabled(reason);
                if let Some(inner) = self.inner.as_mut() {
                    let lock = inner.lock.take();
                    if let Some(Locked::Write(_r)) = lock {
                        let lock_status = match reason {
                            // let the next request try to fetch it
                            InternalError | StorageError | Deferred => LockStatus::TransientError,
                            // no need for the lock anymore
                            OriginNotCache | ResponseTooLarge => LockStatus::GiveUp,
                            // not sure which LockStatus make sense, we treat it as GiveUp for now
                            Custom(_) => LockStatus::GiveUp,
                            // should never happen, NeverEnabled shouldn't hold a lock
                            NeverEnabled => panic!("NeverEnabled holds a write lock"),
                            CacheLockGiveUp | CacheLockTimeout => {
                                panic!("CacheLock* are for cache lock readers only")
                            }
                        };
                        inner
                            .cache_lock
                            .unwrap()
                            .release(inner.key.as_ref().unwrap(), lock_status);
                    }
                }
                // log initial disable reason
                self.inner_mut()
                    .traces
                    .cache_span
                    .set_tag(|| trace::Tag::new("disable_reason", reason.as_str()));
                self.inner = None;
            }
        }
    }

    /* The following methods panic when they are used in the wrong phase.
     * This is better than returning errors as such panics are only caused by coding error, which
     * should be fixed right away. Tokio runtime only crashes the current task instead of the whole
     * program when these panics happen. */

    /// Set the cache to bypass
    ///
    /// # Panic
    /// This call is only allowed in [CachePhase::CacheKey] phase (before any cache lookup is performed).
    /// Use it in any other phase will lead to panic.
    pub fn bypass(&mut self) {
        match self.phase {
            CachePhase::CacheKey => {
                // before cache lookup / found / miss
                self.phase = CachePhase::Bypass;
                self.inner_mut()
                    .traces
                    .cache_span
                    .set_tag(|| trace::Tag::new("bypassed", true));
            }
            _ => panic!("wrong phase to bypass HttpCache {:?}", self.phase),
        }
    }

    /// Enable the cache
    ///
    /// - `storage`: the cache storage backend that implements [storage::Storage]
    /// - `eviction`: optionally the eviction manager, without it, nothing will be evicted from the storage
    /// - `predictor`: optionally a cache predictor. The cache predictor predicts whether something is likely
    ///   to be cacheable or not. This is useful because the proxy can apply different types of optimization to
    ///   cacheable and uncacheable requests.
    /// - `cache_lock`: optionally a cache lock which handles concurrent lookups to the same asset. Without it
    ///   such lookups will all be allowed to fetch the asset independently.
    pub fn enable(
        &mut self,
        storage: &'static (dyn storage::Storage + Sync),
        eviction: Option<&'static (dyn eviction::EvictionManager + Sync)>,
        predictor: Option<&'static (dyn predictor::CacheablePredictor + Sync)>,
        cache_lock: Option<&'static CacheLock>,
    ) {
        match self.phase {
            CachePhase::Disabled(_) => {
                self.phase = CachePhase::Uninit;
                self.inner = Some(Box::new(HttpCacheInner {
                    key: None,
                    meta: None,
                    valid_after: None,
                    max_file_size_bytes: None,
                    miss_handler: None,
                    body_reader: None,
                    storage,
                    eviction,
                    predictor,
                    lock: None,
                    cache_lock,
                    lock_duration: None,
                    lookup_duration: None,
                    traces: CacheTraceCTX::new(),
                }));
            }
            _ => panic!("Cannot enable already enabled HttpCache {:?}", self.phase),
        }
    }

    // Enable distributed tracing
    pub fn enable_tracing(&mut self, parent_span: trace::Span) {
        if let Some(inner) = self.inner.as_mut() {
            inner.traces.enable(parent_span);
        }
    }

    // Get the cache `miss` tracing span
    pub fn get_miss_span(&mut self) -> Option<trace::SpanHandle> {
        self.inner.as_mut().map(|i| i.traces.get_miss_span())
    }

    // shortcut to access inner, panic if phase is disabled
    #[inline]
    fn inner_mut(&mut self) -> &mut HttpCacheInner {
        self.inner.as_mut().unwrap()
    }

    #[inline]
    fn inner(&self) -> &HttpCacheInner {
        self.inner.as_ref().unwrap()
    }

    /// Set the cache key
    /// # Panic
    /// Cache key is only allowed to be set in its own phase. Set it in other phases will cause panic.
    pub fn set_cache_key(&mut self, key: CacheKey) {
        match self.phase {
            CachePhase::Uninit | CachePhase::CacheKey => {
                self.phase = CachePhase::CacheKey;
                self.inner_mut().key = Some(key);
            }
            _ => panic!("wrong phase {:?}", self.phase),
        }
    }

    /// Return the cache key used for asset lookup
    /// # Panic
    /// Can only be called after the cache key is set and the cache is not disabled. Panic otherwise.
    pub fn cache_key(&self) -> &CacheKey {
        match self.phase {
            CachePhase::Disabled(_) | CachePhase::Uninit => panic!("wrong phase {:?}", self.phase),
            _ => self.inner().key.as_ref().unwrap(),
        }
    }

    /// Return the max size allowed to be cached.
    pub fn max_file_size_bytes(&self) -> Option<usize> {
        match self.phase {
            CachePhase::Disabled(_) | CachePhase::Uninit => panic!("wrong phase {:?}", self.phase),
            _ => self.inner().max_file_size_bytes,
        }
    }

    /// Set the maximum response _body_ size in bytes that will be admitted to the cache.
    ///
    /// Response header size does not contribute to the max file size.
    pub fn set_max_file_size_bytes(&mut self, max_file_size_bytes: usize) {
        match self.phase {
            CachePhase::Disabled(_) => panic!("wrong phase {:?}", self.phase),
            _ => {
                self.inner_mut().max_file_size_bytes = Some(max_file_size_bytes);
            }
        }
    }

    /// Set that cache is found in cache storage.
    ///
    /// This function is called after [Self::cache_lookup()] which returns the [CacheMeta] and
    /// [HitHandler].
    ///
    /// The `hit_status` enum allows the caller to force expire assets.
    pub fn cache_found(&mut self, meta: CacheMeta, hit_handler: HitHandler, hit_status: HitStatus) {
        match self.phase {
            // Stale allowed because of cache lock and then retry
            CachePhase::CacheKey | CachePhase::Stale => {
                self.phase = if hit_status.is_fresh() {
                    CachePhase::Hit
                } else {
                    CachePhase::Stale
                };
                let phase = self.phase;
                let inner = self.inner_mut();
                let key = inner.key.as_ref().unwrap();
                if phase == CachePhase::Stale {
                    if let Some(lock) = inner.cache_lock.as_ref() {
                        inner.lock = Some(lock.lock(key));
                    }
                }
                inner.traces.log_meta(&meta);
                if let Some(eviction) = inner.eviction {
                    // TODO: make access() accept CacheKey
                    let cache_key = key.to_compact();
                    // FIXME: get size
                    eviction.access(&cache_key, 0, meta.0.internal.fresh_until);
                }
                inner.traces.start_hit_span(phase, hit_status);
                inner.meta = Some(meta);
                inner.body_reader = Some(hit_handler);
            }
            _ => panic!("wrong phase {:?}", self.phase),
        }
    }

    /// Mark `self` to be cache miss.
    ///
    /// This function is called after [Self::cache_lookup()] finds nothing or the caller decides
    /// not to use the assets found.
    /// # Panic
    /// Panic in other phases.
    pub fn cache_miss(&mut self) {
        match self.phase {
            // from CacheKey: set state to miss during cache lookup
            // from Bypass: response became cacheable, set state to miss to cache
            CachePhase::CacheKey | CachePhase::Bypass => {
                self.phase = CachePhase::Miss;
                self.inner_mut().traces.start_miss_span();
            }
            _ => panic!("wrong phase {:?}", self.phase),
        }
    }

    /// Return the [HitHandler]
    /// # Panic
    /// Call this after [Self::cache_found()], panic in other phases.
    pub fn hit_handler(&mut self) -> &mut HitHandler {
        match self.phase {
            CachePhase::Hit
            | CachePhase::Stale
            | CachePhase::Revalidated
            | CachePhase::RevalidatedNoCache(_) => self.inner_mut().body_reader.as_mut().unwrap(),
            _ => panic!("wrong phase {:?}", self.phase),
        }
    }

    /// Return the body reader during a cache admission (miss/expired) which decouples the downstream
    /// read and upstream cache write
    pub fn miss_body_reader(&mut self) -> Option<&mut HitHandler> {
        match self.phase {
            CachePhase::Miss | CachePhase::Expired => {
                let inner = self.inner_mut();
                if inner.storage.support_streaming_partial_write() {
                    inner.body_reader.as_mut()
                } else {
                    // body_reader could be set even when the storage doesn't support streaming
                    // Expired cache would have the reader set.
                    None
                }
            }
            _ => None,
        }
    }

    /// Call this when cache hit is fully read.
    ///
    /// This call will release resource if any and log the timing in tracing if set.
    /// # Panic
    /// Panic in phases where there is no cache hit.
    pub async fn finish_hit_handler(&mut self) -> Result<()> {
        match self.phase {
            CachePhase::Hit
            | CachePhase::Miss
            | CachePhase::Expired
            | CachePhase::Stale
            | CachePhase::Revalidated
            | CachePhase::RevalidatedNoCache(_) => {
                let inner = self.inner_mut();
                if inner.body_reader.is_none() {
                    // already finished, we allow calling this function more than once
                    return Ok(());
                }
                let body_reader = inner.body_reader.take().unwrap();
                let key = inner.key.as_ref().unwrap();
                let result = body_reader
                    .finish(inner.storage, key, &inner.traces.hit_span.handle())
                    .await;
                inner.traces.finish_hit_span();
                result
            }
            _ => panic!("wrong phase {:?}", self.phase),
        }
    }

    /// Set the [MissHandler] according to cache_key and meta, can only call once
    pub async fn set_miss_handler(&mut self) -> Result<()> {
        match self.phase {
            // set_miss_handler() needs to be called after set_cache_meta() (which change Stale to Expire).
            // This is an artificial rule to enforce the state transitions
            CachePhase::Miss | CachePhase::Expired => {
                let max_file_size_bytes = self.max_file_size_bytes();

                let inner = self.inner_mut();
                if inner.miss_handler.is_some() {
                    panic!("write handler is already set")
                }
                let meta = inner.meta.as_ref().unwrap();
                let key = inner.key.as_ref().unwrap();
                let miss_handler = inner
                    .storage
                    .get_miss_handler(key, meta, &inner.traces.get_miss_span())
                    .await?;

                inner.miss_handler = if let Some(max_size) = max_file_size_bytes {
                    Some(Box::new(MaxFileSizeMissHandler::new(
                        miss_handler,
                        max_size,
                    )))
                } else {
                    Some(miss_handler)
                };

                if inner.storage.support_streaming_partial_write() {
                    // If a reader can access partial write, the cache lock can be released here
                    // to let readers start reading the body.
                    let lock = inner.lock.take();
                    if let Some(Locked::Write(_r)) = lock {
                        inner.cache_lock.unwrap().release(key, LockStatus::Done);
                    }
                    // Downstream read and upstream write can be decoupled
                    let body_reader = inner
                        .storage
                        .lookup(key, &inner.traces.get_miss_span())
                        .await?;

                    if let Some((_meta, body_reader)) = body_reader {
                        inner.body_reader = Some(body_reader);
                    } else {
                        // body_reader should exist now because streaming_partial_write is to support it
                        panic!("unable to get body_reader for {:?}", meta);
                    }
                }
                Ok(())
            }
            _ => panic!("wrong phase {:?}", self.phase),
        }
    }

    /// Return the [MissHandler] to write the response body to cache.
    ///
    /// `None`: the handler has not been set or already finished
    pub fn miss_handler(&mut self) -> Option<&mut MissHandler> {
        match self.phase {
            CachePhase::Miss | CachePhase::Expired => self.inner_mut().miss_handler.as_mut(),
            _ => panic!("wrong phase {:?}", self.phase),
        }
    }

    /// Finish cache admission
    ///
    /// If [self] is dropped without calling this, the cache admission is considered incomplete and
    /// should be cleaned up.
    ///
    /// This call will also trigger eviction if set.
    pub async fn finish_miss_handler(&mut self) -> Result<()> {
        match self.phase {
            CachePhase::Miss | CachePhase::Expired => {
                let inner = self.inner_mut();
                if inner.miss_handler.is_none() {
                    // already finished, we allow calling this function more than once
                    return Ok(());
                }
                let miss_handler = inner.miss_handler.take().unwrap();
                let size = miss_handler.finish().await?;
                let lock = inner.lock.take();
                let key = inner.key.as_ref().unwrap();
                if let Some(Locked::Write(_r)) = lock {
                    // no need to call r.unlock() because release() will call it
                    // r is a guard to make sure the lock is unlocked when this request is dropped
                    inner.cache_lock.unwrap().release(key, LockStatus::Done);
                }
                if let Some(eviction) = inner.eviction {
                    let cache_key = key.to_compact();
                    let meta = inner.meta.as_ref().unwrap();
                    let evicted = eviction.admit(cache_key, size, meta.0.internal.fresh_until);
                    // TODO: make this async
                    let span = inner.traces.child("eviction");
                    let handle = span.handle();
                    for item in evicted {
                        // TODO: warn/log the error
                        let _ = inner
                            .storage
                            .purge(&item, PurgeType::Eviction, &handle)
                            .await;
                    }
                }
                inner.traces.finish_miss_span();
                Ok(())
            }
            _ => panic!("wrong phase {:?}", self.phase),
        }
    }

    /// Set the [CacheMeta] of the cache
    pub fn set_cache_meta(&mut self, meta: CacheMeta) {
        match self.phase {
            // TODO: store the staled meta somewhere else for future use?
            CachePhase::Stale | CachePhase::Miss => {
                let inner = self.inner_mut();
                inner.traces.log_meta(&meta);
                inner.meta = Some(meta);
            }
            _ => panic!("wrong phase {:?}", self.phase),
        }
        if self.phase == CachePhase::Stale {
            self.phase = CachePhase::Expired;
        }
    }

    /// Set the [CacheMeta] of the cache after revalidation.
    ///
    /// Certain info such as the original cache admission time will be preserved. Others will
    /// be replaced by the input `meta`.
    pub async fn revalidate_cache_meta(&mut self, mut meta: CacheMeta) -> Result<bool> {
        let result = match self.phase {
            CachePhase::Stale => {
                let inner = self.inner_mut();
                // TODO: we should keep old meta in place, just use new one to update it
                // that requires cacheable_filter to take a mut header and just return InternalMeta

                // update new meta with old meta's created time
                let created = inner.meta.as_ref().unwrap().0.internal.created;
                meta.0.internal.created = created;
                // meta.internal.updated was already set to new meta's `created`,
                // no need to set `updated` here

                inner.meta.replace(meta);

                let lock = inner.lock.take();
                if let Some(Locked::Write(_r)) = lock {
                    inner
                        .cache_lock
                        .unwrap()
                        .release(inner.key.as_ref().unwrap(), LockStatus::Done);
                }

                let mut span = inner.traces.child("update_meta");
                // TODO: this call can be async
                let result = inner
                    .storage
                    .update_meta(
                        inner.key.as_ref().unwrap(),
                        inner.meta.as_ref().unwrap(),
                        &span.handle(),
                    )
                    .await;
                span.set_tag(|| trace::Tag::new("updated", result.is_ok()));
                result
            }
            _ => panic!("wrong phase {:?}", self.phase),
        };
        self.phase = CachePhase::Revalidated;
        result
    }

    /// After a successful revalidation, update certain headers for the cached asset
    /// such as `Etag` with the fresh response header `resp`.
    pub fn revalidate_merge_header(&mut self, resp: &RespHeader) -> ResponseHeader {
        match self.phase {
            CachePhase::Stale => {
                /*
                 * https://datatracker.ietf.org/doc/html/rfc9110#section-15.4.5
                 * 304 response MUST generate ... would have been sent in a 200 ...
                 * - Content-Location, Date, ETag, and Vary
                 * - Cache-Control and Expires...
                 */
                let mut old_header = self.inner().meta.as_ref().unwrap().0.header.clone();
                let mut clone_header = |header_name: &'static str| {
                    // TODO: multiple headers
                    if let Some(value) = resp.headers.get(header_name) {
                        old_header.insert_header(header_name, value).unwrap();
                    }
                };
                clone_header("cache-control");
                clone_header("expires");
                clone_header("cache-tag");
                clone_header("cdn-cache-control");
                clone_header("etag");
                // https://datatracker.ietf.org/doc/html/rfc9111#section-4.3.4
                // "...cache MUST update its header fields with the header fields provided in the 304..."
                // But if the Vary header changes, the cached response may no longer match the
                // incoming request.
                //
                // For simplicity, ignore changing Vary in revalidation for now.
                // TODO: if we support vary during revalidation, there are a few edge cases to
                // consider (what if Vary header appears/disappears/changes)?
                //
                // clone_header("vary");
                old_header
            }
            _ => panic!("wrong phase {:?}", self.phase),
        }
    }

    /// Mark this asset uncacheable after revalidation
    pub fn revalidate_uncacheable(&mut self, header: ResponseHeader, reason: NoCacheReason) {
        match self.phase {
            CachePhase::Stale => {
                // replace cache meta header
                self.inner_mut().meta.as_mut().unwrap().0.header = header;
            }
            _ => panic!("wrong phase {:?}", self.phase),
        }
        self.phase = CachePhase::RevalidatedNoCache(reason);
        // TODO: remove this asset from cache once finished?
    }

    /// Update the variance of the [CacheMeta].
    ///
    /// Note that this process may change the lookup `key`, and eventually (when the asset is
    /// written to storage) invalidate other cached variants under the same primary key as the
    /// current asset.
    pub fn update_variance(&mut self, variance: Option<HashBinary>) {
        // If this is a cache miss, we will simply update the variance in the meta.
        //
        // If this is an expired response, we will have to consider a few cases:
        //
        // **Case 1**: Variance was absent, but caller sets it now.
        // We will just insert it into the meta. The current asset becomes the primary variant.
        // Because the current location of the asset is already the primary variant, nothing else
        // needs to be done.
        //
        // **Case 2**: Variance was present, but it changed or was removed.
        // We want the current asset to take over the primary slot, in order to invalidate all
        // other variants derived under the old Vary.
        //
        // **Case 3**: Variance did not change.
        // Nothing needs to happen.
        let inner = match self.phase {
            CachePhase::Miss | CachePhase::Expired => self.inner_mut(),
            _ => panic!("wrong phase {:?}", self.phase),
        };

        // Update the variance in the meta
        if let Some(variance_hash) = variance.as_ref() {
            inner
                .meta
                .as_mut()
                .unwrap()
                .set_variance_key(*variance_hash);
        } else {
            inner.meta.as_mut().unwrap().remove_variance();
        }

        // Change the lookup `key` if necessary, in order to admit asset into the primary slot
        // instead of the secondary slot.
        let key = inner.key.as_ref().unwrap();
        if let Some(old_variance) = key.get_variance_key().as_ref() {
            // This is a secondary variant slot.
            if Some(*old_variance) != variance.as_ref() {
                // This new variance does not match the variance in the cache key we used to look
                // up this asset.
                // Drop the cache lock to avoid leaving a dangling lock
                // (because we locked with the old cache key for the secondary slot)
                // TODO: maybe we should try to signal waiting readers to compete for the primary key
                // lock instead? we will not be modifying this secondary slot so it's not actually
                // ready for readers
                if let Some(lock) = inner.cache_lock.as_ref() {
                    lock.release(key, LockStatus::Done);
                }
                // Remove the `variance` from the `key`, so that we admit this asset into the
                // primary slot. (`key` is used to tell storage where to write the data.)
                inner.key.as_mut().unwrap().remove_variance_key();
            }
        }
    }

    /// Return the [CacheMeta] of this asset
    ///
    /// # Panic
    /// Panic in phases which has no cache meta.
    pub fn cache_meta(&self) -> &CacheMeta {
        match self.phase {
            // TODO: allow in Bypass phase?
            CachePhase::Stale
            | CachePhase::Expired
            | CachePhase::Hit
            | CachePhase::Revalidated
            | CachePhase::RevalidatedNoCache(_) => self.inner().meta.as_ref().unwrap(),
            CachePhase::Miss => {
                // this is the async body read case, safe because body_reader is only set
                // after meta is retrieved
                if self.inner().body_reader.is_some() {
                    self.inner().meta.as_ref().unwrap()
                } else {
                    panic!("wrong phase {:?}", self.phase);
                }
            }

            _ => panic!("wrong phase {:?}", self.phase),
        }
    }

    /// Return the [CacheMeta] of this asset if any
    ///
    /// Different from [Self::cache_meta()], this function is allowed to be called in
    /// [CachePhase::Miss] phase where the cache meta maybe set.
    /// # Panic
    /// Panic in phases that shouldn't have cache meta.
    pub fn maybe_cache_meta(&self) -> Option<&CacheMeta> {
        match self.phase {
            CachePhase::Miss
            | CachePhase::Stale
            | CachePhase::Expired
            | CachePhase::Hit
            | CachePhase::Revalidated
            | CachePhase::RevalidatedNoCache(_) => self.inner().meta.as_ref(),
            _ => panic!("wrong phase {:?}", self.phase),
        }
    }

    /// Perform the cache lookup from the given cache storage with the given cache key
    ///
    /// A cache hit will return [CacheMeta] which contains the header and meta info about
    /// the cache as well as a [HitHandler] to read the cache hit body.
    /// # Panic
    /// Panic in other phases.
    pub async fn cache_lookup(&mut self) -> Result<Option<(CacheMeta, HitHandler)>> {
        match self.phase {
            // Stale is allowed here because stale-> cache_lock -> lookup again
            CachePhase::CacheKey | CachePhase::Stale => {
                let inner = self.inner_mut();
                let mut span = inner.traces.child("lookup");
                let key = inner.key.as_ref().unwrap(); // safe, this phase should have cache key
                let now = Instant::now();
                let result = inner.storage.lookup(key, &span.handle()).await?;
                let lookup_duration = now.elapsed();
                // one request may have multiple lookups
                inner.lookup_duration = Some(
                    inner
                        .lookup_duration
                        .map_or(lookup_duration, |d| d + lookup_duration),
                );
                let result = result.and_then(|(meta, header)| {
                    if let Some(ts) = inner.valid_after {
                        if meta.created() < ts {
                            span.set_tag(|| trace::Tag::new("not valid", true));
                            return None;
                        }
                    }
                    Some((meta, header))
                });
                if result.is_none() {
                    if let Some(lock) = inner.cache_lock.as_ref() {
                        inner.lock = Some(lock.lock(key));
                    }
                }
                span.set_tag(|| trace::Tag::new("found", result.is_some()));
                Ok(result)
            }
            _ => panic!("wrong phase {:?}", self.phase),
        }
    }

    /// Update variance and see if the meta matches the current variance
    ///
    /// `cache_lookup() -> compute vary hash -> cache_vary_lookup()`
    /// This function allows callers to compute vary based on the initial cache hit.
    /// `meta` should be the ones returned from the initial cache_lookup()
    /// - return true if the meta is the variance.
    /// - return false if the current meta doesn't match the variance, need to cache_lookup() again
    pub fn cache_vary_lookup(&mut self, variance: HashBinary, meta: &CacheMeta) -> bool {
        match self.phase {
            CachePhase::CacheKey => {
                let inner = self.inner_mut();
                // make sure that all variances found are fresher than this asset
                // this is because when purging all the variance, only the primary slot is deleted
                // the created TS of the primary is the tombstone of all the variances
                inner.valid_after = Some(meta.created());

                // update vary
                let key = inner.key.as_mut().unwrap();
                // if no variance was previously set, then this is the first cache hit
                let is_initial_cache_hit = key.get_variance_key().is_none();
                key.set_variance_key(variance);
                let variance_binary = key.variance_bin();
                let matches_variance = meta.variance() == variance_binary;

                // We should remove the variance in the lookup `key` if this is the primary variant
                // slot. We know this is the primary variant slot if this is the initial cache hit,
                // AND the variance in the `key` already matches the `meta`'s.
                //
                // For the primary variant slot, the storage backend needs to use the primary key
                // for both cache lookup and updating the meta. Otherwise it will look for the
                // asset in the wrong location during revalidation.
                //
                // We can recreate the "full" cache key by using the meta's variance, if needed.
                if matches_variance && is_initial_cache_hit {
                    inner.key.as_mut().unwrap().remove_variance_key();
                }

                matches_variance
            }
            _ => panic!("wrong phase {:?}", self.phase),
        }
    }

    /// Whether this request is behind a cache lock in order to wait for another request to read the
    /// asset.
    pub fn is_cache_locked(&self) -> bool {
        matches!(self.inner().lock, Some(Locked::Read(_)))
    }

    /// Whether this request is the leader request to fetch the assets for itself and other requests
    /// behind the cache lock.
    pub fn is_cache_lock_writer(&self) -> bool {
        matches!(self.inner().lock, Some(Locked::Write(_)))
    }

    /// Take the write lock from this request to transfer it to another one.
    /// # Panic
    ///  Call is_cache_lock_writer() to check first, will panic otherwise.
    pub fn take_write_lock(&mut self) -> WritePermit {
        let lock = self.inner_mut().lock.take().unwrap();
        match lock {
            Locked::Write(w) => w,
            Locked::Read(_) => panic!("take_write_lock() called on read lock"),
        }
    }

    /// Set the write lock, which is usually transferred from [Self::take_write_lock()]
    pub fn set_write_lock(&mut self, write_lock: WritePermit) {
        self.inner_mut().lock.replace(Locked::Write(write_lock));
    }

    /// Whether this request's cache hit is staled
    fn has_staled_asset(&self) -> bool {
        self.phase == CachePhase::Stale
    }

    /// Whether this asset is staled and stale if error is allowed
    pub fn can_serve_stale_error(&self) -> bool {
        self.has_staled_asset() && self.cache_meta().serve_stale_if_error(SystemTime::now())
    }

    /// Whether this asset is staled and stale while revalidate is allowed.
    pub fn can_serve_stale_updating(&self) -> bool {
        self.has_staled_asset()
            && self
                .cache_meta()
                .serve_stale_while_revalidate(SystemTime::now())
    }

    /// Wait for the cache read lock to be unlocked
    /// # Panic
    /// Check [Self::is_cache_locked()], panic if this request doesn't have a read lock.
    pub async fn cache_lock_wait(&mut self) -> LockStatus {
        let inner = self.inner_mut();
        let _span = inner.traces.child("cache_lock");
        let lock = inner.lock.take(); // remove the lock from self
        if let Some(Locked::Read(r)) = lock {
            let now = Instant::now();
            r.wait().await;
            let lock_duration = now.elapsed();
            // it's possible for a request to be locked more than once
            inner.lock_duration = Some(
                inner
                    .lock_duration
                    .map_or(lock_duration, |d| d + lock_duration),
            );
            r.lock_status() // TODO: tag the span with lock status
        } else {
            // should always call is_cache_locked() before this function
            panic!("cache_lock_wait on wrong type of lock")
        }
    }

    /// How long did this request wait behind the read lock
    pub fn lock_duration(&self) -> Option<Duration> {
        // FIXME: this duration is lost when cache is disabled
        self.inner.as_ref().and_then(|i| i.lock_duration)
    }

    /// How long did this request spent on cache lookup and reading the header
    pub fn lookup_duration(&self) -> Option<Duration> {
        // FIXME: this duration is lost when cache is disabled
        self.inner.as_ref().and_then(|i| i.lookup_duration)
    }

    /// Delete the asset from the cache storage
    /// # Panic
    /// Need to be called after the cache key is set. Panic otherwise.
    pub async fn purge(&mut self) -> Result<bool> {
        match self.phase {
            CachePhase::CacheKey => {
                let inner = self.inner_mut();
                let mut span = inner.traces.child("purge");
                let key = inner.key.as_ref().unwrap().to_compact();
                let result = inner
                    .storage
                    .purge(&key, PurgeType::Invalidation, &span.handle())
                    .await;
                // FIXME: also need to remove from eviction manager
                span.set_tag(|| trace::Tag::new("purged", matches!(result, Ok(true))));
                result
            }
            _ => panic!("wrong phase {:?}", self.phase),
        }
    }

    /// Check the cacheable prediction
    ///
    /// Return true if the predictor is not set
    pub fn cacheable_prediction(&self) -> bool {
        if let Some(predictor) = self.inner().predictor {
            predictor.cacheable_prediction(self.cache_key())
        } else {
            true
        }
    }

    /// Tell the predictor that this response, which is previously predicted to be uncacheable,
    /// is cacheable now.
    pub fn response_became_cacheable(&self) {
        if let Some(predictor) = self.inner().predictor {
            predictor.mark_cacheable(self.cache_key());
        }
    }

    /// Tell the predictor that this response is uncacheable so that it will know next time
    /// this request arrives.
    pub fn response_became_uncacheable(&self, reason: NoCacheReason) {
        if let Some(predictor) = self.inner().predictor {
            predictor.mark_uncacheable(self.cache_key(), reason);
        }
    }
}

/// Set the header compression dictionary, which helps serialize http header.
///
/// Return false if it is already set.
pub fn set_compression_dict_path(path: &str) -> bool {
    crate::meta::COMPRESSION_DICT_PATH
        .set(path.to_string())
        .is_ok()
}
