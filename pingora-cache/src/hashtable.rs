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

//! Concurrent hash tables and LRUs

use lru::LruCache;
use parking_lot::{RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::collections::HashMap;

// There are probably off-the-shelf crates of this, DashMap?
/// A hash table that shards to a constant number of tables to reduce lock contention
pub struct ConcurrentHashTable<V, const N: usize> {
    tables: [RwLock<HashMap<u128, V>>; N],
}

#[inline]
fn get_shard(key: u128, n_shards: usize) -> usize {
    (key % n_shards as u128) as usize
}

impl<V, const N: usize> ConcurrentHashTable<V, N>
where
    [RwLock<HashMap<u128, V>>; N]: Default,
{
    pub fn new() -> Self {
        ConcurrentHashTable {
            tables: Default::default(),
        }
    }
    pub fn get(&self, key: u128) -> &RwLock<HashMap<u128, V>> {
        &self.tables[get_shard(key, N)]
    }

    #[allow(dead_code)]
    pub fn read(&self, key: u128) -> RwLockReadGuard<HashMap<u128, V>> {
        self.get(key).read()
    }

    pub fn write(&self, key: u128) -> RwLockWriteGuard<HashMap<u128, V>> {
        self.get(key).write()
    }

    // TODO: work out the lifetimes to provide get/set directly
}

impl<V, const N: usize> Default for ConcurrentHashTable<V, N>
where
    [RwLock<HashMap<u128, V>>; N]: Default,
{
    fn default() -> Self {
        Self::new()
    }
}

#[doc(hidden)] // not need in public API
pub struct LruShard<V>(RwLock<LruCache<u128, V>>);
impl<V> Default for LruShard<V> {
    fn default() -> Self {
        // help satisfy default construction of arrays
        LruShard(RwLock::new(LruCache::unbounded()))
    }
}

/// Sharded concurrent data structure for LruCache
pub struct ConcurrentLruCache<V, const N: usize> {
    lrus: [LruShard<V>; N],
}

impl<V, const N: usize> ConcurrentLruCache<V, N>
where
    [LruShard<V>; N]: Default,
{
    pub fn new(shard_capacity: usize) -> Self {
        use std::num::NonZeroUsize;
        // safe, 1 != 0
        const ONE: NonZeroUsize = unsafe { NonZeroUsize::new_unchecked(1) };
        let mut cache = ConcurrentLruCache {
            lrus: Default::default(),
        };
        for lru in &mut cache.lrus {
            lru.0
                .write()
                .resize(shard_capacity.try_into().unwrap_or(ONE));
        }
        cache
    }
    pub fn get(&self, key: u128) -> &RwLock<LruCache<u128, V>> {
        &self.lrus[get_shard(key, N)].0
    }

    #[allow(dead_code)]
    pub fn read(&self, key: u128) -> RwLockReadGuard<LruCache<u128, V>> {
        self.get(key).read()
    }

    pub fn write(&self, key: u128) -> RwLockWriteGuard<LruCache<u128, V>> {
        self.get(key).write()
    }

    // TODO: work out the lifetimes to provide get/set directly
}
