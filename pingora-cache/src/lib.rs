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

//! The HTTP caching layer for proxies.

#![allow(clippy::new_without_default)]

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
use trace::{CacheTraceCTX, Span, Tag};

pub mod admission;
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
use admission::{AdmissionPolicy, Decision};
pub use eviction::{CacheEntryId, CacheEntryKey, CacheEntryKeyRef};
pub use key::CacheKey;
use lock::{CacheKeyLockImpl, LockStatus, LockWaitOutcome, Locked, UnusableFills, WaitOutcome};
pub use memory::MemCache;
pub use meta::{set_compression_dict_content, set_compression_dict_path};
pub use meta::{CacheMeta, CacheMetaDefaults};
pub use storage::{HitHandler, MissHandler, PurgeOutcome, PurgeTarget, PurgeType, Storage};
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
    /// The response may be cacheable, but this request should not fill the cache.
    ///
    /// This can happen when an admission policy defers an absent key, or when the cache predictor
    /// bypassed lookup and the response cannot safely be admitted by the current request.
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
    /// This request retried cache lookup too many times after waiting behind cache locks, so this
    /// request will fetch from the origin without caching.
    CacheLockRetryLimit,
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
            CacheLockRetryLimit => "CacheLockRetryLimit",
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
    /// Admission decision made for an absent key, if an admission policy was configured.
    pub admission: Option<Decision>,
    /// Set when a reader stopped waiting over a published fill it could not use.
    /// See [`lock::UnusableFills`].
    pub lock_abandon: Option<LockAbandon>,
}

/// A cache-lock wait abandoned over a fill the reader could not use. One value
/// rather than two options, because the two are only ever known together.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LockAbandon {
    /// Why this reader cannot use the fill, from its own matched
    /// [`lock::UnusableFill`]. The writer publishes only tokens; the reason is the
    /// reader's, wrapped as [`NoCacheReason::Custom`].
    pub reason: NoCacheReason,
    /// The published token it matched.
    pub token: u64,
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

/// Indicators of which level of cache freshness logic to force apply to an asset.
///
/// For example, should an existing fresh asset be revalidated or re-retrieved altogether.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForcedFreshness {
    /// Indicates the asset should be considered stale and revalidated
    ForceExpired,

    /// Indicates the asset should be considered absent and treated like a miss
    /// instead of a hit
    ForceMiss,

    /// Indicates the asset should be considered fresh despite possibly being stale
    ForceFresh,
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

    /// Asset exists but is expired, forced to be a hit
    ForceFresh,
}

impl HitStatus {
    /// For displaying cache hit status
    pub fn as_str(&self) -> &'static str {
        self.into()
    }

    /// Whether cached asset can be served as fresh
    pub fn is_fresh(&self) -> bool {
        *self == HitStatus::Fresh || *self == HitStatus::ForceFresh
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
    pub max_retries: Option<usize>,
}

// Fields like storage handlers that are needed only when cache is enabled (or bypassing).
struct HttpCacheInnerEnabled {
    pub meta: Option<CacheMeta>,
    // when set, even if an asset exists, it would only be considered valid after this timestamp
    pub valid_after: Option<SystemTime>,
    // Variance from the stale metadata before set_cache_meta() replaces it.
    // update_variance() uses this to detect Vary family changes and reset provenance.
    stale_meta_variance: Option<HashBinary>,
    pub miss_handler: Option<MissHandler>,
    pub body_reader: Option<HitHandler>,
    pub storage: &'static (dyn storage::Storage + Sync), // static for now
    pub eviction: Option<&'static (dyn eviction::EvictionManager + Sync)>,
    pub admission: Option<&'static dyn AdmissionPolicy>,
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
    // Why the predictor considered this key uncacheable, captured in bypass() so later
    // phases report the reason they acted on rather than inferring one. Outlives cache
    // disablement because it is read after the response header arrives.
    pub predicted_uncacheable_reason: Option<NoCacheReason>,
}

#[derive(Debug, Default)]
#[non_exhaustive]
pub struct CacheOptionOverrides {
    /// How long a cache lock reader should wait before giving up.
    pub wait_timeout: Option<Duration>,
    /// How many times a cache lock reader should retry lookup after waiting on a lock.
    pub max_lock_retries: Option<usize>,
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

