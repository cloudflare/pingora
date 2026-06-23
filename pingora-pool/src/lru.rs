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

use core::hash::{Hash, Hasher};
use lru::LruCache;
use parking_lot::Mutex;
use std::collections::hash_map::DefaultHasher;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::sync::Arc;
use tokio::sync::Notify;

pub struct Node<T> {
    pub close_notifier: Arc<Notify>,
    pub meta: T,
    order: u64,
}

impl<T> Node<T> {
    pub fn new(meta: T) -> Self {
        Node {
            close_notifier: Arc::new(Notify::new()),
            meta,
            order: 0,
        }
    }

    pub fn notify_close(&self) {
        self.close_notifier.notify_one();
    }
}

/// Number of independent LRU shards. Independent of the `N_SHARDS` constant
/// in `crate::connection`. `pub` only so the in-crate benchmark can use it.
pub const N_SHARDS: usize = 16;

/// Bounded, key-sharded LRU used to age out idle connections.
///
/// Each key is hashed into one of [`N_SHARDS`] shards, so the same key
/// always lands in the same shard. `put` and `pop` each take one shard
/// lock on the fast path. When occupancy exceeds `size`, one caller drains
/// the overflow globally in a single batch.
pub struct Lru<K, T>
where
    K: Send,
    T: Send,
{
    /// Per-shard caches. Individually unbounded; global cap enforced via `len`.
    lrus: [Mutex<LruCache<K, Node<T>>>; N_SHARDS],
    /// Total live entries across shards. Lower `order` = older entry.
    len: AtomicUsize,
    /// Monotonic stamp assigned at insert; used to compare age across shards.
    order: AtomicU64,
    /// Global capacity. Eviction runs when `len` exceeds this.
    size: usize,
    drain: AtomicBool,
}

