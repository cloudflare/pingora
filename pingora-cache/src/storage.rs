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

//! Cache backend storage abstraction

use super::{CacheKey, CacheMeta};
use crate::eviction::{CacheEntryId, CacheEntryKey, CacheEntryKeyRef, EvictionManager};
use crate::fill_cancellation::CacheFillCancellationWatcher;
use crate::key::CompactCacheKey;
use crate::trace::SpanHandle;

use async_trait::async_trait;
use log::warn;
use pingora_error::Result;
use std::any::Any;
use std::fmt::{Display, Formatter, Result as FmtResult};
use std::time::SystemTime;
use tokio::runtime::Handle;

/// The reason a purge() is called
#[derive(Debug, Clone, Copy)]
pub enum PurgeType {
    // For eviction because the cache storage is full
    Eviction,
    // For cache invalidation
    Invalidation,
}

/// What a purge is asked to do to the entry it targets.
///
/// This is separate from [`PurgeType`], which says why the purge happened rather than what it
/// does to the entry, and from [`PurgeOutcome`], which is what storage actually did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PurgeAction {
    /// Remove the entry, so the next read for its key is a miss.
    Delete,
    /// Keep the entry but mark it stale, so it revalidates against the origin instead of being
    /// refetched, reusing the stored body when the origin answers 304.
    Expire,
}

/// The entry a [`Storage::purge`] call should remove.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PurgeTarget<'a> {
    /// Remove storage's current entry for this logical cache key.
    ///
    /// This targets one entry. Storage that retains multiple generations for a logical key does
    /// not need to remove inactive generations. Storage must not remove an entry with a different
    /// logical key. When storage retains multiple generations, it may select which identity to
    /// remove.
    Active(&'a CompactCacheKey),
    /// Remove this exact cache entry.
    ///
    /// Storage must match the complete identity, including any [`CacheEntryId`].
    Exact(&'a CacheEntryKey),
}

impl Display for PurgeTarget<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        match self {
            Self::Active(key) => write!(f, "active entry for {key}"),
            Self::Exact(entry) => write!(f, "{entry}"),
        }
    }
}

impl<'a> PurgeTarget<'a> {
    /// Return the target's logical cache key.
    pub fn key(self) -> &'a CompactCacheKey {
        match self {
            Self::Active(key) => key,
            Self::Exact(entry) => entry.key(),
        }
    }

    /// Return a borrowed identity for the removed entry.
    ///
    /// `id` supplies identity discovered while resolving a [`PurgeTarget::Active`] target. It is
    /// ignored for a [`PurgeTarget::Exact`] target, which already contains the complete identity;
    /// if supplied, it must match that identity.
    pub fn removed_entry(self, id: Option<CacheEntryId>) -> CacheEntryKeyRef<'a> {
        match self {
            Self::Active(key) => CacheEntryKeyRef::from_entry_id(key, id),
            Self::Exact(entry) => {
                debug_assert!(
                    id.is_none() || id == entry.entry_id(),
                    "purge outcome ID {id:?} must match exact target ID {:?}",
                    entry.entry_id()
                );
                entry.into()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_removed_entry_accepts_absent_or_matching_id() {
        let id = CacheEntryId::new(42);
        let entry = CacheEntryKey::identified(CompactCacheKey::default(), id);
        let target = PurgeTarget::Exact(&entry);

        assert_eq!(target.removed_entry(None), (&entry).into());
        assert_eq!(target.removed_entry(Some(id)), (&entry).into());
    }

    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "must match exact target ID")]
    fn exact_removed_entry_rejects_mismatched_id() {
        let entry = CacheEntryKey::identified(CompactCacheKey::default(), CacheEntryId::new(42));
        PurgeTarget::Exact(&entry).removed_entry(Some(CacheEntryId::new(43)));
    }
}

/// Outcome of a successful [`Storage::purge`] or [`Storage::expire`] call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PurgeOutcome {
    /// Storage did not find the target entry.
    NotFound,
    /// Storage removed an entry, with the ID selected for an active target, if any.
    ///
    /// Exact targets already contain the complete identity and need not repeat their ID here.
    Purged(Option<CacheEntryId>),
    /// Storage kept the entry and marked it stale.
    ///
    /// Only [`Storage::expire`] returns this. The entry is still stored, so the eviction manager
    /// keeps tracking it.
    Expired,
}

