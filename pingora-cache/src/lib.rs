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

//! The HTTP caching layer for proxies.

#![allow(clippy::new_without_default)]

use cf_rustracing::tag::Tag;
use http::{method::Method, request::Parts as ReqHeader, response::Parts as RespHeader};
use key::{CacheHashKey, CompactCacheKey, HashBinary};
use lock::WritePermit;
use log::warn;
use pingora_error::Result;
use pingora_http::ResponseHeader;
use pingora_timeout::timeout;
use std::time::{Duration, Instant, SystemTime};
use storage::MissFinishType;
use strum::IntoStaticStr;
use trace::{CacheTraceCTX, Span};

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

use crate::max_file_size::MaxFileSizeTracker;
pub use key::CacheKey;
use lock::{CacheKeyLockImpl, LockStatus, Locked};
pub use memory::MemCache;
pub use meta::{set_compression_dict_content, set_compression_dict_path};
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
    digest: HttpCacheDigest,
}

/// This reflects the phase of HttpCache during the lifetime of a request
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CachePhase {
    /// Cache disabled, with reason (NeverEnabled if never explicitly used)
    Disabled(NoCacheReason),
    /// Cache enabled but nothing is set yet
    Uninit,
    /// Cache was enabled, the request decided not to use it
    // HttpCache.inner_enabled is kept
    Bypass,
    /// Awaiting the cache key to be generated
    CacheKey,
    /// Cache hit
    Hit,
    /// No cached asset is found
    Miss,
    /// A staled (expired) asset is found
    Stale,
    /// A staled (expired) asset was found, but another request is revalidating it
    StaleUpdating,
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
            CachePhase::StaleUpdating => "stale-updating",
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
    /// Disabling caching due to unknown body size and previously exceeding maximum asset size;
    /// the asset is otherwise cacheable, but cache needs to confirm the final size of the asset
    /// before it can mark it as cacheable again.
    PredictedResponseTooLarge,
    /// Due to internal caching storage error
    StorageError,
    /// Due to other types of internal issues
    InternalError,
    /// will be cacheable but skip cache admission now
    ///
    /// This happens when the cache predictor predicted that this request is not cacheable, but
    /// the response turns out to be OK to cache. However, it might be too large to re-enable caching
    /// for this request
    Deferred,
    /// Due to the proxy upstream filter declining the current request from going upstream
    DeclinedToUpstream,
    /// Due to the upstream being unreachable or otherwise erroring during proxying
    UpstreamError,
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
            PredictedResponseTooLarge => "PredictedResponseTooLarge",
            StorageError => "StorageError",
            InternalError => "InternalError",
            Deferred => "Deferred",
            DeclinedToUpstream => "DeclinedToUpstream",
            UpstreamError => "UpstreamError",
            CacheLockGiveUp => "CacheLockGiveUp",
            CacheLockTimeout => "CacheLockTimeout",
            Custom(s) => s,
        }
    }
}

/// Information collected about the caching operation that will not be cleared
#[derive(Debug, Default)]
pub struct HttpCacheDigest {
    pub lock_duration: Option<Duration>,
    // time spent in cache lookup and reading the header
    pub lookup_duration: Option<Duration>,
}

/// Convenience function to add a duration to an optional duration
fn add_duration_to_opt(target_opt: &mut Option<Duration>, to_add: Duration) {
    *target_opt = Some(target_opt.map_or(to_add, |existing| existing + to_add));
}

impl HttpCacheDigest {
    fn add_lookup_duration(&mut self, extra_lookup_duration: Duration) {
        add_duration_to_opt(&mut self.lookup_duration, extra_lookup_duration)
    }