    /// Say something about this request's cache fill, so readers coalescing behind
    /// it that cannot use it stop waiting. See [`lock::UnusableFills`] for the
    /// reader's side.
    ///
    /// No-op unless this request holds the write lock. Each call **replaces** the
    /// last, so publish the whole set each time.
    pub fn lock_publish_fill_tokens(&self, tokens: &[u64]) {
        if let Some(Locked::Write(permit)) = self
            .inner
            .as_ref()
            .and_then(|inner| inner.enabled_ctx.as_ref())
            .and_then(|enabled| enabled.lock_ctx.as_ref())
            .and_then(|lock_ctx| lock_ctx.lock.as_ref())
        {
            permit.publish(tokens);
        }
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
                        Custom(reason) => lock_ctx.cache_lock.custom_lock_status(reason),
                        // should never happen, NeverEnabled shouldn't hold a lock
                        NeverEnabled => panic!("NeverEnabled holds a write lock"),
                        CacheLockGiveUp | CacheLockTimeout | CacheLockRetryLimit => {
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
                #[cfg_attr(not(feature = "trace"), allow(unused_mut))]
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
                // Record why the predictor gave up on this key while we still know.
                // Reading it later would race with concurrent requests re-marking the key.
                let predicted_reason = self
                    .inner()
                    .predictor
                    .and_then(|predictor| predictor.predicted_uncacheable_reason(self.cache_key()));
                self.inner_mut().predicted_uncacheable_reason = predicted_reason;

                let traces = &mut self.inner_enabled_mut().traces;
                traces
                    .cache_span
                    .set_tag(|| trace::Tag::new("bypassed", true));
                if let Some(reason) = predicted_reason {
                    traces
                        .cache_span
                        .set_tag(|| trace::Tag::new("bypass_reason", reason.as_str()));
                }
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

                let wait_timeout = option_overrides
                    .as_ref()
                    .and_then(|overrides| overrides.wait_timeout);
                let max_retries = option_overrides
                    .as_ref()
                    .and_then(|overrides| overrides.max_lock_retries);
                let lock_ctx = cache_lock.map(|cache_lock| LockCtx {
                    cache_lock,
                    lock: None,
                    wait_timeout,
                    max_retries,
                });

                self.inner = Some(Box::new(HttpCacheInner {
                    enabled_ctx: Some(Box::new(HttpCacheInnerEnabled {
                        meta: None,
                        valid_after: None,
                        stale_meta_variance: None,
                        miss_handler: None,
                        body_reader: None,
                        storage,
                        eviction,
                        admission: None,
                        lock_ctx,
                        traces: CacheTraceCTX::new(),
                    })),
                    key: None,
                    max_file_size_tracker: None,
                    predictor,
                    predicted_uncacheable_reason: None,
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
                    let wait_timeout = option_overrides
                        .as_ref()
                        .and_then(|overrides| overrides.wait_timeout);
                    let max_retries = option_overrides
                        .as_ref()
                        .and_then(|overrides| overrides.max_lock_retries);
                    let lock_ctx = cache_lock.map(|cache_lock| LockCtx {
                        cache_lock,
                        lock: None,
                        wait_timeout,
                        max_retries,
                    });
                    inner_enabled.lock_ctx = lock_ctx;
                }
            }
            _ => panic!("wrong phase: {:?}", self.phase),
        }
    }

    /// Set the [`AdmissionPolicy`] used to decide whether an absent key may fill the cache.
    ///
    /// The policy is only consulted when storage reports a raw miss. Entries rejected
    /// by `valid_after` filtering still follow the normal miss path.
    ///
    /// # Panics
    ///
    /// Panics after a cache lookup or fill has started.
    pub fn set_admission_policy(&mut self, policy: &'static dyn AdmissionPolicy) {
        match self.phase {
            CachePhase::Uninit | CachePhase::CacheKey => {
                self.inner_enabled_mut().admission = Some(policy);
            }
            _ => panic!("wrong phase to set admission policy: {:?}", self.phase),
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
            .is_none_or(|t| t.add_body_bytes(bytes_len))
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
            HitStatus::Fresh | HitStatus::ForceFresh => CachePhase::Hit,
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
        let stale = phase == CachePhase::Stale;
        if stale || hit_status.is_treated_as_miss() {
            if let Some(lock_ctx) = inner_enabled.lock_ctx.as_mut() {
                lock_ctx.lock = Some(lock_ctx.cache_lock.lock(key, stale));
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
                let cache_key = key.to_compact();
                if hit_handler.should_count_access() {
                    let size = hit_handler.get_eviction_weight();
                    let entry_key =
                        eviction::CacheEntryKey::from_entry_id(cache_key, hit_handler.entry_id());
                    eviction.access(&entry_key, size, meta.0.internal.fresh_until);
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
                let inner_enabled = self.inner_enabled_mut();
                inner_enabled.meta = None;
                inner_enabled.stale_meta_variance = None;
                inner_enabled.traces.start_miss_span();
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

    /// Return whether the underlying storage backend supports streaming partial write.
    ///
    /// Returns None if cache is not enabled.
    pub fn support_streaming_partial_write(&self) -> Option<bool> {
        self.inner.as_ref().and_then(|inner| {
            inner
                .enabled_ctx
                .as_ref()
                .map(|c| c.storage.support_streaming_partial_write())
        })
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
                let Some(miss_handler) = inner_enabled.miss_handler.take() else {
                    // already finished, we allow calling this function more than once
                    return Ok(());
                };
                // Save the entry ID before `finish` consumes the miss handler.
                let entry_id = miss_handler.entry_id();
                let finish_result = miss_handler.finish().await;
                let key = inner
                    .key
                    .as_ref()
                    .expect("key set by miss or expired phase");
                if let Some(lock_ctx) = inner_enabled.lock_ctx.as_mut() {
                    let lock = lock_ctx.lock.take();
                    if let Some(Locked::Write(permit)) = lock {
                        // no need to call r.unlock() because release() will call it
                        // r is a guard to make sure the lock is unlocked when this request is dropped
                        let lock_status = if finish_result.is_ok() {
                            LockStatus::Done
                        } else {
                            LockStatus::TransientError
                        };
                        lock_ctx.cache_lock.release(key, permit, lock_status);
                    }
                }
                let size = match finish_result {
                    Ok(size) => size,
                    Err(e) => {
                        inner_enabled.traces.finish_miss_span();
                        return Err(e);
                    }
                };
                if let Some(eviction) = inner_enabled.eviction {
                    let cache_key = key.to_compact();
                    let meta = inner_enabled.meta.as_ref().unwrap();
                    let entry_key = eviction::CacheEntryKey::from_entry_id(cache_key, entry_id);
                    let evicted = match size {
                        MissFinishType::Created(size) => {
                            eviction.admit(entry_key, size, meta.0.internal.fresh_until)
                        }
                        MissFinishType::Appended(size, max_size) => {
                            eviction.increment_weight(&entry_key, size, max_size)
                        }
                    };
                    // actual eviction can be done async
                    let span = inner_enabled.traces.child("eviction");
                    let handle = span.handle();
                    let storage = inner_enabled.storage;
                    tokio::task::spawn(async move {
                        for item in evicted {
                            let target = storage::PurgeTarget::Exact(&item);
                            if let Err(e) =
                                storage.purge(target, PurgeType::Eviction, &handle).await
                            {
                                warn!(
                                    "Failed to purge {target} during eviction for finish miss handler: {e}"
                                );
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
    ///
    /// # Panics
    ///
    /// Panics unless called in [CachePhase::Miss] or [CachePhase::Stale]. In stale phase, the
    /// stale metadata must still be present.
    pub fn set_cache_meta(&mut self, mut meta: CacheMeta) {
        match self.phase {
            // TODO: store the staled meta somewhere else for future use?
            CachePhase::Stale => {
                let inner_enabled = self.inner_enabled_mut();
                let old_meta = inner_enabled
                    .meta
                    .as_ref()
                    .expect("stale phase has cache meta");
                inner_enabled.stale_meta_variance = old_meta.variance();
                meta.set_provenance(old_meta.provenance());
                // TODO: have a separate expired span?
                inner_enabled.traces.log_meta_in_miss_span(&meta);
                inner_enabled.meta = Some(meta);
            }
            CachePhase::Miss => {
                let inner_enabled = self.inner_enabled_mut();
                inner_enabled.stale_meta_variance = None;
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
                let provenance = old_meta.provenance();
                meta.0.internal.created = created;
                meta.set_provenance(provenance);
                // meta.internal.updated was already set to new meta's `created`,
                // no need to set `updated` here
                // Merge old extensions with new ones. New exts take precedence if they conflict.
                let mut extensions = old_meta.0.extensions;
                extensions.extend(meta.0.extensions);
                meta.0.extensions = extensions;
                inner_enabled.stale_meta_variance = None;

                inner_enabled.meta.replace(meta);

                #[cfg_attr(not(feature = "trace"), allow(unused_mut))]
                let mut span = inner_enabled.traces.child("update_meta");
                let result = inner_enabled
                    .storage
                    .update_meta(
                        inner.key.as_ref().unwrap(),
                        inner_enabled.meta.as_ref().unwrap(),
                        &span.handle(),
                    )
                    .await;
                span.set_tag(|| trace::Tag::new("updated", result.is_ok()));

                // regardless of result, release the cache lock
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
        // Because the current location of the asset is already the primary variant, the lookup key
        // does not need to change. If this is an expired response, this is a new Vary family, so
        // provenance is reset to the refreshed metadata's created timestamp.
        //
        // **Case 2**: Variance was present, but it changed or was removed.
        // We want the current asset to take over the primary slot, in order to invalidate all
        // other variants derived under the old Vary. For expired responses, provenance is reset
        // to the refreshed metadata's created timestamp.
        //
        // **Case 3**: Variance did not change.
        // Nothing needs to happen.
        //
        // These provenance updates do not provide ordering on their own. Writers need a cache lock
        // to avoid racing each other. A purge can still race with a stale refresh: whichever observes
        // or writes storage last determines whether old provenance is carried forward or replaced.
        let phase = self.phase;
        let inner = match phase {
            CachePhase::Miss | CachePhase::Expired => self.inner_mut(),
            _ => panic!("wrong phase {:?}", self.phase),
        };
        let inner_enabled = inner
            .enabled_ctx
            .as_mut()
            .expect("cache enabled on miss and expired");
        let old_key_variance = inner.key.as_ref().unwrap().get_variance_key().copied();
        let stale_meta_variance = if phase == CachePhase::Expired {
            inner_enabled.stale_meta_variance.take()
        } else {
            inner_enabled.stale_meta_variance = None;
            None
        };
        let reset_provenance_to_created = if phase == CachePhase::Expired {
            match old_key_variance {
                Some(old_variance) => Some(old_variance) != variance,
                None => stale_meta_variance != variance,
            }
        } else {
            false
        };

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
        if reset_provenance_to_created {
            inner_enabled
                .meta
                .as_mut()
                .unwrap()
                .reset_provenance_to_created();
        }

        // Change the lookup `key` if necessary, in order to admit asset into the primary slot
        // instead of the secondary slot.
        let key = inner.key.as_ref().unwrap();
        if let Some(old_variance) = old_key_variance {
            // This is a secondary variant slot.
            if Some(old_variance) != variance {
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
    /// any phase and will not panic due to a wrong phase. It returns the cache meta in
    /// the phases where one may be set ([CachePhase::Miss], [CachePhase::Stale],
    /// [CachePhase::StaleUpdating], [CachePhase::Expired], [CachePhase::Hit],
    /// [CachePhase::Revalidated], and [CachePhase::RevalidatedNoCache]); in all other
    /// phases it returns `None` because no cache meta can exist.
    pub fn maybe_cache_meta(&self) -> Option<&CacheMeta> {
        match self.phase {
            CachePhase::Miss
            | CachePhase::Stale
            | CachePhase::StaleUpdating
            | CachePhase::Expired
            | CachePhase::Hit
            | CachePhase::Revalidated
            | CachePhase::RevalidatedNoCache(_) => self.inner_enabled().meta.as_ref(),
            _ => None,
        }
    }

    /// Return the [`CacheKey`] of this asset if any.
    ///
    /// This is allowed to be called in any phase. If the cache key callback was not called,
    /// this will return None.
    pub fn maybe_cache_key(&self) -> Option<&CacheKey> {
        (!matches!(
            self.phase(),
            CachePhase::Disabled(NoCacheReason::NeverEnabled) | CachePhase::Uninit
        ))
        .then(|| self.cache_key())
    }

    /// Perform the cache lookup from the given cache storage with the given cache key
    ///
    /// A cache hit will return [CacheMeta] which contains the header and meta info about
    /// the cache as well as a [HitHandler] to read the cache hit body.
    ///
    /// When an admission policy defers a raw storage miss, this returns `Ok(None)` and disables
    /// caching with [`NoCacheReason::Deferred`]. Callers must check [`Self::enabled()`] before
    /// calling [`Self::cache_miss()`].
    ///
    /// Admission is observed at most once per [`HttpCache`], on an initial
    /// [`CachePhase::CacheKey`] raw storage miss. Retried lookups and stale refills reuse the
    /// existing admission outcome or proceed without another observation.
    ///
    /// Entries rejected by `valid_after` filtering are not raw storage misses and bypass
    /// admission. After an invalidation, admission therefore does not provide additional
    /// suppression for concurrent fills beyond the configured cache-lock behavior.
    ///
    /// # Panic
    /// Panic in other phases.
    pub async fn cache_lookup(&mut self) -> Result<Option<(CacheMeta, HitHandler)>> {
        match self.phase {
            // Stale is allowed here because stale-> cache_lock -> lookup again
            CachePhase::CacheKey | CachePhase::Stale => {
                let observe_admission =
                    self.phase == CachePhase::CacheKey && self.digest.admission.is_none();
                let (result, admission) = {
                    let inner = self
                        .inner
                        .as_mut()
                        .expect("Cache phase is checked and should have inner");
                    let inner_enabled = inner
                        .enabled_ctx
                        .as_mut()
                        .expect("Cache enabled on cache_lookup");
                    #[cfg_attr(not(feature = "trace"), allow(unused_mut))]
                    let mut span = inner_enabled.traces.child("lookup");
                    let key = inner.key.as_ref().unwrap(); // safe, this phase should have cache key
                    let now = Instant::now();
                    let result = inner_enabled.storage.lookup(key, &span.handle()).await?;
                    // one request may have multiple lookups
                    self.digest.add_lookup_duration(now.elapsed());
                    let storage_miss = result.is_none();
                    let result = result.and_then(|(meta, header)| {
                        if let Some(ts) = inner_enabled.valid_after {
                            // `created` (not `provenance`) is the right field to compare on
                            // the variant side: we are asking "was this specific variant
                            // admitted before the primary's tombstone?" -- a fact about the
                            // variant entry itself.
                            if meta.created() < ts {
                                span.set_tag(|| trace::Tag::new("not valid", true));
                                return None;
                            }
                        }
                        Some((meta, header))
                    });
                    let admission = (storage_miss && observe_admission)
                        .then(|| inner_enabled.admission.map(|policy| policy.observe(key)))
                        .flatten();
                    if let Some(decision) = admission {
                        span.set_tag(|| {
                            trace::Tag::new("admission.observed", decision.observed() as i64)
                        });
                        span.set_tag(|| {
                            trace::Tag::new("admission.deferred", decision.is_deferred())
                        });
                    }
                    if result.is_none() && admission.is_none_or(|decision| !decision.is_deferred())
                    {
                        if let Some(lock_ctx) = inner_enabled.lock_ctx.as_mut() {
                            lock_ctx.lock = Some(lock_ctx.cache_lock.lock(key, false));
                        }
                    }
                    span.set_tag(|| trace::Tag::new("found", result.is_some()));
                    (result, admission)
                };
                if let Some(decision) = admission {
                    self.digest.admission = Some(decision);
                    if decision.is_deferred() {
                        self.disable(NoCacheReason::Deferred);
                    }
                }
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
                // the provenance timestamp of the primary is the tombstone of all the variances
                inner
                    .enabled_ctx
                    .as_mut()
                    .expect("cache enabled")
                    .valid_after = Some(meta.provenance());

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

    /// Maximum number of cache lock retries configured for this request.
    pub fn cache_lock_max_retries(&self) -> Option<usize> {
        self.inner_enabled()
            .lock_ctx
            .as_ref()
            .and_then(|l| l.max_retries)
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
    ///
    /// A request carrying an [`lock::UnusableFills`] on its cache key can also stop
    /// early with [`LockWaitOutcome::Abandoned`], which [`Self::lock_abandon`] keeps
    /// afterwards.
    ///
    /// # Panic
    /// Check [Self::is_cache_locked()], panic if this request doesn't have a read lock.
    pub async fn cache_lock_wait(&mut self) -> LockWaitOutcome {
        // Taken before the mutable borrow below. Naming nothing waits as always.
        let unusable = self
            .maybe_cache_key()
            .and_then(|key| key.extensions.get::<UnusableFills>())
            .cloned();

        let inner_enabled = self.inner_enabled_mut();
        #[cfg_attr(not(feature = "trace"), allow(unused_mut))]
        let mut span = inner_enabled.traces.child("cache_lock");
        // should always call is_cache_locked() before this function, which should guarantee that
        // the inner cache has a read lock and lock ctx
        let (read_lock, outcome) = if let Some(lock_ctx) = inner_enabled.lock_ctx.as_mut() {
            let lock = lock_ctx.lock.take(); // remove the lock from self
            if let Some(Locked::Read(r)) = lock {
                let now = Instant::now();
                // it's possible for a request to be locked more than once,
                // so wait the remainder of our configured timeout
                let wait = async {
                    match unusable.as_ref() {
                        Some(unusable) => r.wait_unless_published(unusable).await,
                        None => {
                            r.wait().await;
                            WaitOutcome::Released
                        }
                    }
                };
                let outcome = if let Some(wait_timeout) = lock_ctx.wait_timeout {
                    let wait_timeout =
                        wait_timeout.saturating_sub(self.lock_duration().unwrap_or(Duration::ZERO));
                    match timeout(wait_timeout, wait).await {
                        Ok(outcome) => Self::wait_result(&r, outcome),
                        Err(_) => LockWaitOutcome::WaitTimeout,
                    }
                } else {
                    Self::wait_result(&r, wait.await)
                };
                self.digest.add_lock_duration(now.elapsed());
                // On the digest as well as returned: a logging filter reports it
                // long after the caller has acted on the outcome.
                if let LockWaitOutcome::Abandoned { reason, token } = outcome {
                    self.digest.lock_abandon = Some(LockAbandon { reason, token });
                }
                (r, outcome)
            } else {
                panic!("cache_lock_wait on wrong type of lock")
            }
        } else {
            panic!("cache_lock_wait without cache lock")
        };
        if let Some(lock_ctx) = self.inner_enabled().lock_ctx.as_ref() {
            lock_ctx
                .cache_lock
                .trace_lock_wait(&mut span, &read_lock, outcome.lock_status());
        }
        outcome
    }

    /// How long did this request wait behind the read lock
    pub fn lock_duration(&self) -> Option<Duration> {
        self.digest.lock_duration
    }

    /// An abandoning reader's outcome is deliberately not written to the shared
    /// lock status: the writer and every other reader are unaffected.
    fn wait_result(lock: &lock::ReadLock, outcome: WaitOutcome) -> LockWaitOutcome {
        match outcome {
            WaitOutcome::Abandoned(matched) => LockWaitOutcome::Abandoned {
                reason: NoCacheReason::Custom(matched.reason),
                token: matched.token,
            },
            WaitOutcome::Released | WaitOutcome::AgeTimeout => {
                Self::released_result(lock.lock_status())
            }
        }
    }

    /// A released lock should never still read [`LockStatus::Waiting`]. `Dangling`
    /// already means "bad state, recompete", and warns, so no panic is needed.
    fn released_result(status: LockStatus) -> LockWaitOutcome {
        match status {
            LockStatus::Done => LockWaitOutcome::Done,
            LockStatus::TransientError => LockWaitOutcome::TransientError,
            LockStatus::Dangling => LockWaitOutcome::Dangling,
            LockStatus::WaitTimeout => LockWaitOutcome::WaitTimeout,
            LockStatus::AgeTimeout => LockWaitOutcome::AgeTimeout,
            LockStatus::GiveUp => LockWaitOutcome::GiveUp,
            LockStatus::Waiting => {
                debug_assert!(false, "a released lock cannot still be Waiting");
                LockWaitOutcome::Dangling
            }
        }
    }

    /// The fill this request stopped waiting over, and why it could not use it.
    ///
    /// Set only when [`Self::cache_lock_wait`] returned
    /// [`LockWaitOutcome::Abandoned`]. Absent for every other outcome, including
    /// [`LockWaitOutcome::GiveUp`], which is the writer giving up rather than this
    /// request abandoning the wait.
    pub fn lock_abandon(&self) -> Option<LockAbandon> {
        self.digest.lock_abandon
    }

    /// How long did this request spent on cache lookup and reading the header
    pub fn lookup_duration(&self) -> Option<Duration> {
        self.digest.lookup_duration
    }

    /// Return the [`Decision`] made for an absent cache key.
    pub fn admission_decision(&self) -> Option<Decision> {
        self.digest.admission
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

    #[cfg_attr(not(feature = "trace"), allow(unused_mut))]
    async fn purge_impl(
        storage: &'static (dyn storage::Storage + Sync),
        eviction: Option<&'static (dyn eviction::EvictionManager + Sync)>,
        key: &CompactCacheKey,
        mut span: Span,
    ) -> Result<bool> {
        let target = storage::PurgeTarget::Active(key);
        let result = storage
            .purge(target, PurgeType::Invalidation, &span.handle())
            .await;
        let purged = match result.as_ref() {
            Ok(storage::PurgeOutcome::NotFound) | Err(_) => false,
            Ok(storage::PurgeOutcome::Purged(entry_id)) => {
                if let Some(eviction) = eviction {
                    eviction.remove(target.removed_entry(*entry_id));
                }
                true
            }
        };
        span.set_tag(|| trace::Tag::new("purged", purged));
        result?;
        Ok(purged)
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

    /// The reason the predictor remembered for this key when [Self::bypass] ran.
    ///
    /// `None` when the cache was not bypassed, when no predictor is configured, or when the
    /// predictor does not track reasons. Callers must treat `None` as "unknown" rather than
    /// as evidence about the previous response.
    pub fn predicted_uncacheable_reason(&self) -> Option<NoCacheReason> {
        self.inner
            .as_ref()
            .and_then(|inner| inner.predicted_uncacheable_reason)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lock::{CacheLock, UnusableFill};
    use async_trait::async_trait;
    use http::StatusCode;
    use std::any::Any;
    use std::num::NonZeroU32;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{LazyLock, Mutex};

    /// Storage fixture with successful metadata updates and configurable purge results.
    struct UpdateOkStorage {
        purge_ok: bool,
    }
    struct IdentifiedEntryStorage {
        append: bool,
    }
    struct OneShotLookupStorage {
        entries: Mutex<Vec<(CompactCacheKey, CacheMeta)>>,
    }
    struct EmptyHitHandler {
        entry_id: Option<u64>,
    }
    struct IdentifiedMissHandler {
        finish: MissFinishType,
    }
    struct CountingDeferPolicy(AtomicUsize);
    struct CountingReadyPolicy(AtomicUsize);
    #[derive(Default)]
    struct RecordingEviction {
        removed: Mutex<Option<eviction::CacheEntryKey>>,
        accessed: Mutex<Option<eviction::CacheEntryKey>>,
        admitted: Mutex<Option<eviction::CacheEntryKey>>,
        incremented: Mutex<Option<(eviction::CacheEntryKey, usize, Option<usize>)>>,
    }

    static UPDATE_OK_STORAGE: UpdateOkStorage = UpdateOkStorage { purge_ok: false };
    static PURGE_OK_STORAGE: UpdateOkStorage = UpdateOkStorage { purge_ok: true };
    static IDENTIFIED_CREATED_STORAGE: IdentifiedEntryStorage =
        IdentifiedEntryStorage { append: false };
    static IDENTIFIED_APPENDED_STORAGE: IdentifiedEntryStorage =
        IdentifiedEntryStorage { append: true };
    // Only one test uses this storage. Keep it that way unless the tests also isolate their keys
    // and clear any entries they push.
    static ONE_SHOT_LOOKUP_STORAGE: OneShotLookupStorage = OneShotLookupStorage {
        entries: Mutex::new(Vec::new()),
    };
    static RAW_MISS_DEFER_POLICY: CountingDeferPolicy = CountingDeferPolicy(AtomicUsize::new(0));
    static VALID_AFTER_DEFER_POLICY: CountingDeferPolicy = CountingDeferPolicy(AtomicUsize::new(0));
    static STALE_DEFER_POLICY: CountingDeferPolicy = CountingDeferPolicy(AtomicUsize::new(0));
    static RAW_MISS_READY_POLICY: CountingReadyPolicy = CountingReadyPolicy(AtomicUsize::new(0));
    static TWO_USE_ADMISSION_POLICY: LazyLock<admission::MinUsesAdmissionPolicy> =
        LazyLock::new(|| admission::MinUsesAdmissionPolicy::new(NonZeroU32::new(2).unwrap()));
    impl AdmissionPolicy for CountingDeferPolicy {
        fn observe(&self, _key: &CacheKey) -> Decision {
            self.0.fetch_add(1, Ordering::Relaxed);
            Decision::Defer { observed: 1 }
        }
    }

    impl AdmissionPolicy for CountingReadyPolicy {
        fn observe(&self, _key: &CacheKey) -> Decision {
            let observed = self.0.fetch_add(1, Ordering::Relaxed) + 1;
            Decision::Ready {
                observed: observed as u32,
            }
        }
    }

    #[async_trait]
    impl storage::HandleHit for EmptyHitHandler {
        async fn read_body(&mut self) -> Result<Option<bytes::Bytes>> {
            Ok(None)
        }

        async fn finish(
            self: Box<Self>,
            _storage: &'static (dyn Storage + Sync),
            _key: &CacheKey,
            _trace: &trace::SpanHandle,
        ) -> Result<()> {
            Ok(())
        }

        fn as_any(&self) -> &(dyn Any + Send + Sync) {
            self
        }

        fn as_any_mut(&mut self) -> &mut (dyn Any + Send + Sync) {
            self
        }

        fn entry_id(&self) -> Option<eviction::CacheEntryId> {
            self.entry_id.map(eviction::CacheEntryId::new)
        }
    }

    #[async_trait]
    impl storage::HandleMiss for IdentifiedMissHandler {
        async fn write_body(&mut self, _data: bytes::Bytes, _eof: bool) -> Result<()> {
            Ok(())
        }

        async fn finish(self: Box<Self>) -> Result<MissFinishType> {
            Ok(self.finish)
        }

        fn entry_id(&self) -> Option<eviction::CacheEntryId> {
            Some(eviction::CacheEntryId::new(7))
        }
    }

    #[async_trait]
    impl Storage for UpdateOkStorage {
        async fn lookup(
            &'static self,
            _key: &CacheKey,
            _trace: &trace::SpanHandle,
        ) -> Result<Option<(CacheMeta, HitHandler)>> {
            Ok(None)
        }

        async fn get_miss_handler(
            &'static self,
            _key: &CacheKey,
            _meta: &CacheMeta,
            _trace: &trace::SpanHandle,
        ) -> Result<MissHandler> {
            unreachable!("tests do not write bodies through this storage")
        }

        async fn purge(
            &'static self,
            _target: storage::PurgeTarget<'_>,
            _purge_type: PurgeType,
            _trace: &trace::SpanHandle,
        ) -> Result<storage::PurgeOutcome> {
            Ok(if self.purge_ok {
                storage::PurgeOutcome::Purged(None)
            } else {
                storage::PurgeOutcome::NotFound
            })
        }

        async fn update_meta(
            &'static self,
            _key: &CacheKey,
            _meta: &CacheMeta,
            _trace: &trace::SpanHandle,
        ) -> Result<bool> {
            Ok(true)
        }

        fn as_any(&self) -> &(dyn Any + Send + Sync + 'static) {
            self
        }
    }

    #[async_trait]
    impl Storage for IdentifiedEntryStorage {
        async fn lookup(
            &'static self,
            _key: &CacheKey,
            _trace: &trace::SpanHandle,
        ) -> Result<Option<(CacheMeta, HitHandler)>> {
            Ok(None)
        }

        async fn get_miss_handler(
            &'static self,
            _key: &CacheKey,
            _meta: &CacheMeta,
            _trace: &trace::SpanHandle,
        ) -> Result<MissHandler> {
            let finish = if self.append {
                MissFinishType::Appended(2, Some(9))
            } else {
                MissFinishType::Created(1)
            };
            Ok(Box::new(IdentifiedMissHandler { finish }))
        }

        async fn purge(
            &'static self,
            target: storage::PurgeTarget<'_>,
            _purge_type: PurgeType,
            _trace: &trace::SpanHandle,
        ) -> Result<storage::PurgeOutcome> {
            let entry_id = match target {
                storage::PurgeTarget::Active(_) => Some(eviction::CacheEntryId::new(1)),
                storage::PurgeTarget::Exact(_) => None,
            };
            Ok(storage::PurgeOutcome::Purged(entry_id))
        }

        async fn update_meta(
            &'static self,
            _key: &CacheKey,
            _meta: &CacheMeta,
            _trace: &trace::SpanHandle,
        ) -> Result<bool> {
            Ok(true)
        }

        fn as_any(&self) -> &(dyn Any + Send + Sync + 'static) {
            self
        }
    }

    #[async_trait]
    impl eviction::EvictionManager for RecordingEviction {
        fn total_size(&self) -> usize {
            0
        }

        fn total_items(&self) -> usize {
            0
        }

        fn evicted_size(&self) -> usize {
            0
        }

        fn evicted_items(&self) -> usize {
            0
        }

        fn admit(
            &self,
            item: eviction::CacheEntryKey,
            _size: usize,
            _fresh_until: SystemTime,
        ) -> Vec<eviction::CacheEntryKey> {
            *self.admitted.lock().unwrap() = Some(item);
            Vec::new()
        }

        fn increment_weight(
            &self,
            item: &eviction::CacheEntryKey,
            delta: usize,
            max_weight: Option<usize>,
        ) -> Vec<eviction::CacheEntryKey> {
            *self.incremented.lock().unwrap() = Some((item.clone(), delta, max_weight));
            Vec::new()
        }

        fn remove(&self, item: eviction::CacheEntryKeyRef<'_>) {
            *self.removed.lock().unwrap() = Some(eviction::CacheEntryKey::from_entry_id(
                item.key().clone(),
                item.entry_id(),
            ));
        }

        fn access(
            &self,
            item: &eviction::CacheEntryKey,
            _size: usize,
            _fresh_until: SystemTime,
        ) -> bool {
            *self.accessed.lock().unwrap() = Some(item.clone());
            true
        }

        fn peek(&self, _item: &eviction::CacheEntryKey) -> bool {
            false
        }

        async fn save(&self, _dir_path: &str) -> Result<()> {
            Ok(())
        }

        async fn load(&self, _dir_path: &str) -> Result<()> {
            Ok(())
        }
    }

    #[async_trait]
    impl Storage for OneShotLookupStorage {
        async fn lookup(
            &'static self,
            key: &CacheKey,
            _trace: &trace::SpanHandle,
        ) -> Result<Option<(CacheMeta, HitHandler)>> {
            let compact_key = key.to_compact();
            let mut entries = self.entries.lock().unwrap();
            let Some(pos) = entries
                .iter()
                .position(|(entry_key, _)| entry_key == &compact_key)
            else {
                return Ok(None);
            };
            let (_, meta) = entries.remove(pos);
            Ok(Some((meta, Box::new(EmptyHitHandler { entry_id: None }))))
        }

        async fn get_miss_handler(
            &'static self,
            _key: &CacheKey,
            _meta: &CacheMeta,
            _trace: &trace::SpanHandle,
        ) -> Result<MissHandler> {
            unreachable!("tests do not write bodies through this storage")
        }

        async fn purge(
            &'static self,
            _target: storage::PurgeTarget<'_>,
            _purge_type: PurgeType,
            _trace: &trace::SpanHandle,
        ) -> Result<storage::PurgeOutcome> {
            Ok(storage::PurgeOutcome::NotFound)
        }

        async fn update_meta(
            &'static self,
            _key: &CacheKey,
            _meta: &CacheMeta,
            _trace: &trace::SpanHandle,
        ) -> Result<bool> {
            Ok(true)
        }

        fn as_any(&self) -> &(dyn Any + Send + Sync + 'static) {
            self
        }
    }

    fn test_meta(created: SystemTime) -> CacheMeta {
        let header = ResponseHeader::build(StatusCode::OK, None).unwrap();
        CacheMeta::new(created + Duration::from_secs(60), created, 30, 30, header)
    }

    fn cache_with_stale_meta(meta: CacheMeta, key: CacheKey) -> HttpCache {
        let mut cache = HttpCache::new();
        cache.enable(&UPDATE_OK_STORAGE, None, None, None, None);
        cache.set_cache_key(key);
        cache.phase = CachePhase::Stale;
        cache.inner_enabled_mut().meta = Some(meta);
        cache
    }

    static FILL_INTEREST_LOCK: LazyLock<CacheLock> =
        LazyLock::new(|| CacheLock::new(Duration::from_secs(30)));

    const WRONG_PLACE: u64 = 7;
    const SOMEWHERE_ELSE: u64 = 9;

    /// Reasons are the application's, not the cache's; it wraps them as
    /// [`NoCacheReason::Custom`].
    const NO_GOOD: &str = "NoGoodToThisReader";
    const NO_GOOD_EITHER: &str = "AlsoNoGood";

    fn cannot_use(token: u64) -> UnusableFills {
        UnusableFills {
            fills: vec![UnusableFill {
                token,
                reason: NO_GOOD,
            }]
            .into(),
        }
    }

    fn locked_reader(key: &str, interest: Option<UnusableFills>) -> HttpCache {
        let mut cache_key = CacheKey::new(key, "");
        if let Some(interest) = interest {
            cache_key.extensions.insert(interest);
        }
        let mut cache = HttpCache::new();
        cache.enable(
            &UPDATE_OK_STORAGE,
            None,
            None,
            Some(&*FILL_INTEREST_LOCK),
            None,
        );
        cache.set_cache_key(cache_key);
        cache
    }

    /// A reader that stops waiting reports `GiveUp` with its own reason, so the
    /// give-up is attributed to why it stopped rather than the generic
    /// `CacheLockGiveUp`.
    #[tokio::test]
    async fn a_reader_that_stops_waiting_reports_its_own_reason() {
        let key = "stops-waiting";

        let mut writer = locked_reader(key, None);
        assert!(writer.cache_lookup().await.unwrap().is_none());
        assert!(!writer.is_cache_locked(), "the first request is the writer");

        let mut reader = locked_reader(key, Some(cannot_use(WRONG_PLACE)));
        assert!(reader.cache_lookup().await.unwrap().is_none());
        assert!(reader.is_cache_locked(), "the second request coalesces");

        // The writer learns where it is filling from, and says so.
        let waiting = tokio::spawn(async move {
            let status = reader.cache_lock_wait().await;
            (status, reader.lock_abandon())
        });
        tokio::task::yield_now().await;
        assert!(!waiting.is_finished(), "nothing published yet");

        writer.lock_publish_fill_tokens(&[WRONG_PLACE]);

        assert_eq!(
            waiting.await.unwrap(),
            (
                LockWaitOutcome::Abandoned {
                    reason: NoCacheReason::Custom(NO_GOOD),
                    token: WRONG_PLACE,
                },
                Some(LockAbandon {
                    reason: NoCacheReason::Custom(NO_GOOD),
                    token: WRONG_PLACE,
                })
            ),
            "its own reason and token, on the outcome and on the digest alike"
        );

        // The writer never released, so a reader arriving afterwards without an
        // interest still coalesces behind it.
        let mut other = locked_reader(key, None);
        assert!(other.cache_lookup().await.unwrap().is_none());
        assert!(other.is_cache_locked(), "the lock is untouched");

        writer.release_write_lock(NoCacheReason::StorageError);
    }

    /// The reason follows the token that matched, not the set: a key carries one
    /// [`UnusableFills`], so unrelated parts of an application share it.
    #[tokio::test]
    async fn the_reason_comes_from_the_token_that_matched() {
        let key = "reason-per-token";

        let mut writer = locked_reader(key, None);
        assert!(writer.cache_lookup().await.unwrap().is_none());

        let interest = UnusableFills {
            fills: vec![
                UnusableFill {
                    token: WRONG_PLACE,
                    reason: NO_GOOD,
                },
                UnusableFill {
                    token: SOMEWHERE_ELSE,
                    reason: NO_GOOD_EITHER,
                },
            ]
            .into(),
        };
        let mut reader = locked_reader(key, Some(interest));
        assert!(reader.cache_lookup().await.unwrap().is_none());
        assert!(reader.is_cache_locked());

        let waiting = tokio::spawn(async move {
            let status = reader.cache_lock_wait().await;
            (status, reader.lock_abandon())
        });
        tokio::task::yield_now().await;

        // Only the second token is published, so only its reason may surface.
        writer.lock_publish_fill_tokens(&[SOMEWHERE_ELSE]);

        assert_eq!(
            waiting.await.unwrap(),
            (
                LockWaitOutcome::Abandoned {
                    reason: NoCacheReason::Custom(NO_GOOD_EITHER),
                    token: SOMEWHERE_ELSE,
                },
                Some(LockAbandon {
                    reason: NoCacheReason::Custom(NO_GOOD_EITHER),
                    token: SOMEWHERE_ELSE,
                })
            ),
            "the matched token's own reason, not the first in the set"
        );

        writer.release_write_lock(NoCacheReason::StorageError);
    }

    /// A reader whose tokens are never published waits for the writer, as before.
    #[tokio::test]
    async fn a_reader_naming_unpublished_tokens_still_waits() {
        let key = "unpublished-tokens";

        let mut writer = locked_reader(key, None);
        assert!(writer.cache_lookup().await.unwrap().is_none());

        let mut reader = locked_reader(key, Some(cannot_use(WRONG_PLACE)));
        assert!(reader.cache_lookup().await.unwrap().is_none());
        assert!(reader.is_cache_locked());

        let waiting = tokio::spawn(async move { reader.cache_lock_wait().await });
        tokio::task::yield_now().await;
        assert!(!waiting.is_finished(), "the reader is still coalescing");

        writer.release_write_lock(NoCacheReason::StorageError);

        assert_eq!(waiting.await.unwrap(), LockWaitOutcome::TransientError);
    }

    fn cache_with_lookup_storage(key: CacheKey) -> HttpCache {
        let mut cache = HttpCache::new();
        cache.enable(&ONE_SHOT_LOOKUP_STORAGE, None, None, None, None);
        cache.set_cache_key(key);
        cache
    }

    #[tokio::test]
    async fn purge_removes_identified_entry() {
        let recording = Box::leak(Box::new(RecordingEviction::default()));
        let key = CacheKey::new("expanded-purge", "").to_compact();

        assert!(HttpCache::purge_impl(
            &IDENTIFIED_CREATED_STORAGE,
            Some(recording),
            &key,
            trace::Span::inactive(),
        )
        .await
        .unwrap());

        let removed = recording
            .removed
            .lock()
            .unwrap()
            .take()
            .expect("purge should remove the identified entry");
        assert_eq!(
            removed,
            eviction::CacheEntryKey::identified(key, eviction::CacheEntryId::new(1))
        );
        assert!(recording.accessed.lock().unwrap().is_none());
        assert!(recording.admitted.lock().unwrap().is_none());
        assert!(recording.incremented.lock().unwrap().is_none());
    }

    #[tokio::test]
    async fn purge_removes_key_only_entry() {
        let recording = Box::leak(Box::new(RecordingEviction::default()));
        let key = CacheKey::new("key-only-purge", "").to_compact();

        assert!(HttpCache::purge_impl(
            &PURGE_OK_STORAGE,
            Some(recording),
            &key,
            trace::Span::inactive(),
        )
        .await
        .unwrap());

        let removed = recording
            .removed
            .lock()
            .unwrap()
            .take()
            .expect("purge should remove the key-only entry");
        assert_eq!(removed, eviction::CacheEntryKey::key_only(key));
    }

    #[test]
    fn cache_hit_passes_entry_id_to_eviction() {
        let recording = Box::leak(Box::new(RecordingEviction::default()));
        let key = CacheKey::new("identified-hit", "");
        let mut cache = HttpCache::new();
        cache.enable(
            &IDENTIFIED_CREATED_STORAGE,
            Some(recording),
            None,
            None,
            None,
        );
        cache.set_cache_key(key.clone());
        cache.cache_found(
            test_meta(SystemTime::now()),
            Box::new(EmptyHitHandler { entry_id: Some(7) }),
            HitStatus::Fresh,
        );

        assert_eq!(
            recording.accessed.lock().unwrap().take(),
            Some(eviction::CacheEntryKey::identified(
                key.to_compact(),
                eviction::CacheEntryId::new(7)
            ))
        );
        assert!(recording.removed.lock().unwrap().is_none());
        assert!(recording.admitted.lock().unwrap().is_none());
        assert!(recording.incremented.lock().unwrap().is_none());
    }

    #[tokio::test]
    async fn cache_miss_passes_entry_id_to_eviction() {
        let recording = Box::leak(Box::new(RecordingEviction::default()));
        let key = CacheKey::new("identified-miss", "");
        let mut cache = HttpCache::new();
        cache.enable(
            &IDENTIFIED_CREATED_STORAGE,
            Some(recording),
            None,
            None,
            None,
        );
        cache.set_cache_key(key.clone());
        cache.cache_miss();
        cache.set_cache_meta(test_meta(SystemTime::now()));
        cache.set_miss_handler().await.unwrap();
        cache.finish_miss_handler().await.unwrap();
        assert_eq!(
            recording.admitted.lock().unwrap().take(),
            Some(eviction::CacheEntryKey::identified(
                key.to_compact(),
                eviction::CacheEntryId::new(7)
            ))
        );
        assert!(recording.incremented.lock().unwrap().is_none());
        assert!(recording.removed.lock().unwrap().is_none());
        assert!(recording.accessed.lock().unwrap().is_none());

        let mut cache = HttpCache::new();
        cache.enable(
            &IDENTIFIED_APPENDED_STORAGE,
            Some(recording),
            None,
            None,
            None,
        );
        cache.set_cache_key(key.clone());
        cache.cache_miss();
        cache.set_cache_meta(test_meta(SystemTime::now()));
        cache.set_miss_handler().await.unwrap();
        cache.finish_miss_handler().await.unwrap();
        assert_eq!(
            recording.incremented.lock().unwrap().take(),
            Some((
                eviction::CacheEntryKey::identified(
                    key.to_compact(),
                    eviction::CacheEntryId::new(7)
                ),
                2,
                Some(9)
            ))
        );
        assert!(recording.admitted.lock().unwrap().is_none());
        assert!(recording.removed.lock().unwrap().is_none());
        assert!(recording.accessed.lock().unwrap().is_none());
    }

    #[tokio::test]
    async fn raw_storage_miss_can_defer_admission() {
        RAW_MISS_DEFER_POLICY.0.store(0, Ordering::Relaxed);
        let mut cache = HttpCache::new();
        cache.enable(&UPDATE_OK_STORAGE, None, None, None, None);
        cache.set_admission_policy(&RAW_MISS_DEFER_POLICY);
        cache.set_cache_key(CacheKey::new("deferred-storage-miss", ""));

        assert!(cache.cache_lookup().await.unwrap().is_none());
        assert_eq!(cache.phase(), CachePhase::Disabled(NoCacheReason::Deferred));
        assert_eq!(
            cache.admission_decision(),
            Some(Decision::Defer { observed: 1 })
        );
        assert_eq!(RAW_MISS_DEFER_POLICY.0.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn second_storage_miss_can_proceed_to_fill() {
        let key = CacheKey::new("two-use-admission", "");

        let mut first = HttpCache::new();
        first.enable(&UPDATE_OK_STORAGE, None, None, None, None);
        first.set_admission_policy(&*TWO_USE_ADMISSION_POLICY);
        first.set_cache_key(key.clone());
        assert!(first.cache_lookup().await.unwrap().is_none());
        assert_eq!(first.phase(), CachePhase::Disabled(NoCacheReason::Deferred));

        let mut second = HttpCache::new();
        second.enable(&UPDATE_OK_STORAGE, None, None, None, None);
        second.set_admission_policy(&*TWO_USE_ADMISSION_POLICY);
        second.set_cache_key(key);
        assert!(second.cache_lookup().await.unwrap().is_none());
        assert_eq!(
            second.admission_decision(),
            Some(Decision::Ready { observed: 2 })
        );
        assert_eq!(second.phase(), CachePhase::CacheKey);
        second.cache_miss();
        assert_eq!(second.phase(), CachePhase::Miss);
    }

    #[tokio::test]
    async fn repeated_raw_miss_is_observed_once_per_request() {
        RAW_MISS_READY_POLICY.0.store(0, Ordering::Relaxed);
        let mut cache = HttpCache::new();
        cache.enable(&UPDATE_OK_STORAGE, None, None, None, None);
        cache.set_admission_policy(&RAW_MISS_READY_POLICY);
        cache.set_cache_key(CacheKey::new("repeated-ready-admission", ""));

        assert!(cache.cache_lookup().await.unwrap().is_none());
        assert!(cache.cache_lookup().await.unwrap().is_none());
        assert_eq!(RAW_MISS_READY_POLICY.0.load(Ordering::Relaxed), 1);
        assert_eq!(
            cache.admission_decision(),
            Some(Decision::Ready { observed: 1 })
        );
    }

    #[tokio::test]
    async fn stale_refill_does_not_observe_admission() {
        STALE_DEFER_POLICY.0.store(0, Ordering::Relaxed);
        let key = CacheKey::new("stale-refill-admission", "");
        let mut cache = HttpCache::new();
        cache.enable(&UPDATE_OK_STORAGE, None, None, None, None);
        cache.set_admission_policy(&STALE_DEFER_POLICY);
        cache.set_cache_key(key);
        cache.phase = CachePhase::Stale;
        cache.inner_enabled_mut().meta = Some(test_meta(SystemTime::now()));

        assert!(cache.cache_lookup().await.unwrap().is_none());
        assert_eq!(STALE_DEFER_POLICY.0.load(Ordering::Relaxed), 0);
        assert_eq!(cache.admission_decision(), None);
        cache.cache_miss();
        assert_eq!(cache.phase(), CachePhase::Miss);
    }

    #[tokio::test]
    async fn valid_after_rejection_does_not_observe_admission() {
        VALID_AFTER_DEFER_POLICY.0.store(0, Ordering::Relaxed);
        let created = SystemTime::UNIX_EPOCH + Duration::from_secs(100);
        let key = CacheKey::new("valid-after-not-admission", "");
        ONE_SHOT_LOOKUP_STORAGE
            .entries
            .lock()
            .unwrap()
            .push((key.to_compact(), test_meta(created)));

        let mut cache = HttpCache::new();
        cache.enable(&ONE_SHOT_LOOKUP_STORAGE, None, None, None, None);
        cache.set_admission_policy(&VALID_AFTER_DEFER_POLICY);
        cache.set_cache_key(key);
        cache.inner_enabled_mut().valid_after = Some(created + Duration::from_secs(1));

        assert!(cache.cache_lookup().await.unwrap().is_none());
        assert_eq!(cache.phase(), CachePhase::CacheKey);
        assert_eq!(cache.admission_decision(), None);
        assert_eq!(VALID_AFTER_DEFER_POLICY.0.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn test_set_cache_meta_preserves_stale_provenance() {
        let created = SystemTime::UNIX_EPOCH + Duration::from_secs(100);
        let family_start = SystemTime::UNIX_EPOCH + Duration::from_secs(80);
        let refresh = SystemTime::UNIX_EPOCH + Duration::from_secs(200);
        let variance = [1; 16];

        let mut old_meta = test_meta(created);
        old_meta.set_provenance(family_start);
        old_meta.set_variance_key(variance);
        let mut cache = cache_with_stale_meta(old_meta, CacheKey::new("preserve", ""));

        cache.set_cache_meta(test_meta(refresh));

        assert_eq!(cache.phase(), CachePhase::Expired);
        assert_eq!(cache.cache_meta().created(), refresh);
        assert_eq!(cache.cache_meta().provenance(), family_start);
        assert_eq!(cache.inner_enabled().stale_meta_variance, Some(variance));
    }

    #[tokio::test]
    async fn test_revalidate_cache_meta_preserves_created_and_provenance() {
        let created = SystemTime::UNIX_EPOCH + Duration::from_secs(100);
        let family_start = SystemTime::UNIX_EPOCH + Duration::from_secs(80);
        let revalidated_at = SystemTime::UNIX_EPOCH + Duration::from_secs(200);

        let mut old_meta = test_meta(created);
        old_meta.set_provenance(family_start);
        let mut cache = cache_with_stale_meta(old_meta, CacheKey::new("revalidate", ""));

        cache
            .revalidate_cache_meta(test_meta(revalidated_at))
            .await
            .unwrap();

        assert_eq!(cache.phase(), CachePhase::Revalidated);
        assert_eq!(cache.cache_meta().created(), created);
        assert_eq!(cache.cache_meta().updated(), revalidated_at);
        assert_eq!(cache.cache_meta().provenance(), family_start);
    }

    #[test]
    fn test_update_variance_preserves_provenance_when_primary_variance_unchanged() {
        let created = SystemTime::UNIX_EPOCH + Duration::from_secs(100);
        let family_start = SystemTime::UNIX_EPOCH + Duration::from_secs(80);
        let refresh = SystemTime::UNIX_EPOCH + Duration::from_secs(200);
        let variance = [1; 16];

        let mut old_meta = test_meta(created);
        old_meta.set_provenance(family_start);
        old_meta.set_variance_key(variance);
        let mut cache = cache_with_stale_meta(old_meta, CacheKey::new("same-vary", ""));

        cache.set_cache_meta(test_meta(refresh));
        cache.update_variance(Some(variance));

        assert_eq!(cache.cache_meta().provenance(), family_start);
        assert_eq!(cache.cache_meta().variance(), Some(variance));
        assert!(cache.cache_key().get_variance_key().is_none());
    }

    #[test]
    fn test_update_variance_resets_provenance_when_primary_variance_changes() {
        let created = SystemTime::UNIX_EPOCH + Duration::from_secs(100);
        let family_start = SystemTime::UNIX_EPOCH + Duration::from_secs(80);
        let refresh = SystemTime::UNIX_EPOCH + Duration::from_secs(200);
        let old_variance = [1; 16];
        let new_variance = [2; 16];

        let mut old_meta = test_meta(created);
        old_meta.set_provenance(family_start);
        old_meta.set_variance_key(old_variance);
        let mut cache = cache_with_stale_meta(old_meta, CacheKey::new("changed-vary", ""));

        cache.set_cache_meta(test_meta(refresh));
        cache.update_variance(Some(new_variance));

        assert_eq!(cache.cache_meta().provenance(), refresh);
        assert_eq!(cache.cache_meta().variance(), Some(new_variance));
        assert!(cache.cache_key().get_variance_key().is_none());
    }

    #[test]
    fn test_update_variance_resets_provenance_when_primary_variance_appears() {
        let created = SystemTime::UNIX_EPOCH + Duration::from_secs(100);
        let family_start = SystemTime::UNIX_EPOCH + Duration::from_secs(80);
        let refresh = SystemTime::UNIX_EPOCH + Duration::from_secs(200);
        let variance = [1; 16];

        let mut old_meta = test_meta(created);
        old_meta.set_provenance(family_start);
        let mut cache = cache_with_stale_meta(old_meta, CacheKey::new("vary-appears", ""));

        cache.set_cache_meta(test_meta(refresh));
        cache.update_variance(Some(variance));

        assert_eq!(cache.cache_meta().provenance(), refresh);
        assert_eq!(cache.cache_meta().variance(), Some(variance));
        assert!(cache.cache_key().get_variance_key().is_none());
    }

    #[test]
    fn test_update_variance_resets_provenance_when_secondary_takes_primary_slot() {
        let created = SystemTime::UNIX_EPOCH + Duration::from_secs(100);
        let family_start = SystemTime::UNIX_EPOCH + Duration::from_secs(80);
        let refresh = SystemTime::UNIX_EPOCH + Duration::from_secs(200);
        let old_variance = [1; 16];
        let mut key = CacheKey::new("secondary-takeover", "");
        key.set_variance_key(old_variance);

        let mut old_meta = test_meta(created);
        old_meta.set_provenance(family_start);
        old_meta.set_variance_key(old_variance);
        let mut cache = cache_with_stale_meta(old_meta, key);

        cache.set_cache_meta(test_meta(refresh));
        cache.update_variance(None);

        assert_eq!(cache.cache_meta().provenance(), refresh);
        assert!(cache.cache_meta().variance().is_none());
        assert!(cache.cache_key().get_variance_key().is_none());
    }

    #[test]
    fn test_update_variance_preserves_provenance_when_secondary_variance_unchanged() {
        let created = SystemTime::UNIX_EPOCH + Duration::from_secs(100);
        let family_start = SystemTime::UNIX_EPOCH + Duration::from_secs(80);
        let refresh = SystemTime::UNIX_EPOCH + Duration::from_secs(200);
        let variance = [1; 16];
        let mut key = CacheKey::new("secondary-same-vary", "");
        key.set_variance_key(variance);

        let mut old_meta = test_meta(created);
        old_meta.set_provenance(family_start);
        old_meta.set_variance_key(variance);
        let mut cache = cache_with_stale_meta(old_meta, key);

        cache.set_cache_meta(test_meta(refresh));
        cache.update_variance(Some(variance));

        assert_eq!(cache.cache_meta().provenance(), family_start);
        assert_eq!(cache.cache_meta().variance(), Some(variance));
        assert_eq!(cache.cache_key().get_variance_key(), Some(&variance));
    }

    #[tokio::test]
    async fn test_cache_vary_lookup_uses_provenance_for_valid_after() {
        let family_start = SystemTime::UNIX_EPOCH + Duration::from_secs(100);
        let secondary_created = SystemTime::UNIX_EPOCH + Duration::from_secs(150);
        let primary_refreshed = SystemTime::UNIX_EPOCH + Duration::from_secs(200);
        let primary_variance = [1; 16];
        let secondary_variance = [2; 16];

        let mut primary_meta = test_meta(primary_refreshed);
        primary_meta.set_provenance(family_start);
        primary_meta.set_variance_key(primary_variance);

        let mut secondary_meta = test_meta(secondary_created);
        secondary_meta.set_provenance(family_start);
        secondary_meta.set_variance_key(secondary_variance);

        let mut cache = cache_with_lookup_storage(CacheKey::new("valid-after-provenance", ""));
        assert!(!cache.cache_vary_lookup(secondary_variance, &primary_meta));
        assert_eq!(
            cache.inner_enabled().valid_after,
            Some(primary_meta.provenance())
        );

        let secondary_key = cache.cache_key().to_compact();
        ONE_SHOT_LOOKUP_STORAGE
            .entries
            .lock()
            .unwrap()
            .push((secondary_key, secondary_meta));

        assert!(cache.cache_lookup().await.unwrap().is_some());
    }
}