/// Cache storage interface
#[async_trait]
pub trait Storage {
    // TODO: shouldn't have to be static

    /// Lookup the storage for the given [CacheKey].
    async fn lookup(
        &'static self,
        key: &CacheKey,
        trace: &SpanHandle,
    ) -> Result<Option<(CacheMeta, HitHandler)>>;

    /// Lookup the storage for the given [CacheKey] using a streaming write tag.
    ///
    /// When streaming partial writes is supported, the request that initiates the write will also
    /// pass an optional `streaming_write_tag` so that the storage may try to find the associated
    /// [HitHandler], for the same ongoing write.
    ///
    /// Therefore, when the write tag is set, the storage implementation should either return a
    /// [HitHandler] that can be matched to that tag, or none at all. Otherwise when the storage
    /// supports concurrent streaming writes for the same key, the calling request may receive a
    /// different body from the one it expected.
    ///
    /// By default this defers to the standard `Storage::lookup` implementation.
    async fn lookup_streaming_write(
        &'static self,
        key: &CacheKey,
        _streaming_write_tag: Option<&[u8]>,
        trace: &SpanHandle,
    ) -> Result<Option<(CacheMeta, HitHandler)>> {
        self.lookup(key, trace).await
    }

    /// Write the given [CacheMeta] to the storage. Return [MissHandler] to write the body later.
    async fn get_miss_handler(
        &'static self,
        key: &CacheKey,
        meta: &CacheMeta,
        trace: &SpanHandle,
    ) -> Result<MissHandler>;

    /// Delete one cached entry for the given target.
    ///
    /// [`PurgeTarget::Active`] asks storage to select one active entry for a logical cache key; it
    /// does not request removal of every retained generation. [`PurgeTarget::Exact`] identifies the
    /// exact entry to remove.
    ///
    /// When resolving an active target, storage must return the storage-defined ID of the entry it
    /// actually removed in [`PurgeOutcome::Purged`]. If [`HandleHit::entry_id`] or
    /// [`HandleMiss::entry_id`] returns `Some`, an active purge outcome must contain that
    /// [`CacheEntryId`]. Returning `None` would leave the identified entry tracked by the eviction
    /// manager. Exact targets already contain the complete identity.
    async fn purge(
        &'static self,
        target: PurgeTarget<'_>,
        purge_type: PurgeType,
        trace: &SpanHandle,
    ) -> Result<PurgeOutcome>;

    /// Mark one cached entry stale so it revalidates, keeping the entry stored.
    ///
    /// Targets resolve the same way as [`Storage::purge`]. Once this returns
    /// [`PurgeOutcome::Expired`], the entry that was stored when the call ran must not be served
    /// as a fresh hit, and its body should stay usable so a revalidation can reuse it on a 304.
    ///
    /// Stale is not the same as unusable. Whether a reader is served the stale body while it
    /// revalidates is up to the entry's serve stale windows, which this does not touch.
    /// How that is recorded is up to storage: rewriting the stored freshness and applying it on
    /// read both satisfy the contract.
    ///
    /// This is deliberately not [`Storage::update_meta`]. That call needs a full [`CacheKey`] and
    /// a [`CacheMeta`] the caller already holds from a hit, and a purge has neither. It would also
    /// pin the recording strategy to rewriting the stored meta, turning this into a read modify
    /// write that a concurrent revalidation can clobber.
    ///
    /// The guarantee covers the stored entry, not a fill already in flight. A miss handler that
    /// commits after this returns may replace the entry with a fresh one, which is the same race
    /// [`Storage::purge`] has and is expected to be resolved by whatever serializes writes for
    /// that key.
    ///
    /// The default deletes the entry instead. Storage that cannot mark an entry stale must still
    /// keep the next read from serving it, and deleting gives that guarantee at the cost of a
    /// full refetch. It reports [`PurgeOutcome::Purged`] so the caller knows the entry is gone
    /// and the eviction manager stops tracking it.
    async fn expire(
        &'static self,
        target: PurgeTarget<'_>,
        trace: &SpanHandle,
    ) -> Result<PurgeOutcome> {
        self.purge(target, PurgeType::Invalidation, trace).await
    }