    fn add_lock_duration(&mut self, extra_lock_duration: Duration) {
        add_duration_to_opt(&mut self.lock_duration, extra_lock_duration)
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

/// Indicators of which level of purge logic to apply to an asset. As in should
/// the purged file be revalidated or re-retrieved altogether
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForcedInvalidationKind {
    /// Indicates the asset should be considered stale and revalidated
    ForceExpired,

    /// Indicates the asset should be considered absent and treated like a miss
    /// instead of a hit
    ForceMiss,
}

/// Freshness state of cache hit asset
///
///
#[derive(Debug, Copy, Clone, IntoStaticStr, PartialEq, Eq)]
#[strum(serialize_all = "snake_case")]
pub enum HitStatus {
    /// The asset's freshness directives indicate it has expired
    Expired,

    /// The asset was marked as expired, and should be treated as stale
    ForceExpired,

    /// The asset was marked as absent, and should be treated as a miss
    ForceMiss,

    /// An error occurred while processing the asset, so it should be treated as
    /// a miss
    FailedHitFilter,

    /// The asset is not expired
    Fresh,
}

impl HitStatus {
    /// For displaying cache hit status
    pub fn as_str(&self) -> &'static str {
        self.into()
    }

    /// Whether cached asset can be served as fresh
    pub fn is_fresh(&self) -> bool {
        *self == HitStatus::Fresh
    }

    /// Check whether the hit status should be treated as a miss. A forced miss
    /// is obviously treated as a miss. A hit-filter failure is treated as a
    /// miss because we can't use the asset as an actual hit. If we treat it as
    /// expired, we still might not be able to use it even if revalidation
    /// succeeds.
    pub fn is_treated_as_miss(self) -> bool {
        matches!(self, HitStatus::ForceMiss | HitStatus::FailedHitFilter)
    }
}

pub struct LockCtx {
    pub lock: Option<Locked>,
    pub cache_lock: &'static CacheKeyLockImpl,
    pub wait_timeout: Option<Duration>,
}

// Fields like storage handlers that are needed only when cache is enabled (or bypassing).
struct HttpCacheInnerEnabled {
    pub meta: Option<CacheMeta>,
    // when set, even if an asset exists, it would only be considered valid after this timestamp
    pub valid_after: Option<SystemTime>,
    pub miss_handler: Option<MissHandler>,
    pub body_reader: Option<HitHandler>,
    pub storage: &'static (dyn storage::Storage + Sync), // static for now
    pub eviction: Option<&'static (dyn eviction::EvictionManager + Sync)>,
    pub lock_ctx: Option<LockCtx>,
    pub traces: trace::CacheTraceCTX,
}

struct HttpCacheInner {
    // Prefer adding fields to InnerEnabled if possible, these fields are released
    // when cache is disabled.
    // If fields are needed after cache disablement, add directly to Inner.
    pub enabled_ctx: Option<Box<HttpCacheInnerEnabled>>,
    pub key: Option<CacheKey>,
    // when set, an asset will be rejected from the cache if it exceeds configured size in bytes
    pub max_file_size_tracker: Option<MaxFileSizeTracker>,
    pub predictor: Option<&'static (dyn predictor::CacheablePredictor + Sync)>,
}

#[derive(Debug, Default)]
#[non_exhaustive]
pub struct CacheOptionOverrides {
    pub wait_timeout: Option<Duration>,
}

impl HttpCache {
    /// Create a new [HttpCache].
    ///
    /// Caching is not enabled by default.
    pub fn new() -> Self {
        HttpCache {
            phase: CachePhase::Disabled(NoCacheReason::NeverEnabled),
            inner: None,
            digest: HttpCacheDigest::default(),
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
            Hit | Stale | StaleUpdating => false,
            Uninit | CacheKey => false, // invalid states for this call, treat them as false to keep it simple
        }
    }

    /// Check whether the backend storage is the type `T`.
    pub fn storage_type_is<T: 'static>(&self) -> bool {
        self.inner
            .as_ref()
            .and_then(|inner| {
                inner
                    .enabled_ctx
                    .as_ref()
                    .and_then(|ie| ie.storage.as_any().downcast_ref::<T>())
            })
            .is_some()
    }

    /// Release the cache lock if the current request is a cache writer.
    ///
    /// Generally callers should prefer using `disable` when a cache lock should be released
    /// due to an error to clear all cache context. This function is for releasing the cache lock
    /// while still keeping the cache around for reading, e.g. when serving stale.
    pub fn release_write_lock(&mut self, reason: NoCacheReason) {
        use NoCacheReason::*;
        if let Some(inner) = self.inner.as_mut() {
            if let Some(lock_ctx) = inner
                .enabled_ctx
                .as_mut()
                .and_then(|ie| ie.lock_ctx.as_mut())
            {
                let lock = lock_ctx.lock.take();
                if let Some(Locked::Write(permit)) = lock {
                    let lock_status = match reason {
                        // let the next request try to fetch it
                        InternalError | StorageError | Deferred | UpstreamError => {
                            LockStatus::TransientError
                        }
                        // depends on why the proxy upstream filter declined the request,
                        // for now still allow next request try to acquire to avoid thundering herd
                        DeclinedToUpstream => LockStatus::TransientError,
                        // no need for the lock anymore
                        OriginNotCache | ResponseTooLarge | PredictedResponseTooLarge => {
                            LockStatus::GiveUp
                        }
                        // not sure which LockStatus make sense, we treat it as GiveUp for now
                        Custom(_) => LockStatus::GiveUp,
                        // should never happen, NeverEnabled shouldn't hold a lock
                        NeverEnabled => panic!("NeverEnabled holds a write lock"),
                        CacheLockGiveUp | CacheLockTimeout => {
                            panic!("CacheLock* are for cache lock readers only")
                        }
                    };
                    lock_ctx
                        .cache_lock
                        .release(inner.key.as_ref().unwrap(), permit, lock_status);
                }
            }
        }
    }

