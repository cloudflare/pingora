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
use crate::eviction::{CacheEntryId, CacheEntryKey, CacheEntryKeyRef};
use crate::key::CompactCacheKey;
use crate::trace::SpanHandle;

use async_trait::async_trait;
use pingora_error::Result;
use std::any::Any;
use std::fmt::{Display, Formatter, Result as FmtResult};

/// The reason a purge() is called
#[derive(Debug, Clone, Copy)]
pub enum PurgeType {
    // For eviction because the cache storage is full
    Eviction,
    // For cache invalidation
    Invalidation,
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

/// Outcome of a successful [`Storage::purge`] call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PurgeOutcome {
    /// Storage did not find the target entry.
    NotFound,
    /// Storage removed an entry, with the ID selected for an active target, if any.
    ///
    /// Exact targets already contain the complete identity and need not repeat their ID here.
    Purged(Option<CacheEntryId>),
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

/// Cache miss handling trait
#[async_trait]
pub trait HandleMiss {
    /// Write the given body to the storage
    async fn write_body(&mut self, data: bytes::Bytes, eof: bool) -> Result<()>;

    /// Finish the cache admission
    ///
    /// When `self` is dropped without calling this function, the storage should consider this write
    /// failed.
    async fn finish(
        self: Box<Self>, // because self is always used as a trait object
    ) -> Result<MissFinishType>;

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
