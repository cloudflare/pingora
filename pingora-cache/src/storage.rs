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

//! Cache backend storage abstraction

use super::{CacheKey, CacheMeta};
use crate::key::CompactCacheKey;
use crate::trace::SpanHandle;

use async_trait::async_trait;
use pingora_error::Result;
use std::any::Any;

/// The reason a purge() is called
#[derive(Debug, Clone, Copy)]
pub enum PurgeType {
    // For eviction because the cache storage is full
    Eviction,
    // For cache invalidation
    Invalidation,
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

    /// Delete the cached asset for the given key
    ///
    /// [CompactCacheKey] is used here because it is how eviction managers store the keys
    async fn purge(
        &'static self,
        key: &CompactCacheKey,
        purge_type: PurgeType,
        trace: &SpanHandle,
    ) -> Result<bool>;

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

    /// Whether this storage allow seeking to a certain range of body
    fn can_seek(&self) -> bool {
        false
    }

    /// Try to seek to a certain range of the body
    ///
    /// `end: None` means to read to the end of the body.
    fn seek(&mut self, _start: usize, _end: Option<usize>) -> Result<()> {
        // to prevent impl can_seek() without impl seek
        todo!("seek() needs to be implemented")
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

    /// Helper function to cast the trait object to concrete types
    fn as_any(&self) -> &(dyn Any + Send + Sync);
}

/// Hit Handler
pub type HitHandler = Box<(dyn HandleHit + Sync + Send)>;

/// MissFinishType
pub enum MissFinishType {
    Created(usize),
    Appended(usize),
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
}

/// Miss Handler
pub type MissHandler = Box<(dyn HandleMiss + Sync + Send)>;

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