    /// Disable caching
    pub fn disable(&mut self, reason: NoCacheReason) {
        // XXX: compile type enforce?
        assert!(
            reason != NoCacheReason::NeverEnabled,
            "NeverEnabled not allowed as a disable reason"
        );
        match self.phase {
            CachePhase::Disabled(old_reason) => {
                // replace reason
                if old_reason == NoCacheReason::NeverEnabled {
                    // safeguard, don't allow replacing NeverEnabled as a reason
                    // TODO: can be promoted to assertion once confirmed nothing is attempting this
                    warn!("Tried to replace cache NeverEnabled with reason: {reason:?}");
                    return;
                }
                self.phase = CachePhase::Disabled(reason);
            }
            _ => {
                self.phase = CachePhase::Disabled(reason);
                self.release_write_lock(reason);
                // enabled_ctx will be cleared out
                let mut inner_enabled = self
                    .inner_mut()
                    .enabled_ctx
                    .take()
                    .expect("could remove enabled_ctx on disable");
                // log initial disable reason
                inner_enabled
                    .traces
                    .cache_span
                    .set_tag(|| trace::Tag::new("disable_reason", reason.as_str()));
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
                self.inner_enabled_mut()
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
        cache_lock: Option<&'static CacheKeyLockImpl>,
        option_overrides: Option<CacheOptionOverrides>,
    ) {
        match self.phase {
            CachePhase::Disabled(_) => {
                self.phase = CachePhase::Uninit;

                let lock_ctx = cache_lock.map(|cache_lock| LockCtx {
                    cache_lock,
                    lock: None,
                    wait_timeout: option_overrides
                        .as_ref()
                        .and_then(|overrides| overrides.wait_timeout),
                });

                self.inner = Some(Box::new(HttpCacheInner {
                    enabled_ctx: Some(Box::new(HttpCacheInnerEnabled {
                        meta: None,
                        valid_after: None,
                        miss_handler: None,
                        body_reader: None,
                        storage,
                        eviction,
                        lock_ctx,
                        traces: CacheTraceCTX::new(),
                    })),
                    key: None,
                    max_file_size_tracker: None,
                    predictor,
                }));
            }
            _ => panic!("Cannot enable already enabled HttpCache {:?}", self.phase),
        }
    }

    /// Set the cache lock implementation.
    /// # Panic
    /// Must be called before a cache lock is attempted to be acquired,
    /// i.e. in the `cache_key_callback` or `cache_hit_filter` phases.
    pub fn set_cache_lock(
        &mut self,
        cache_lock: Option<&'static CacheKeyLockImpl>,
        option_overrides: Option<CacheOptionOverrides>,
    ) {
        match self.phase {
            CachePhase::Disabled(_)
            | CachePhase::CacheKey
            | CachePhase::Stale
            | CachePhase::Hit => {
                let inner_enabled = self.inner_enabled_mut();
                if inner_enabled
                    .lock_ctx
                    .as_ref()
                    .is_some_and(|ctx| ctx.lock.is_some())
                {
                    panic!("lock already set when resetting cache lock")
                } else {
                    let lock_ctx = cache_lock.map(|cache_lock| LockCtx {
                        cache_lock,
                        lock: None,
                        wait_timeout: option_overrides.and_then(|overrides| overrides.wait_timeout),
                    });
                    inner_enabled.lock_ctx = lock_ctx;
                }
            }
            _ => panic!("wrong phase: {:?}", self.phase),
        }
    }

    // Enable distributed tracing
    pub fn enable_tracing(&mut self, parent_span: trace::Span) {
        if let Some(inner_enabled) = self.inner.as_mut().and_then(|i| i.enabled_ctx.as_mut()) {
            inner_enabled.traces.enable(parent_span);
        }
    }

    // Get the cache parent tracing span
    pub fn get_cache_span(&self) -> Option<trace::SpanHandle> {
        self.inner
            .as_ref()
            .and_then(|i| i.enabled_ctx.as_ref().map(|ie| ie.traces.get_cache_span()))
    }

    // Get the cache `miss` tracing span
    pub fn get_miss_span(&self) -> Option<trace::SpanHandle> {
        self.inner
            .as_ref()
            .and_then(|i| i.enabled_ctx.as_ref().map(|ie| ie.traces.get_miss_span()))
    }

    // Get the cache `hit` tracing span
    pub fn get_hit_span(&self) -> Option<trace::SpanHandle> {
        self.inner
            .as_ref()
            .and_then(|i| i.enabled_ctx.as_ref().map(|ie| ie.traces.get_hit_span()))
    }

    // shortcut to access inner fields, panic if phase is disabled
    #[inline]
    fn inner_enabled_mut(&mut self) -> &mut HttpCacheInnerEnabled {
        self.inner.as_mut().unwrap().enabled_ctx.as_mut().unwrap()
    }

    #[inline]
    fn inner_enabled(&self) -> &HttpCacheInnerEnabled {
        self.inner.as_ref().unwrap().enabled_ctx.as_ref().unwrap()
    }

    // shortcut to access inner fields, panic if cache was never enabled
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
            CachePhase::Disabled(NoCacheReason::NeverEnabled) | CachePhase::Uninit => {
                panic!("wrong phase {:?}", self.phase)
            }
            _ => self
                .inner()
                .key
                .as_ref()
                .expect("cache key should be set (set_cache_key not called?)"),
        }
    }