    /// Update cache header and metadata for the already stored asset.
    async fn update_meta(
        &'static self,
        key: &CacheKey,
        meta: &CacheMeta,
        trace: &SpanHandle,
    ) -> Result<bool>;

    /// Whether this storage backend supports reading partially written data
    ///
    /// This is to indicate when cache should unlock readers
    fn support_streaming_partial_write(&self) -> bool {
        false
    }

    /// Helper function to cast the trait object to concrete types
    fn as_any(&self) -> &(dyn Any + Send + Sync + 'static);
}

/// Cache hit handling trait
#[async_trait]
pub trait HandleHit {
    /// Read cached body
    ///
    /// Return `None` when no more body to read.
    ///
    /// A storage whose fills can be cancelled, see [`HandleMiss::cancellation_watcher`], should
    /// return `None` only once the whole body has been read, and an error to a reader of a write
    /// that was dropped before [`HandleMiss::finish`].
    async fn read_body(&mut self) -> Result<Option<bytes::Bytes>>;

    /// Finish the current cache hit
    async fn finish(
        self: Box<Self>, // because self is always used as a trait object
        storage: &'static (dyn Storage + Sync),
        key: &CacheKey,
        trace: &SpanHandle,
    ) -> Result<()>;

    /// Whether this storage allows seeking to a certain range of body for single ranges.
    fn can_seek(&self) -> bool {
        false
    }

    /// Whether this storage allows seeking to a certain range of body for multipart ranges.
    ///
    /// By default uses the `can_seek` implementation.
    fn can_seek_multipart(&self) -> bool {
        self.can_seek()
    }

    /// Try to seek to a certain range of the body for single ranges.
    ///
    /// `end: None` means to read to the end of the body.
    fn seek(&mut self, _start: usize, _end: Option<usize>) -> Result<()> {
        // to prevent impl can_seek() without impl seek
        todo!("seek() needs to be implemented")
    }

    /// Try to seek to a certain range of the body for multipart ranges.
    ///
    /// Works in an identical manner to `seek()`.
    ///
    /// `end: None` means to read to the end of the body.
    ///
    /// By default uses the `seek` implementation, but hit handlers may customize the
    /// implementation specifically to anticipate multipart requests.
    fn seek_multipart(&mut self, start: usize, end: Option<usize>) -> Result<()> {
        // to prevent impl can_seek() without impl seek
        self.seek(start, end)
    }

    // TODO: fn is_stream_hit()

    /// Should we count this hit handler instance as an access in the eviction manager.
    ///
    /// Defaults to returning true to track all cache hits as accesses. Customize this if certain
    /// hits should not affect the eviction system's view of the asset.
    fn should_count_access(&self) -> bool {
        true
    }

    /// Returns the weight of the current cache hit asset to report to the eviction manager.
    ///
    /// This allows the eviction system to initialize a weight for the asset, in case it is not
    /// already tracking it (e.g. storage is out of sync with the eviction manager).
    ///
    /// Defaults to 0.
    fn get_eviction_weight(&self) -> usize {
        0
    }

    /// Return the identity of this entry to the eviction manager.
    ///
    /// Storage that identifies a stored generation beyond its
    /// [`crate::key::CompactCacheKey`] can return that identity here so access accounting targets
    /// the exact entry. Storage that keys eviction only on the cache key should leave this at the
    /// default `None`.
    ///
    /// Storage that identifies entries must implement both this method and
    /// [`HandleMiss::entry_id`] using the same identity scheme.
    fn entry_id(&self) -> Option<CacheEntryId> {
        None
    }

    /// Helper function to cast the trait object to concrete types
    fn as_any(&self) -> &(dyn Any + Send + Sync);