impl<K, T> Lru<K, T>
where
    K: Hash + Eq + Send,
    T: Send,
{
    pub fn new(size: usize) -> Self {
        Lru {
            lrus: std::array::from_fn(|_| Mutex::new(LruCache::unbounded())),
            len: AtomicUsize::new(0),
            order: AtomicU64::new(0),
            size,
            drain: AtomicBool::new(false),
        }
    }

    #[inline]
    fn shard(&self, key: &K) -> &Mutex<LruCache<K, Node<T>>> {
        let mut hasher = DefaultHasher::new();
        key.hash(&mut hasher);
        &self.lrus[(hasher.finish() as usize) % N_SHARDS]
    }

    /// Drain to `size - headroom` by globally oldest `order`.
    ///
    /// Holds every shard lock for the duration to serialize concurrent
    /// evictors and prevent victim-selection races. Returns the metas of
    /// evicted nodes; `notify_close` has fired on each before return.
    fn evict_lru(&self) -> Vec<T> {
        // Fast path: another evictor may have already drained the overflow.
        if self.len.load(Relaxed) <= self.size {
            return Vec::new();
        }

        let headroom = self.evict_headroom();
        let drain_target = self.size.saturating_sub(headroom);
        let mut lrus: Vec<_> = self.lrus.iter().map(|lru| lru.lock()).collect();
        // A single over-capacity insert usually drains headroom + 1 entries.
        // Concurrent inserts can grow the batch further, so this is only a hint.
        let mut evicted_metas = Vec::with_capacity(headroom + 1);
        while self.len.load(Relaxed) > drain_target {
            let mut oldest = None;
            for (index, lru) in lrus.iter().enumerate() {
                if let Some((_, node)) = lru.peek_lru() {
                    if oldest.is_none_or(|(_, order)| node.order < order) {
                        oldest = Some((index, node.order));
                    }
                }
            }

            let Some((index, _)) = oldest else { break };
            let Some((_, evicted)) = lrus[index].pop_lru() else {
                break;
            };
            self.len.fetch_sub(1, Relaxed);
            evicted.notify_close();
            evicted_metas.push(evicted.meta);
        }
        evicted_metas
    }

    /// Headroom below `size`. Amortizes the all-shards-lock cost over
    /// several subsequent over-capacity inserts. Capped at [`N_SHARDS`]
    /// and `size / 4` so tiny pools keep tight semantics.
    #[inline]
    fn evict_headroom(&self) -> usize {
        N_SHARDS.min(self.size / 4)
    }

    /// Insert `value` for `key`. Returns evicted metas, oldest first.
    ///
    /// `len` is only incremented on new inserts (not replacements), so
    /// over-capacity eviction tracks live entries.
    pub fn put(&self, key: K, mut value: Node<T>) -> Vec<T> {
        if self.drain.load(Relaxed) {
            value.notify_close(); // sort of hack to simulate being evicted right away
            return Vec::new();
        }

        value.order = self.order.fetch_add(1, Relaxed);
        if self.shard(&key).lock().put(key, value).is_some() {
            // replaced
            return Vec::new();
        }
        if self.len.fetch_add(1, Relaxed) + 1 > self.size {
            return self.evict_lru();
        }
        Vec::new()
    }

    pub fn add(&self, key: K, meta: T) -> (Arc<Notify>, Vec<T>) {
        let node = Node::new(meta);
        let notifier = node.close_notifier.clone();
        (notifier, self.put(key, node))
    }

    pub fn pop(&self, key: &K) -> Option<Node<T>> {
        let popped = self.shard(key).lock().pop(key);
        if popped.is_some() {
            self.len.fetch_sub(1, Relaxed);
        }
        popped
    }

    #[allow(dead_code)]
    pub fn drain(&self) {
        self.drain.store(true, Relaxed);

        for lru in &self.lrus {
            let mut lru_cache = lru.lock();
            for (_, item) in lru_cache.iter() {
                item.notify_close();
            }
            lru_cache.clear();
        }
        self.len.store(0, Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::FutureExt;
    use log::debug;

    #[tokio::test]
    async fn test_evict_close() {
        let pool: Lru<i32, ()> = Lru::new(2);
        let (notifier1, _) = pool.add(1, ());
        let (notifier2, _) = pool.add(2, ());
        let (notifier3, _) = pool.add(3, ());
        let closed_item = tokio::select! {
            _ = notifier1.notified() => {debug!("notifier1"); 1},
            _ = notifier2.notified() => {debug!("notifier2"); 2},
            _ = notifier3.notified() => {debug!("notifier3"); 3},
        };
        assert_eq!(closed_item, 1);
    }

    #[tokio::test]
    async fn test_evict_close_with_pop() {
        let pool: Lru<i32, ()> = Lru::new(2);
        let (notifier1, _) = pool.add(1, ());
        let (notifier2, _) = pool.add(2, ());
        pool.pop(&1);
        let (notifier3, _) = pool.add(3, ());
        let (notifier4, _) = pool.add(4, ());
        let closed_item = tokio::select! {
            _ = notifier1.notified() => {debug!("notifier1"); 1},
            _ = notifier2.notified() => {debug!("notifier2"); 2},
            _ = notifier3.notified() => {debug!("notifier3"); 3},
            _ = notifier4.notified() => {debug!("notifier4"); 4},
        };
        assert_eq!(closed_item, 2);
    }

    #[tokio::test]
    async fn test_replaced_node_is_not_treated_as_eviction() {
        let pool: Lru<i32, i32> = Lru::new(2);
        let (replaced_notifier, evicted) = pool.add(1, 10);
        assert!(evicted.is_empty());

        let (_, evicted) = pool.add(1, 20);
        assert!(evicted.is_empty());
        assert_eq!(pool.pop(&1).unwrap().meta, 20);

        assert!(
            replaced_notifier.notified().now_or_never().is_none(),
            "replaced node should not notify close"
        );
    }

    #[tokio::test]
    async fn test_drain() {
        let pool: Lru<i32, ()> = Lru::new(4);
        let (notifier1, _) = pool.add(1, ());
        let (notifier2, _) = pool.add(2, ());
        let (notifier3, _) = pool.add(3, ());
        pool.drain();
        let (notifier4, _) = pool.add(4, ());

        tokio::join!(
            notifier1.notified(),
            notifier2.notified(),
            notifier3.notified(),
            notifier4.notified()
        );
    }
}