    /// Return the max size allowed to be cached.
    pub fn max_file_size_bytes(&self) -> Option<usize> {
        assert!(
            !matches!(
                self.phase,
                CachePhase::Disabled(NoCacheReason::NeverEnabled)
            ),
            "tried to access max file size bytes when cache never enabled"
        );
        self.inner()
            .max_file_size_tracker
            .as_ref()
            .map(|t| t.max_file_size_bytes())
    }

    /// Set the maximum response _body_ size in bytes that will be admitted to the cache.
    ///
    /// Response header size should not contribute to the max file size.
    ///
    /// To track body bytes, call `track_bytes_for_max_file_size`.
    pub fn set_max_file_size_bytes(&mut self, max_file_size_bytes: usize) {
        match self.phase {
            CachePhase::Disabled(_) => panic!("wrong phase {:?}", self.phase),
            _ => {
                self.inner_mut().max_file_size_tracker =
                    Some(MaxFileSizeTracker::new(max_file_size_bytes));
            }
        }
    }

    /// Record body bytes for the max file size tracker.
    ///
    /// The `bytes_len` input contributes to a cumulative body byte tracker.
    ///
    /// Once the cumulative body bytes exceeds the maximum allowable cache file size (as configured
    /// by `set_max_file_size_bytes`), then the return value will be false.
    ///
    /// Else the return value is true as long as the max file size is not exceeded.
    /// If max file size was not configured, the return value is always true.
    pub fn track_body_bytes_for_max_file_size(&mut self, bytes_len: usize) -> bool {
        // This is intended to be callable when cache has already been disabled,
        // so that we can re-mark an asset as cacheable if the body size is under limits.
        assert!(
            !matches!(
                self.phase,
                CachePhase::Disabled(NoCacheReason::NeverEnabled)
            ),
            "tried to access max file size bytes when cache never enabled"
        );
        self.inner_mut()
            .max_file_size_tracker
            .as_mut()
            .map_or(true, |t| t.add_body_bytes(bytes_len))
    }

    /// Check if the max file size has been exceeded according to max file size tracker.
    ///
    /// Return true if max file size was exceeded.
    pub fn exceeded_max_file_size(&self) -> bool {
        assert!(
            !matches!(
                self.phase,
                CachePhase::Disabled(NoCacheReason::NeverEnabled)
            ),
            "tried to access max file size bytes when cache never enabled"
        );
        self.inner()
            .max_file_size_tracker
            .as_ref()
            .is_some_and(|t| !t.allow_caching())
    }