    /// Helper function to cast the trait object to concrete types
    fn as_any_mut(&mut self) -> &mut (dyn Any + Send + Sync);
}

/// Hit Handler
pub type HitHandler = Box<dyn HandleHit + Sync + Send>;

/// MissFinishType
pub enum MissFinishType {
    /// A new asset was created with the given size.
    Created(usize),
    /// Appended size to existing asset, with an optional max size param.
    Appended(usize, Option<usize>),
}

/// A write's eviction accounting, which the caller prepares before it finishes the write and
/// [`MissCompletion::complete`] applies once the write succeeds.
///
/// [`HandleMiss::finish_with_eviction`] receives it. Storage that overrides that method can move
/// it into the worker that finishes the write, so the write's accounting has one owner whether or
/// not the caller is still awaiting the write. It is not `Clone`, and `complete` consumes it, so a
/// write is accounted at most once. A successful `finish_with_eviction` returns the
/// [`MissCompleted`] that only `complete` creates, so an override cannot report success without
/// having applied it.
#[must_use = "a successful write is accounted only by calling `complete`"]
pub struct MissCompletion {
    item: CacheEntryKey,
    fresh_until: SystemTime,
    eviction: Option<&'static (dyn EvictionManager + Sync)>,
    storage: &'static (dyn Storage + Sync),
    trace: SpanHandle,
    trace_name: &'static str,
    /// The runtime that purges evicted entries, when [`Self::new`] ran on one with an eviction
    /// manager to purge for.
    executor: Option<Handle>,
}

impl MissCompletion {
    /// Prepare the accounting for a write of `item`, an entry fresh until `fresh_until`.
    ///
    /// Without an `eviction` manager, [`Self::complete`] does nothing. With one, it purges the
    /// entries the write evicts from `storage`, under a span named `trace_name` that starts as a
    /// child of `trace` when the write completes. The purges run on the Tokio runtime current
    /// here, so `complete` can run on a thread outside it, such as a blocking worker. When this
    /// runs outside a runtime, they run on the one current where `complete` runs.
    pub fn new(
        item: CacheEntryKey,
        fresh_until: SystemTime,
        eviction: Option<&'static (dyn EvictionManager + Sync)>,
        storage: &'static (dyn Storage + Sync),
        trace: SpanHandle,
        trace_name: &'static str,
    ) -> Self {
        MissCompletion {
            item,
            fresh_until,
            eviction,
            storage,
            trace,
            trace_name,
            executor: eviction.and_then(|_| Handle::try_current().ok()),
        }
    }

    /// Apply a successful write to the eviction manager: admit a created entry at its size, or
    /// grow an appended one by its delta, capped by its max size. The entries this evicts are
    /// purged from storage in the background, each as a [`PurgeType::Eviction`] of that exact
    /// entry.
    ///
    /// This neither blocks nor awaits, so storage can call it from whatever worker finishes the
    /// write, a blocking thread included.
    ///
    /// Returns the receipt that a successful [`HandleMiss::finish_with_eviction`] returns.
    ///
    /// # Panics
    ///
    /// Panics when there is an eviction manager and neither [`Self::new`] nor this call ran
    /// inside a Tokio runtime.
    pub fn complete(self, finish: MissFinishType) -> MissCompleted {
        let completed = MissCompleted { _private: () };
        let Some(eviction) = self.eviction else {
            return completed;
        };
        let executor = self.executor.unwrap_or_else(Handle::current);
        let evicted = match finish {
            MissFinishType::Created(size) => eviction.admit(self.item, size, self.fresh_until),
            MissFinishType::Appended(delta, max_weight) => {
                eviction.increment_weight(&self.item, delta, max_weight)
            }
        };
        // actual eviction can be done async
        let span = self.trace.child(self.trace_name, |o| o.start());
        let handle = span.handle();
        let storage = self.storage;
        let trace_name = self.trace_name;
        executor.spawn(async move {
            for item in evicted {
                let target = PurgeTarget::Exact(&item);
                if let Err(e) = storage.purge(target, PurgeType::Eviction, &handle).await {
                    warn!("Failed to purge {target} during {trace_name}: {e}");
                }
            }
        });
        completed
    }
}

/// Proof that a write's [`MissCompletion`] was applied.
///
/// Only [`MissCompletion::complete`] creates one, and a successful
/// [`HandleMiss::finish_with_eviction`] must return it, so an override that drops its completion
/// cannot also report success. Storage cannot make one any other way:
///
/// ```compile_fail
/// let forged = pingora_cache::storage::MissCompleted { _private: () };
/// ```
#[derive(Debug)]
pub struct MissCompleted {
    _private: (),
}

/// Cache miss handling trait
#[async_trait]
pub trait HandleMiss {
    /// Write the given body to the storage
    async fn write_body(&mut self, data: bytes::Bytes, eof: bool) -> Result<()>;

