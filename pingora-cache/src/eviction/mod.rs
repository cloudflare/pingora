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

//! Cache eviction module

use crate::key::CompactCacheKey;

use async_trait::async_trait;
use pingora_error::Result;
use std::time::SystemTime;

pub mod lru;
pub mod simple_lru;

/// The trait that a cache eviction algorithm needs to implement
///
/// NOTE: these trait methods require &self not &mut self, which means concurrency should
/// be handled the implementations internally.
#[async_trait]
pub trait EvictionManager {
    /// Total size of the cache in bytes tracked by this eviction manager
    fn total_size(&self) -> usize;
    /// Number of assets tracked by this eviction manager
    fn total_items(&self) -> usize;
    /// Number of bytes that are already evicted
    ///
    /// The accumulated number is returned to play well with Prometheus counter metric type.
    fn evicted_size(&self) -> usize;
    /// Number of assets that are already evicted
    ///
    /// The accumulated number is returned to play well with Prometheus counter metric type.
    fn evicted_items(&self) -> usize;

    /// Admit an item
    ///
    /// Return one or more items to evict. The sizes of these items are deducted
    /// from the total size already. The caller needs to make sure that these assets are actually
    /// removed from the storage.
    ///
    /// If the item is already admitted, A. update its freshness; B. if the new size is larger than the
    /// existing one, Some(_) might be returned for the caller to evict.
    fn admit(
        &self,
        item: CompactCacheKey,
        size: usize,
        fresh_until: SystemTime,
    ) -> Vec<CompactCacheKey>;

    /// Adjust an item's weight upwards by a delta. If the item is not already admitted,
    /// nothing will happen.
    ///
    /// Return one or more items to evict. The sizes of these items are deducted
    /// from the total size already. The caller needs to make sure that these assets are actually
    /// removed from the storage.
    fn increment_weight(&self, item: CompactCacheKey, delta: usize) -> Vec<CompactCacheKey>;

    /// Remove an item from the eviction manager.
    ///
    /// The size of the item will be deducted.
    fn remove(&self, item: &CompactCacheKey);

    /// Access an item that should already be in cache.
    ///
    /// If the item is not tracked by this [EvictionManager], track it but no eviction will happen.
    ///
    /// The call used for asking the eviction manager to track the assets that are already admitted
    /// in the cache storage system.
    fn access(&self, item: &CompactCacheKey, size: usize, fresh_until: SystemTime) -> bool;

    /// Peek into the manager to see if the item is already tracked by the system
    ///
    /// This function should have no side-effect on the asset itself. For example, for LRU, this
    /// method shouldn't change the popularity of the asset being peeked.
    fn peek(&self, item: &CompactCacheKey) -> bool;

    /// Serialize to save the state of this eviction manager to disk
    ///
    /// This function is for preserving the eviction manager's state across server restarts.
    ///
    /// `dir_path` define the directory on disk that the data should use.
    // dir_path is &str no AsRef<Path> so that trait objects can be used
    async fn save(&self, dir_path: &str) -> Result<()>;

    /// The counterpart of [Self::save()].
    async fn load(&self, dir_path: &str) -> Result<()>;
}