    /// Set that cache is found in cache storage.
    ///
    /// This function is called after [Self::cache_lookup()] which returns the [CacheMeta] and
    /// [HitHandler].
    ///
    /// The `hit_status` enum allows the caller to force expire assets.
    pub fn cache_found(&mut self, meta: CacheMeta, hit_handler: HitHandler, hit_status: HitStatus) {
        // Stale allowed because of cache lock and then retry
        if !matches!(self.phase, CachePhase::CacheKey | CachePhase::Stale) {
            panic!("wrong phase {:?}", self.phase)
        }

        self.phase = match hit_status {
            HitStatus::Fresh => CachePhase::Hit,
            HitStatus::Expired | HitStatus::ForceExpired => CachePhase::Stale,
            HitStatus::FailedHitFilter | HitStatus::ForceMiss => self.phase,
        };

        let phase = self.phase;
        let inner = self.inner_mut();

        let key = inner.key.as_ref().expect("key must be set on hit");
        let inner_enabled = inner
            .enabled_ctx
            .as_mut()
            .expect("cache_found must be called while cache enabled");

        // The cache lock might not be set for stale hit or hits treated as
        // misses, so we need to initialize it here
        if phase == CachePhase::Stale || hit_status.is_treated_as_miss() {
            if let Some(lock_ctx) = inner_enabled.lock_ctx.as_mut() {
                lock_ctx.lock = Some(lock_ctx.cache_lock.lock(key));
            }
        }

        if hit_status.is_treated_as_miss() {
            // Clear the body and meta for hits that are treated as misses
            inner_enabled.body_reader = None;
            inner_enabled.meta = None;
        } else {
            // Set the metadata appropriately for legit hits
            inner_enabled.traces.start_hit_span(phase, hit_status);
            inner_enabled.traces.log_meta_in_hit_span(&meta);
            if let Some(eviction) = inner_enabled.eviction {
                // TODO: make access() accept CacheKey
                let cache_key = key.to_compact();
                if hit_handler.should_count_access() {
                    let size = hit_handler.get_eviction_weight();
                    eviction.access(&cache_key, size, meta.0.internal.fresh_until);
                }
            }
            inner_enabled.meta = Some(meta);
            inner_enabled.body_reader = Some(hit_handler);
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
            // from Stale: waited for cache lock, then retried and found asset was gone
            CachePhase::CacheKey | CachePhase::Bypass | CachePhase::Stale => {
                self.phase = CachePhase::Miss;
                // It's possible that we've set the meta on lookup and have come back around
                // here after not being able to acquire the cache lock, and our item has since
                // purged or expired. We should be sure that the meta is not set in this case
                // as there shouldn't be a meta set for cache misses.
                self.inner_enabled_mut().meta = None;
                self.inner_enabled_mut().traces.start_miss_span();
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
            | CachePhase::StaleUpdating
            | CachePhase::Revalidated
            | CachePhase::RevalidatedNoCache(_) => {
                self.inner_enabled_mut().body_reader.as_mut().unwrap()
            }
            _ => panic!("wrong phase {:?}", self.phase),
        }
    }

    /// Return the body reader during a cache admission (miss/expired) which decouples the downstream
    /// read and upstream cache write
    pub fn miss_body_reader(&mut self) -> Option<&mut HitHandler> {
        match self.phase {
            CachePhase::Miss | CachePhase::Expired => {
                let inner_enabled = self.inner_enabled_mut();
                if inner_enabled.storage.support_streaming_partial_write() {
                    inner_enabled.body_reader.as_mut()
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
            | CachePhase::StaleUpdating
            | CachePhase::Revalidated
            | CachePhase::RevalidatedNoCache(_) => {
                let inner = self.inner_mut();
                let inner_enabled = inner.enabled_ctx.as_mut().expect("cache enabled");
                if inner_enabled.body_reader.is_none() {
                    // already finished, we allow calling this function more than once
                    return Ok(());
                }
                let body_reader = inner_enabled.body_reader.take().unwrap();
                let key = inner.key.as_ref().unwrap();
                let result = body_reader
                    .finish(
                        inner_enabled.storage,
                        key,
                        &inner_enabled.traces.hit_span.handle(),
                    )
                    .await;
                inner_enabled.traces.finish_hit_span();
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
                let inner = self.inner_mut();
                let inner_enabled = inner
                    .enabled_ctx
                    .as_mut()
                    .expect("cache enabled on miss and expired");
                if inner_enabled.miss_handler.is_some() {
                    panic!("write handler is already set")
                }
                let meta = inner_enabled.meta.as_ref().unwrap();
                let key = inner.key.as_ref().unwrap();
                let miss_handler = inner_enabled
                    .storage
                    .get_miss_handler(key, meta, &inner_enabled.traces.get_miss_span())
                    .await?;

                inner_enabled.miss_handler = Some(miss_handler);

                if inner_enabled.storage.support_streaming_partial_write() {
                    // If a reader can access partial write, the cache lock can be released here
                    // to let readers start reading the body.
                    if let Some(lock_ctx) = inner_enabled.lock_ctx.as_mut() {
                        let lock = lock_ctx.lock.take();
                        if let Some(Locked::Write(permit)) = lock {
                            lock_ctx.cache_lock.release(key, permit, LockStatus::Done);
                        }
                    }
                    // Downstream read and upstream write can be decoupled
                    let body_reader = inner_enabled
                        .storage
                        .lookup_streaming_write(
                            key,
                            inner_enabled
                                .miss_handler
                                .as_ref()
                                .expect("miss handler already set")
                                .streaming_write_tag(),
                            &inner_enabled.traces.get_miss_span(),
                        )
                        .await?;

                    if let Some((_meta, body_reader)) = body_reader {
                        inner_enabled.body_reader = Some(body_reader);
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
            CachePhase::Miss | CachePhase::Expired => {
                self.inner_enabled_mut().miss_handler.as_mut()
            }
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
                let inner_enabled = inner
                    .enabled_ctx
                    .as_mut()
                    .expect("cache enabled on miss and expired");
                if inner_enabled.miss_handler.is_none() {
                    // already finished, we allow calling this function more than once
                    return Ok(());
                }
                let miss_handler = inner_enabled.miss_handler.take().unwrap();
                let size = miss_handler.finish().await?;
                let key = inner
                    .key
                    .as_ref()
                    .expect("key set by miss or expired phase");
                if let Some(lock_ctx) = inner_enabled.lock_ctx.as_mut() {
                    let lock = lock_ctx.lock.take();
                    if let Some(Locked::Write(permit)) = lock {
                        // no need to call r.unlock() because release() will call it
                        // r is a guard to make sure the lock is unlocked when this request is dropped
                        lock_ctx.cache_lock.release(key, permit, LockStatus::Done);
                    }
                }
                if let Some(eviction) = inner_enabled.eviction {
                    let cache_key = key.to_compact();
                    let meta = inner_enabled.meta.as_ref().unwrap();
                    let evicted = match size {
                        MissFinishType::Created(size) => {
                            eviction.admit(cache_key, size, meta.0.internal.fresh_until)
                        }
                        MissFinishType::Appended(size) => {
                            eviction.increment_weight(cache_key, size)
                        }
                    };
                    // actual eviction can be done async
                    let span = inner_enabled.traces.child("eviction");
                    let handle = span.handle();
                    let storage = inner_enabled.storage;
                    tokio::task::spawn(async move {
                        for item in evicted {
                            if let Err(e) = storage.purge(&item, PurgeType::Eviction, &handle).await
                            {
                                warn!("Failed to purge {item} during eviction for finish miss handler: {e}");
                            }
                        }
                    });
                }
                inner_enabled.traces.finish_miss_span();
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
                let inner_enabled = self.inner_enabled_mut();
                // TODO: have a separate expired span?
                inner_enabled.traces.log_meta_in_miss_span(&meta);
                inner_enabled.meta = Some(meta);
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
                let inner_enabled = inner
                    .enabled_ctx
                    .as_mut()
                    .expect("stale phase has cache enabled");
                // TODO: we should keep old meta in place, just use new one to update it
                // that requires cacheable_filter to take a mut header and just return InternalMeta

                // update new meta with old meta's created time
                let old_meta = inner_enabled.meta.take().unwrap();
                let created = old_meta.0.internal.created;
                meta.0.internal.created = created;
                // meta.internal.updated was already set to new meta's `created`,
                // no need to set `updated` here
                // Merge old extensions with new ones. New exts take precedence if they conflict.
                let mut extensions = old_meta.0.extensions;
                extensions.extend(meta.0.extensions);
                meta.0.extensions = extensions;

                inner_enabled.meta.replace(meta);

                if let Some(lock_ctx) = inner_enabled.lock_ctx.as_mut() {
                    let lock = lock_ctx.lock.take();
                    if let Some(Locked::Write(permit)) = lock {
                        lock_ctx.cache_lock.release(
                            inner.key.as_ref().expect("key set by stale phase"),
                            permit,
                            LockStatus::Done,
                        );
                    }
                }

                let mut span = inner_enabled.traces.child("update_meta");
                // TODO: this call can be async
                let result = inner_enabled
                    .storage
                    .update_meta(
                        inner.key.as_ref().unwrap(),
                        inner_enabled.meta.as_ref().unwrap(),
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
                let mut old_header = self.inner_enabled().meta.as_ref().unwrap().0.header.clone();
                let mut clone_header = |header_name: &'static str| {
                    for (i, value) in resp.headers.get_all(header_name).iter().enumerate() {
                        if i == 0 {
                            old_header
                                .insert_header(header_name, value)
                                .expect("can add valid header");
                        } else {
                            old_header
                                .append_header(header_name, value)
                                .expect("can add valid header");
                        }
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
                self.inner_enabled_mut().meta.as_mut().unwrap().0.header = header;
                // upstream request done, release write lock
                self.release_write_lock(reason);
            }
            _ => panic!("wrong phase {:?}", self.phase),
        }
        self.phase = CachePhase::RevalidatedNoCache(reason);
        // TODO: remove this asset from cache once finished?
    }

    /// Mark this asset as stale, but being updated separately from this request.
    pub fn set_stale_updating(&mut self) {
        match self.phase {
            CachePhase::Stale => self.phase = CachePhase::StaleUpdating,
            _ => panic!("wrong phase {:?}", self.phase),
        }
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
        let inner_enabled = inner
            .enabled_ctx
            .as_mut()
            .expect("cache enabled on miss and expired");

        // Update the variance in the meta
        if let Some(variance_hash) = variance.as_ref() {
            inner_enabled
                .meta
                .as_mut()
                .unwrap()
                .set_variance_key(*variance_hash);
        } else {
            inner_enabled.meta.as_mut().unwrap().remove_variance();
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
                if let Some(lock_ctx) = inner_enabled.lock_ctx.as_mut() {
                    if let Some(Locked::Write(permit)) = lock_ctx.lock.take() {
                        lock_ctx.cache_lock.release(key, permit, LockStatus::Done);
                    }
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
            | CachePhase::StaleUpdating
            | CachePhase::Expired
            | CachePhase::Hit
            | CachePhase::Revalidated
            | CachePhase::RevalidatedNoCache(_) => self.inner_enabled().meta.as_ref().unwrap(),
            CachePhase::Miss => {
                // this is the async body read case, safe because body_reader is only set
                // after meta is retrieved
                if self.inner_enabled().body_reader.is_some() {
                    self.inner_enabled().meta.as_ref().unwrap()
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
            | CachePhase::StaleUpdating
            | CachePhase::Expired
            | CachePhase::Hit
            | CachePhase::Revalidated
            | CachePhase::RevalidatedNoCache(_) => self.inner_enabled().meta.as_ref(),
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
                let inner = self
                    .inner
                    .as_mut()
                    .expect("Cache phase is checked and should have inner");
                let inner_enabled = inner
                    .enabled_ctx
                    .as_mut()
                    .expect("Cache enabled on cache_lookup");
                let mut span = inner_enabled.traces.child("lookup");
                let key = inner.key.as_ref().unwrap(); // safe, this phase should have cache key
                let now = Instant::now();
                let result = inner_enabled.storage.lookup(key, &span.handle()).await?;
                // one request may have multiple lookups
                self.digest.add_lookup_duration(now.elapsed());
                let result = result.and_then(|(meta, header)| {
                    if let Some(ts) = inner_enabled.valid_after {
                        if meta.created() < ts {
                            span.set_tag(|| trace::Tag::new("not valid", true));
                            return None;
                        }
                    }
                    Some((meta, header))
                });
                if result.is_none() {
                    if let Some(lock_ctx) = inner_enabled.lock_ctx.as_mut() {
                        lock_ctx.lock = Some(lock_ctx.cache_lock.lock(key));
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
            // Stale is allowed here because stale-> cache_lock -> lookup again
            CachePhase::CacheKey | CachePhase::Stale => {
                let inner = self.inner_mut();
                // make sure that all variances found are fresher than this asset
                // this is because when purging all the variance, only the primary slot is deleted
                // the created TS of the primary is the tombstone of all the variances
                inner
                    .enabled_ctx
                    .as_mut()
                    .expect("cache enabled")
                    .valid_after = Some(meta.created());

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
        matches!(
            self.inner_enabled()
                .lock_ctx
                .as_ref()
                .and_then(|l| l.lock.as_ref()),
            Some(Locked::Read(_))
        )
    }

    /// Whether this request is the leader request to fetch the assets for itself and other requests
    /// behind the cache lock.
    pub fn is_cache_lock_writer(&self) -> bool {
        matches!(
            self.inner_enabled()
                .lock_ctx
                .as_ref()
                .and_then(|l| l.lock.as_ref()),
            Some(Locked::Write(_))
        )
    }

    /// Take the write lock from this request to transfer it to another one.
    /// # Panic
    ///  Call is_cache_lock_writer() to check first, will panic otherwise.
    pub fn take_write_lock(&mut self) -> (WritePermit, &'static CacheKeyLockImpl) {
        let lock_ctx = self
            .inner_enabled_mut()
            .lock_ctx
            .as_mut()
            .expect("take_write_lock() called without cache lock");
        let lock = lock_ctx
            .lock
            .take()
            .expect("take_write_lock() called without lock");
        match lock {
            Locked::Write(w) => (w, lock_ctx.cache_lock),
            Locked::Read(_) => panic!("take_write_lock() called on read lock"),
        }
    }

    /// Set the write lock, which is usually transferred from [Self::take_write_lock()]
    ///
    /// # Panic
    /// Panics if cache lock was not originally configured for this request.
    // TODO: it may make sense to allow configuring the CacheKeyLock here too that the write permit
    // is associated with
    // (The WritePermit comes from the CacheKeyLock and should be used when releasing from the CacheKeyLock,
    // shouldn't be possible to give a WritePermit to a request using a different CacheKeyLock)
    pub fn set_write_lock(&mut self, write_lock: WritePermit) {
        if let Some(lock_ctx) = self.inner_enabled_mut().lock_ctx.as_mut() {
            lock_ctx.lock.replace(Locked::Write(write_lock));
        }
    }

    /// Whether this request's cache hit is staled
    fn has_staled_asset(&self) -> bool {
        matches!(self.phase, CachePhase::Stale | CachePhase::StaleUpdating)
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
        let inner_enabled = self.inner_enabled_mut();
        let mut span = inner_enabled.traces.child("cache_lock");
        // should always call is_cache_locked() before this function, which should guarantee that
        // the inner cache has a read lock and lock ctx
        if let Some(lock_ctx) = inner_enabled.lock_ctx.as_mut() {
            let lock = lock_ctx.lock.take(); // remove the lock from self
            if let Some(Locked::Read(r)) = lock {
                let now = Instant::now();
                // it's possible for a request to be locked more than once,
                // so wait the remainder of our configured timeout
                let status = if let Some(wait_timeout) = lock_ctx.wait_timeout {
                    let wait_timeout =
                        wait_timeout.saturating_sub(self.lock_duration().unwrap_or(Duration::ZERO));
                    match timeout(wait_timeout, r.wait()).await {
                        Ok(()) => r.lock_status(),
                        // TODO: need to differentiate WaitTimeout vs. Lock(Age)Timeout (expired)?
                        Err(_) => LockStatus::Timeout,
                    }
                } else {
                    r.wait().await;
                    r.lock_status()
                };
                self.digest.add_lock_duration(now.elapsed());
                let tag_value: &'static str = status.into();
                span.set_tag(|| Tag::new("status", tag_value));
                status
            } else {
                panic!("cache_lock_wait on wrong type of lock")
            }
        } else {
            panic!("cache_lock_wait without cache lock")
        }
    }

    /// How long did this request wait behind the read lock
    pub fn lock_duration(&self) -> Option<Duration> {
        self.digest.lock_duration
    }

    /// How long did this request spent on cache lookup and reading the header
    pub fn lookup_duration(&self) -> Option<Duration> {
        self.digest.lookup_duration
    }

    /// Delete the asset from the cache storage
    /// # Panic
    /// Need to be called after the cache key is set. Panic otherwise.
    pub async fn purge(&self) -> Result<bool> {
        match self.phase {
            CachePhase::CacheKey => {
                let inner = self.inner();
                let inner_enabled = self.inner_enabled();
                let span = inner_enabled.traces.child("purge");
                let key = inner.key.as_ref().unwrap().to_compact();
                Self::purge_impl(inner_enabled.storage, inner_enabled.eviction, &key, span).await
            }
            _ => panic!("wrong phase {:?}", self.phase),
        }
    }

    /// Delete the asset from the cache storage via a spawned task.
    /// Returns corresponding `JoinHandle` of that task.
    /// # Panic
    /// Need to be called after the cache key is set. Panic otherwise.
    pub fn spawn_async_purge(
        &self,
        context: &'static str,
    ) -> tokio::task::JoinHandle<Result<bool>> {
        if matches!(self.phase, CachePhase::Disabled(_) | CachePhase::Uninit) {
            panic!("wrong phase {:?}", self.phase);
        }

        let inner_enabled = self.inner_enabled();
        let span = inner_enabled.traces.child("purge");
        let key = self.inner().key.as_ref().unwrap().to_compact();
        let storage = inner_enabled.storage;
        let eviction = inner_enabled.eviction;
        tokio::task::spawn(async move {
            Self::purge_impl(storage, eviction, &key, span)
                .await
                .map_err(|e| {
                    warn!("Failed to purge {key} (context: {context}): {e}");
                    e
                })
        })
    }

    async fn purge_impl(
        storage: &'static (dyn storage::Storage + Sync),
        eviction: Option<&'static (dyn eviction::EvictionManager + Sync)>,
        key: &CompactCacheKey,
        mut span: Span,
    ) -> Result<bool> {
        let result = storage
            .purge(key, PurgeType::Invalidation, &span.handle())
            .await;
        let purged = matches!(result, Ok(true));
        // need to inform eviction manager if asset was removed
        if let Some(eviction) = eviction.as_ref() {
            if purged {
                eviction.remove(key);
            }
        }
        span.set_tag(|| trace::Tag::new("purged", purged));
        result
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

    /// Tag all spans as being part of a subrequest.
    pub fn tag_as_subrequest(&mut self) {
        self.inner_enabled_mut()
            .traces
            .cache_span
            .set_tag(|| Tag::new("is_subrequest", true))
    }
}