    /// Finish the cache admission
    ///
    /// When `self` is dropped before this function or [`Self::finish_with_eviction`] finishes the
    /// write, the storage should consider this write failed.
    ///
    /// The proxy drops the miss handler of a cancelled fill without finishing it, and
    /// takes `Ok(None)` from [`HandleHit::read_body`] as the end of a complete body. A storage
    /// whose fills can be cancelled should therefore give the hit handlers reading this write,
    /// whether from [`Storage::lookup_streaming_write`] or from a lookup that found it in
    /// progress, an error rather than `Ok(None)` once it is dropped unfinished, so they do not
    /// serve the truncated body as a success. [`MemCache`](crate::MemCache) does so when built
    /// with [`MemCache::fail_readers_of_dropped_writes`](crate::MemCache::fail_readers_of_dropped_writes).
    async fn finish(
        self: Box<Self>, // because self is always used as a trait object
    ) -> Result<MissFinishType>;

    /// Finish the cache admission, as [`Self::finish`] does, and apply the finished write to
    /// eviction through `completion`.
    ///
    /// The cache finishes writes through this method. The default calls `finish`, then
    /// [`MissCompletion::complete`] with its result, with no await in between, so dropping the
    /// caller's future cannot separate a finished write from its accounting. A failed `finish`
    /// drops `completion` without applying it.
    ///
    /// The [`MissCompleted`] a success returns can only come from `complete`, so the compiler
    /// rejects an implementation that reports success without applying `completion`.
    ///
    /// Storage that corrects eviction weights out of band, for example from a background size
    /// scan, can override this to keep those corrections out of the interval between a write's
    /// completion and its accounting: move `completion` into the worker that finishes the write,
    /// and call `complete` there before releasing the write's own active-write state. An override
    /// returns the receipt from `complete` when the write succeeds, and drops `completion` without
    /// calling it when the write fails. Such a worker can outlive the caller's future and still
    /// complete the write's accounting after the caller is gone.
    ///
    /// Callers that drop a miss handler without finishing it, as the proxy does for a cancelled
    /// fill, never call this, so storage must not expect a completion for that write.
    ///
    /// A handler that wraps another handler must forward this method as well as `finish`:
    /// inheriting the default would apply the accounting outside the wrapped storage's write.
    async fn finish_with_eviction(
        self: Box<Self>,
        completion: MissCompletion,
    ) -> Result<MissCompleted> {
        let finish = self.finish().await?;
        Ok(completion.complete(finish))
    }

    /// Return a streaming write tag recognized by the underlying [`Storage`].
    ///
    /// This is an arbitrary data identifier that is used to associate this miss handler's current
    /// write with a hit handler for the same write. This identifier will be compared by the
    /// storage during `lookup_streaming_write`.
    // This write tag is essentially an borrowed data blob of bytes retrieved from the miss handler
    // and passed to storage, which means it can support strings or small data types, e.g. bytes
    // represented by a u64.
    // The downside with the current API is that such a data blob must be owned by the miss handler
    // and stored in a way that permits retrieval as a byte slice (not computed on the fly).
    // But most use cases likely only require a simple integer and may not like the overhead of a
    // Vec/String allocation or even a Cow, though such data types can also be used here.
    fn streaming_write_tag(&self) -> Option<&[u8]> {
        None
    }

