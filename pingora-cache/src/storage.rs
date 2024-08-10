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

    /// Lookup the storage for the given [CacheKey]
    async fn lookup(
        &'static self,
        key: &CacheKey,
        trace: &SpanHandle,
    ) -> Result<Option<(CacheMeta, HitHandler)>>;

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

    /// Helper function to cast the trait object to concrete types
    fn as_any(&self) -> &(dyn Any + Send + Sync);
}

/// Hit Handler
pub type HitHandler = Box<(dyn HandleHit + Sync + Send)>;

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
    ) -> Result<usize>;
}

/// Miss Handler
pub type MissHandler = Box<(dyn HandleMiss + Sync + Send)>;
