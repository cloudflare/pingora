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

//! Concurrent hash tables and LRUs

use lru::LruCache;
use parking_lot::{RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::collections::HashMap;

// There are probably off-the-shelf crates of this, DashMap?
/// A hash table that shards to a constant number of tables to reduce lock contention
#[derive(Debug)]
pub struct ConcurrentHashTable<V, const N: usize> {
    tables: [RwLock<HashMap<u128, V>>; N],
}

#[inline]
fn get_shard(key: u128, n_shards: usize) -> usize {
    (key % n_shards as u128) as usize
}

impl<V, const N: usize> ConcurrentHashTable<V, N> {
    pub fn new() -> Self {
        // Build the per-shard array element-by-element via `arrayvec`. The
        // stdlib only auto-derives `Default` for `[T; N]` up to N=32, so the
        // previous `Default::default()`-based init silently capped this type
        // at 32 shards. Mirrors the same `arrayvec::ArrayVec` pattern that
        // `pingora_lru::Lru<T, N>::with_capacity_and_watermark` uses to lift
        // the same constraint, so the two sharded structures now support
        // identical shard counts (callers can pick any `N`).
        let mut tables = arrayvec::ArrayVec::<_, N>::new();
        for _ in 0..N {
            tables.push(RwLock::new(HashMap::new()));
        }
        ConcurrentHashTable {
            // `into_inner` is infallible here because the loop above pushed
            // exactly N elements. `.ok().expect(...)` avoids requiring the
            // element type to be `Debug` (which `into_inner`'s `Err` payload
            // would otherwise demand for `.expect`).
            tables: tables
                .into_inner()
                .ok()
                .expect("ArrayVec pushed N times, into_inner is infallible"),
        }
    }
    pub fn get(&self, key: u128) -> &RwLock<HashMap<u128, V>> {
        &self.tables[get_shard(key, N)]
    }

    #[allow(dead_code)]
    pub fn get_shard_at_idx(&self, idx: usize) -> Option<&RwLock<HashMap<u128, V>>> {
        self.tables.get(idx)
    }

    #[allow(dead_code)]
    pub fn read(&self, key: u128) -> RwLockReadGuard<'_, HashMap<u128, V>> {
        self.get(key).read()
    }

    pub fn write(&self, key: u128) -> RwLockWriteGuard<'_, HashMap<u128, V>> {
        self.get(key).write()
    }

    #[allow(dead_code)]
    pub fn for_each<F>(&self, mut f: F)
    where
        F: FnMut(&u128, &V),
    {
        for shard in &self.tables {
            let guard = shard.read();
            for (key, value) in guard.iter() {
                f(key, value);
            }
        }
    }

    // TODO: work out the lifetimes to provide get/set directly
}

impl<V, const N: usize> Default for ConcurrentHashTable<V, N> {
    fn default() -> Self {
        Self::new()
    }
}

#[doc(hidden)] // not need in public API
pub struct LruShard<V>(RwLock<LruCache<u128, V>>);

/// Sharded concurrent data structure for LruCache
pub struct ConcurrentLruCache<V, const N: usize> {
    lrus: [LruShard<V>; N],
}

impl<V, const N: usize> ConcurrentLruCache<V, N> {
    pub fn new(shard_capacity: usize) -> Self {
        use std::num::NonZeroUsize;
        // safe, 1 != 0
        const ONE: NonZeroUsize = NonZeroUsize::new(1).unwrap();
        // Same `arrayvec` element-by-element init as `ConcurrentHashTable::new`
        // and `pingora_lru::Lru` — works for any `N`, not just `N <= 32`.
        let cap = shard_capacity.try_into().unwrap_or(ONE);
        let mut lrus = arrayvec::ArrayVec::<_, N>::new();
        for _ in 0..N {
            lrus.push(LruShard(RwLock::new(LruCache::new(cap))));
        }
        ConcurrentLruCache {
            // Same as `ConcurrentHashTable::new` — `into_inner` cannot fail
            // because we pushed exactly N elements; `.ok().expect(...)` keeps
            // the message readable without forcing the element type to be
            // `Debug`.
            lrus: lrus
                .into_inner()
                .ok()
                .expect("ArrayVec pushed N times, into_inner is infallible"),
        }
    }
    pub fn get(&self, key: u128) -> &RwLock<LruCache<u128, V>> {
        &self.lrus[get_shard(key, N)].0
    }

    #[allow(dead_code)]
    pub fn read(&self, key: u128) -> RwLockReadGuard<'_, LruCache<u128, V>> {
        self.get(key).read()
    }

    pub fn write(&self, key: u128) -> RwLockWriteGuard<'_, LruCache<u128, V>> {
        self.get(key).write()
    }

    // TODO: work out the lifetimes to provide get/set directly
}