    /// Release this writer's demand for an in-progress streaming fill, returning `true` only if
    /// this call released it.
    ///
    /// A storage that shares one fill among concurrent readers can count each party that still
    /// wants the object: the request that writes it, plus every reader attached to the write.
    /// The writer's demand is that request's share, which an application drops through
    /// [`crate::HttpCache::release_miss_writer_demand`] once the request's client has left.
    /// Storage that tracks demand should make the release idempotent, so that a later call
    /// returns `false`. Other backends return `false`.
    fn release_writer_demand(&self) -> bool {
        false
    }

    /// Return a watcher that resolves once this fill should be cancelled.
    ///
    /// The proxy polls the watcher only after the filling request's downstream has been
    /// abandoned; see [`CacheFillCancellation`](crate::fill_cancellation::CacheFillCancellation)
    /// for what a resolved watcher makes it do and for how long one value stays in force.
    /// Backends that let every fill run until the upstream completes leave this at the default
    /// `None`.
    ///
    /// The [`CacheFillCancellation`](crate::fill_cancellation::CacheFillCancellation) belongs to
    /// the state shared by everyone using one logical fill, and exists before any miss handler
    /// does. The paths that release demand call `cancel()` on it, and every miss handler of
    /// that fill, across upstream attempts, returns a watcher of the same value. It is neither
    /// installed from the downstream-abandonment callback, nor keyed globally by cache key, nor
    /// recreated per upstream attempt.
    ///
    /// In the example below, an application hook that sees the downstream abandoned reaches
    /// `FillState::release_writer` by calling
    /// [`HttpCache::release_miss_writer_demand`](crate::HttpCache::release_miss_writer_demand),
    /// which calls [`Self::release_writer_demand`] on the active miss handler.
    ///
    /// ```no_run
    /// use std::sync::Arc;
    ///
    /// use async_trait::async_trait;
    /// use parking_lot::Mutex;
    /// use pingora_cache::storage::{HandleMiss, MissFinishType};
    /// use pingora_cache::{CacheFillCancellation, CacheFillCancellationWatcher};
    /// use pingora_error::Result;
    ///
    /// /// Demand for one logical fill: how many readers are attached, whether the filling
    /// /// request still wants it, and whether admission of new readers has closed.
    /// struct Demand {
    ///     readers: usize,
    ///     writer: bool,
    ///     closed: bool,
    /// }
    ///
    /// /// Shared by every reader and writer of one logical fill.
    /// struct FillState {
    ///     demand: Mutex<Demand>,
    ///     cancellation: CacheFillCancellation,
    /// }
    ///
    /// impl FillState {
    ///     /// A fill starts with the filling request's own writer demand held.
    ///     fn new() -> Self {
    ///         Self {
    ///             demand: Mutex::new(Demand {
    ///                 readers: 0,
    ///                 writer: true,
    ///                 closed: false,
    ///             }),
    ///             cancellation: CacheFillCancellation::new(),
    ///         }
    ///     }
    ///
    ///     /// Admit a new reader. Returns `false` once admission has closed, in which
    ///     /// case the caller must not attach to this fill.
    ///     fn admit_reader(&self) -> bool {
    ///         let mut demand = self.demand.lock();
    ///         if demand.closed {
    ///             return false;
    ///         }
    ///         demand.readers += 1;
    ///         true
    ///     }
    ///
    ///     /// Release a reader's demand.
    ///     fn release_reader(&self) {
    ///         let mut demand = self.demand.lock();
    ///         demand.readers -= 1;
    ///         self.close_if_idle(&mut demand);
    ///     }
    ///
    ///     /// Release the filling request's own demand, leaving any readers attached.
    ///     /// Repeat calls are harmless and report that there was nothing left to release.
    ///     fn release_writer(&self) -> bool {
    ///         let mut demand = self.demand.lock();
    ///         if !demand.writer {
    ///             return false;
    ///         }
    ///         demand.writer = false;
    ///         self.close_if_idle(&mut demand);
    ///         true
    ///     }
    ///
    ///     /// The last demand to leave closes admission and cancels the fill under the same
    ///     /// lock that counted it: `cancel()` is final, so a reader admitted after admission
    ///     /// closes would attach to a fill that never resumes.
    ///     fn close_if_idle(&self, demand: &mut Demand) {
    ///         if demand.readers == 0 && !demand.writer {
    ///             demand.closed = true;
    ///             self.cancellation.cancel();
    ///         }
    ///     }
    /// }
    ///
    /// struct FillWriter {
    ///     fill: Arc<FillState>,
    /// }
    ///
    /// #[async_trait]
    /// impl HandleMiss for FillWriter {
    ///     async fn write_body(&mut self, _data: bytes::Bytes, _eof: bool) -> Result<()> {
    ///         Ok(())
    ///     }
    ///
    ///     async fn finish(self: Box<Self>) -> Result<MissFinishType> {
    ///         Ok(MissFinishType::Created(0))
    ///     }
    ///
    ///     fn cancellation_watcher(&self) -> Option<CacheFillCancellationWatcher> {
    ///         // Every writer of this fill watches the same value.
    ///         Some(self.fill.cancellation.watcher())
    ///     }
    ///
    ///     fn release_writer_demand(&self) -> bool {
    ///         // The fill outlives this writer whenever readers remain attached.
    ///         self.fill.release_writer()
    ///     }
    /// }
    /// ```
    fn cancellation_watcher(&self) -> Option<CacheFillCancellationWatcher> {
        None
    }

    /// Return the identity of the entry produced by this write to the eviction manager.
    ///
    /// Storage that identifies a stored generation beyond its
    /// [`crate::key::CompactCacheKey`] should return that identity here so admission and weight
    /// updates target the entry that storage will produce. Storage that keys eviction only on the
    /// cache key should leave this at the default `None`.
    ///
    /// The value must identify the committed entry and remain valid after [`Self::finish`]. It
    /// must not identify only temporary write state. Storage that identifies entries must implement
    /// both this method and [`HandleHit::entry_id`] using the same identity scheme.
    fn entry_id(&self) -> Option<CacheEntryId> {
        None
    }
}

/// Miss Handler
pub type MissHandler = Box<dyn HandleMiss + Sync + Send>;

pub mod streaming_write {
    /// Portable u64 (sized) write id convenience type for use with streaming writes.
    ///
    /// Often an integer value is sufficient for a streaming write tag. This convenience type enables
    /// storing such a value and functions for consistent conversion between byte sequence data types.
    #[derive(Debug, Clone, Copy)]
    pub struct U64WriteId([u8; 8]);

    impl U64WriteId {
        pub fn as_bytes(&self) -> &[u8] {
            &self.0[..]
        }
    }

    impl From<u64> for U64WriteId {
        fn from(value: u64) -> U64WriteId {
            U64WriteId(value.to_be_bytes())
        }
    }
    impl From<U64WriteId> for u64 {
        fn from(value: U64WriteId) -> u64 {
            u64::from_be_bytes(value.0)
        }
    }
    impl TryFrom<&[u8]> for U64WriteId {
        type Error = std::array::TryFromSliceError;

        fn try_from(value: &[u8]) -> std::result::Result<Self, Self::Error> {
            Ok(U64WriteId(value.try_into()?))
        }
    }

    /// Portable u32 (sized) write id convenience type for use with streaming writes.
    ///
    /// Often an integer value is sufficient for a streaming write tag. This convenience type enables
    /// storing such a value and functions for consistent conversion between byte sequence data types.
    #[derive(Debug, Clone, Copy)]
    pub struct U32WriteId([u8; 4]);

    impl U32WriteId {
        pub fn as_bytes(&self) -> &[u8] {
            &self.0[..]
        }
    }

    impl From<u32> for U32WriteId {
        fn from(value: u32) -> U32WriteId {
            U32WriteId(value.to_be_bytes())
        }
    }
    impl From<U32WriteId> for u32 {
        fn from(value: U32WriteId) -> u32 {
            u32::from_be_bytes(value.0)
        }
    }
    impl TryFrom<&[u8]> for U32WriteId {
        type Error = std::array::TryFromSliceError;

        fn try_from(value: &[u8]) -> std::result::Result<Self, Self::Error> {
            Ok(U32WriteId(value.try_into()?))
        }
    }
}
