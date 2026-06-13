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

//! An implementation of an LRU that focuses on memory efficiency, concurrency and persistence
//!
//! Features
//! - keys can have different sizes
//! - LRUs are sharded to avoid global locks.
//! - Memory layout and usage are optimized: small and no memory fragmentation

pub mod linked_list;

use linked_list::{LinkedList, LinkedListIter};

use hashbrown::HashMap;
use parking_lot::RwLock;
use rand::Rng;
use std::sync::atomic::{AtomicUsize, Ordering};

/// The LRU with `N` shards
pub struct Lru<T, const N: usize> {
    units: [RwLock<LruUnit<T>>; N],
    /// Lock-free `Relaxed` shadow of each shard's item count, backing
    /// [`Lru::shard_len`] and the P2C selection in [`Lru::evict_to_limit`].
    /// Maintained alongside [`Lru::len`] at every count-mutating site.
    shard_lens: [AtomicUsize; N],
    weight: AtomicUsize,
    weight_limit: AtomicUsize,
    len_watermark: Option<usize>,
    len: AtomicUsize,
    evicted_weight: AtomicUsize,
    evicted_len: AtomicUsize,
}

impl<T, const N: usize> Lru<T, N> {
    /// Create an [Lru] with the given weight limit and predicted capacity.
    ///
    /// The capacity is per shard (for simplicity). So the total capacity = capacity * N
    pub fn with_capacity(weight_limit: usize, capacity: usize) -> Self {
        Self::with_capacity_and_watermark(weight_limit, capacity, None)
    }

    /// Create an [Lru] with the given weight limit, predicted capacity and optional watermark
    ///
    /// The capacity is per shard (for simplicity). So the total capacity = capacity * N
    ///
    /// The watermark indicates at what count we should begin evicting and acts as a limit
    /// on the total number of allowed items.
    pub fn with_capacity_and_watermark(
        weight_limit: usize,
        capacity: usize,
        len_watermark: Option<usize>,
    ) -> Self {
        // use the unsafe code from ArrayVec just to init the array
        let mut units = arrayvec::ArrayVec::<_, N>::new();
        let mut shard_lens = arrayvec::ArrayVec::<_, N>::new();
        for _ in 0..N {
            units.push(RwLock::new(LruUnit::with_capacity(capacity)));
            shard_lens.push(AtomicUsize::new(0));
        }
        Lru {
            units: units.into_inner().map_err(|_| "").unwrap(),
            shard_lens: shard_lens
                .into_inner()
                .expect("shard_lens ArrayVec filled with exactly N elements"),
            weight: AtomicUsize::new(0),
            weight_limit: AtomicUsize::new(weight_limit),
            len_watermark,
            len: AtomicUsize::new(0),
            evicted_weight: AtomicUsize::new(0),
            evicted_len: AtomicUsize::new(0),
        }
    }

    /// Return the current total weight limit.
    pub fn weight_limit(&self) -> usize {
        self.weight_limit.load(Ordering::Relaxed)
    }

    /// Set the total weight limit used by [`Self::evict_to_limit`].
    pub fn set_weight_limit(&self, weight_limit: usize) {
        self.weight_limit.store(weight_limit, Ordering::Relaxed);
    }

    /// Increment item-count bookkeeping for `shard`. Both atomics use
    /// `Relaxed`; called while holding the shard write lock so that
    /// `len` and `shard_lens[shard]` advance in lockstep.
    #[inline]
    fn incr_count(&self, shard: usize) {
        self.len.fetch_add(1, Ordering::Relaxed);
        self.shard_lens[shard].fetch_add(1, Ordering::Relaxed);
    }

    /// Decrement item-count bookkeeping for `shard`. See
    /// [`Self::incr_count`].
    #[inline]
    fn decr_count(&self, shard: usize) {
        self.len.fetch_sub(1, Ordering::Relaxed);
        self.shard_lens[shard].fetch_sub(1, Ordering::Relaxed);
    }

    /// Admit the key value to the [Lru]
    ///
    /// Return the shard index which the asset is added to
    pub fn admit(&self, key: u64, data: T, weight: usize) -> usize {
        let shard = get_shard(key, N);
        let unit = &mut self.units[shard].write();

        // Make sure weight is positive otherwise eviction won't work
        // TODO: Probably should use NonZeroUsize instead
        let weight = weight.max(1);

        let old_weight = unit.admit(key, data, weight);
        if old_weight != weight {
            self.weight.fetch_add(weight, Ordering::Relaxed);
            if old_weight > 0 {
                self.weight.fetch_sub(old_weight, Ordering::Relaxed);
            } else {
                // Assume old_weight == 0 means a new item is admitted
                self.incr_count(shard);
            }
        }
        shard
    }

    /// Increment the weight associated with a given key, up to an optional max weight.
    /// If a `max_weight` is provided, the weight cannot exceed this max weight. If the current
    /// weight is higher than the max, it will be capped to the max.
    ///
    /// Return the total new weight. 0 indicates the key did not exist.
    pub fn increment_weight(&self, key: u64, delta: usize, max_weight: Option<usize>) -> usize {
        let shard = get_shard(key, N);
        let unit = &mut self.units[shard].write();
        if let Some((old_weight, new_weight)) = unit.increment_weight(key, delta, max_weight) {
            if new_weight >= old_weight {
                self.weight
                    .fetch_add(new_weight - old_weight, Ordering::Relaxed);
            } else {
                self.weight
                    .fetch_sub(old_weight - new_weight, Ordering::Relaxed);
            }
            new_weight
        } else {
            0
        }
    }

    /// Promote the key to the head of the LRU
    ///
    /// Return `true` if the key exists.
    pub fn promote(&self, key: u64) -> bool {
        self.units[get_shard(key, N)].write().access(key)
    }

    /// Promote to the top n of the LRU
    ///
    /// This function acquires a read lock first to check if the key is already
    /// in the top `n` positions. If so, it returns early without a write lock.
    /// Otherwise it falls through to a write lock for the actual promotion.
    ///
    /// **Performance note**: this optimization only helps when `n` covers a
    /// significant fraction of the shard. At production scale (~100K+ items
    /// per shard), hot items are rarely in the top N positions, so the
    /// read-lock scan is usually wasted work that adds latency without
    /// reducing contention. Benchmarks (`cargo bench --bench bench_lru`)
    /// show that plain [`promote()`](Self::promote) is faster at scale.
    /// Consider using `promote()` directly unless profiling shows a clear
    /// benefit for your workload.
    ///
    /// Return false if the item doesn't exist
    pub fn promote_top_n(&self, key: u64, top: usize) -> bool {
        let unit = &self.units[get_shard(key, N)];
        if !unit.read().need_promote(key, top) {
            return true;
        }
        unit.write().access(key)
    }

    /// Evict at most one item from the given shard, identified by the
    /// hash-like `shard` seed (mapped into `0..N` via `% N`).
    ///
    /// Return the evicted asset and its size if there is anything to evict.
    pub fn evict_shard(&self, shard: u64) -> Option<(T, usize)> {
        self.evict_shard_at(get_shard(shard, N))
    }

    /// Evict at most one item from the shard at index `shard` (in `0..N`).
    /// Internal entry point that skips the `% N` round-trip in
    /// [`Self::evict_shard`].
    fn evict_shard_at(&self, shard: usize) -> Option<(T, usize)> {
        assert!(shard < N);
        let evicted = self.units[shard].write().evict();
        if let Some((_, weight)) = evicted.as_ref() {
            self.weight.fetch_sub(*weight, Ordering::Relaxed);
            self.decr_count(shard);
            self.evicted_weight.fetch_add(*weight, Ordering::Relaxed);
            self.evicted_len.fetch_add(1, Ordering::Relaxed);
        }
        evicted
    }

    /// Evict the [Lru] until the overall weight is below the limit (or the configured watermark).
    ///
    /// Return a list of evicted items.
    ///
    /// Each iteration selects the shard to evict from using the "power of two
    /// choices" strategy: two shards are picked uniformly at random and the
    /// one with more items is chosen (see
    /// <https://brooker.co.za/blog/2012/01/17/two-random.html>). This biases
    /// eviction toward longer shards and drives [`Self::shard_len`] toward a
    /// uniform distribution, which keeps per-shard serialization cost (e.g.
    /// `pingora_cache::eviction::lru::Manager::serialize_shard`) bounded.
    ///
    /// Selection is by item count, not weight, even when eviction is
    /// triggered by `weight_limit`. With heavily skewed item weights this
    /// may evict more items than a weight-biased policy to reach the same
    /// total weight — the tradeoff is intentional in favor of bounded
    /// per-shard serialization cost.
    ///
    /// O(1) per iteration in the common case. If the chosen shard is
    /// empty when we acquire its write lock (the Relaxed shadow may
    /// not always reflect actual emptiness, and P2C may tie-break to
    /// an empty shard when all shadow lengths are equal), we linearly
    /// probe successive shard indices until one yields an item or we
    /// wrap back to the starting shard — at which point every shard
    /// was observed empty and we exit. Bounded by at most N probes
    /// per outer iteration.
    pub fn evict_to_limit(&self) -> Vec<(T, usize)> {
        self.evict_to_limit_with_rng(&mut rand::thread_rng())
    }

    /// Internal entry point for [`Self::evict_to_limit`] that lets tests
    /// inject a seeded RNG for deterministic P2C selection.
    fn evict_to_limit_with_rng<R: Rng>(&self, rng: &mut R) -> Vec<(T, usize)> {
        let mut evicted = vec![];
        let mut initial_weight = self.weight();
        let mut initial_len = self.len();

        // Transient over-limit weight can persist until the next
        // admit/increment_weight call, which is acceptable because the
        // next admission will re-trigger eviction.
        let weight_limit = self.weight_limit();
        while (initial_weight > weight_limit && self.weight() > weight_limit)
            || self
                .len_watermark
                .is_some_and(|w| initial_len > w && self.len() > w)
        {
            // Power of two choices: pick the longer of two random shards.
            // N == 1 short-circuits the redundant second roll.
            let start = if N <= 1 {
                0
            } else {
                let a = rng.gen_range(0..N);
                let b = rng.gen_range(0..N);
                if self.shard_len(a) >= self.shard_len(b) {
                    a
                } else {
                    b
                }
            };
            // Try the chosen shard first; on a miss (empty or raced),
            // linearly probe successive indices. Wrapping back to
            // `start` means every shard was observed empty, so we exit.
            let mut shard = start;
            let evicted_one = loop {
                if let Some(item) = self.evict_shard_at(shard) {
                    break Some(item);
                }
                shard = (shard + 1) % N;
                if shard == start {
                    break None;
                }
            };
            match evicted_one {
                Some(i) => {
                    initial_weight = initial_weight.saturating_sub(i.1);
                    initial_len = initial_len.saturating_sub(1);
                    evicted.push(i);
                }
                None => break,
            }
        }
        evicted
    }

    /// Remove the given asset.
    pub fn remove(&self, key: u64) -> Option<(T, usize)> {
        let shard = get_shard(key, N);
        let removed = self.units[shard].write().remove(key);
        if let Some((_, weight)) = removed.as_ref() {
            self.weight.fetch_sub(*weight, Ordering::Relaxed);
            self.decr_count(shard);
        }
        removed
    }

    /// Insert the item to the tail of this LRU.
    ///
    /// Useful to recreate an LRU in most-to-least order
    pub fn insert_tail(&self, key: u64, data: T, weight: usize) -> bool {
        let shard = get_shard(key, N);
        if self.units[shard].write().insert_tail(key, data, weight) {
            self.weight.fetch_add(weight, Ordering::Relaxed);
            self.incr_count(shard);
            true
        } else {
            false
        }
    }

    /// Check existence of a key without changing the order in LRU.
    pub fn peek(&self, key: u64) -> bool {
        self.units[get_shard(key, N)].read().peek(key).is_some()
    }

    /// Check the weight of a key without changing the order in LRU.
    pub fn peek_weight(&self, key: u64) -> Option<usize> {
        self.units[get_shard(key, N)].read().peek_weight(key)
    }

    /// Peek at the least-recently-used item in the given shard without removing it.
    ///
    /// Returns a clone of the data and the weight, or `None` if the shard is empty
    /// or `shard >= N`.
    pub fn peek_lru(&self, shard: usize) -> Option<(T, usize)>
    where
        T: Clone,
    {
        self.units
            .get(shard)?
            .read()
            .peek_lru()
            .map(|(data, weight)| (data.clone(), weight))
    }

    /// Return the current total weight.
    ///
    /// Lock-free `Relaxed` load. Best-effort: not synchronized with
    /// concurrent admissions or evictions on other threads.
    pub fn weight(&self) -> usize {
        self.weight.load(Ordering::Relaxed)
    }

    /// Return the total weight of items evicted from this [Lru].
    pub fn evicted_weight(&self) -> usize {
        self.evicted_weight.load(Ordering::Relaxed)
    }

    /// Return the total count of items evicted from this [Lru].
    pub fn evicted_len(&self) -> usize {
        self.evicted_len.load(Ordering::Relaxed)
    }

    /// The number of items inside this [Lru].
    #[allow(clippy::len_without_is_empty)]
    pub fn len(&self) -> usize {
        self.len.load(Ordering::Relaxed)
    }

    /// Scan a shard with the given function F
    pub fn iter_for_each<F>(&self, shard: usize, f: F)
    where
        F: FnMut((&T, usize)),
    {
        assert!(shard < N);
        self.units[shard].read().iter().for_each(f);
    }

    /// Get the total number of shards
    pub const fn shards(&self) -> usize {
        N
    }

    /// Get the number of items inside a shard.
    ///
    /// Lock-free `Relaxed` load from a per-shard atomic shadow. Best-effort:
    /// there is no cross-thread ordering between this and [`Self::len`], and
    /// `Σ shard_len(i)` is not guaranteed to equal [`Self::len`] at any
    /// given instant. Suitable for eviction-balance heuristics and
    /// observability; not suitable for synchronization.
    pub fn shard_len(&self, shard: usize) -> usize {
        self.shard_lens[shard].load(Ordering::Relaxed)
    }

    /// Get the weight (total size) inside a shard
    pub fn shard_weight(&self, shard: usize) -> usize {
        self.units[shard].read().used_weight
    }
}

#[inline]
fn get_shard(key: u64, n_shards: usize) -> usize {
    (key % n_shards as u64) as usize
}

struct LruNode<T> {
    data: T,
    list_index: usize,
    weight: usize,
}

struct LruUnit<T> {
    lookup_table: HashMap<u64, Box<LruNode<T>>>,
    order: LinkedList,
    used_weight: usize,
}

impl<T> LruUnit<T> {
    fn with_capacity(capacity: usize) -> Self {
        LruUnit {
            lookup_table: HashMap::with_capacity(capacity),
            order: LinkedList::with_capacity(capacity),
            used_weight: 0,
        }
    }

    /// Peek data associated with key, if it exists.
    pub fn peek(&self, key: u64) -> Option<&T> {
        self.lookup_table.get(&key).map(|n| &n.data)
    }

    /// Peek weight associated with key, if it exists.
    pub fn peek_weight(&self, key: u64) -> Option<usize> {
        self.lookup_table.get(&key).map(|n| n.weight)
    }

    /// Admit into LRU, return old weight if there was any.
    pub fn admit(&mut self, key: u64, data: T, weight: usize) -> usize {
        if let Some(node) = self.lookup_table.get_mut(&key) {
            let old_weight = Self::adjust_weight(node, &mut self.used_weight, weight);
            node.data = data;
            self.order.promote(node.list_index);
            return old_weight;
        }
        self.used_weight += weight;
        let list_index = self.order.push_head(key);
        let node = Box::new(LruNode {
            data,
            list_index,
            weight,
        });
        self.lookup_table.insert(key, node);
        0
    }

    /// Increase the weight of an existing key. Returns the new weight or 0 if the key did not
    /// exist, along with the new weight (or 0).
    ///
    /// If a `max_weight` is provided, the weight cannot exceed this max weight. If the current
    /// weight is higher than the max, it will be capped to the max.
    pub fn increment_weight(
        &mut self,
        key: u64,
        delta: usize,
        max_weight: Option<usize>,
    ) -> Option<(usize, usize)> {
        if let Some(node) = self.lookup_table.get_mut(&key) {
            let new_weight =
                max_weight.map_or(node.weight + delta, |m| (node.weight + delta).min(m));
            let old_weight = Self::adjust_weight(node, &mut self.used_weight, new_weight);
            self.order.promote(node.list_index);
            return Some((old_weight, new_weight));
        }
        None
    }

    pub fn access(&mut self, key: u64) -> bool {
        if let Some(node) = self.lookup_table.get(&key) {
            self.order.promote(node.list_index);
            true
        } else {
            false
        }
    }

    // Check if a key is already in the top n most recently used nodes.
    // this is a heuristic to reduce write, which requires exclusive locks, for promotion,
    // especially on very populate nodes
    // NOTE: O(n) search here so limit needs to be small
    pub fn need_promote(&self, key: u64, limit: usize) -> bool {
        !self.order.exist_near_head(key, limit)
    }

    // try to evict 1 node
    pub fn evict(&mut self) -> Option<(T, usize)> {
        self.order.pop_tail().map(|key| {
            // unwrap is safe because we always insert in both the hashtable and the list
            let node = self.lookup_table.remove(&key).unwrap();
            self.used_weight -= node.weight;
            (node.data, node.weight)
        })
    }

    /// Peek at the least-recently-used item without removing it.
    ///
    /// Returns a reference to the data and weight of the tail item, or `None`
    /// if empty.
    pub fn peek_lru(&self) -> Option<(&T, usize)> {
        self.order
            .tail()
            .and_then(|idx| self.order.peek(idx))
            .and_then(|key| self.lookup_table.get(&key))
            .map(|node| (&node.data, node.weight))
    }

    // TODO: scan the tail up to K elements to decide which ones to evict

    pub fn remove(&mut self, key: u64) -> Option<(T, usize)> {
        self.lookup_table.remove(&key).map(|node| {
            let list_key = self.order.remove(node.list_index);
            assert_eq!(key, list_key);
            self.used_weight -= node.weight;
            (node.data, node.weight)
        })
    }

    pub fn insert_tail(&mut self, key: u64, data: T, weight: usize) -> bool {
        if self.lookup_table.contains_key(&key) {
            return false;
        }
        let list_index = self.order.push_tail(key);
        let node = Box::new(LruNode {
            data,
            list_index,
            weight,
        });
        self.lookup_table.insert(key, node);
        self.used_weight += weight;
        true
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        assert_eq!(self.lookup_table.len(), self.order.len());
        self.lookup_table.len()
    }

    #[cfg(test)]
    pub fn used_weight(&self) -> usize {
        self.used_weight
    }

    pub fn iter(&self) -> LruUnitIter<'_, T> {
        LruUnitIter {
            unit: self,
            iter: self.order.iter(),
        }
    }

    // Adjusts node weight to the new given weight.
    // Returns old weight.
    #[inline]
    fn adjust_weight(node: &mut LruNode<T>, used_weight: &mut usize, weight: usize) -> usize {
        let old_weight = node.weight;
        if weight != old_weight {
            *used_weight += weight;
            *used_weight -= old_weight;
            node.weight = weight;
        }
        old_weight
    }
}

struct LruUnitIter<'a, T> {
    unit: &'a LruUnit<T>,
    iter: LinkedListIter<'a>,
}

impl<'a, T> Iterator for LruUnitIter<'a, T> {
    type Item = (&'a T, usize);

    fn next(&mut self) -> Option<Self::Item> {
        self.iter.next().map(|key| {
            // safe because we always items in table and list are always 1:1
            let node = self.unit.lookup_table.get(key).unwrap();
            (&node.data, node.weight)
        })
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.iter.size_hint()
    }
}

impl<T> DoubleEndedIterator for LruUnitIter<'_, T> {
    fn next_back(&mut self) -> Option<Self::Item> {
        self.iter.next_back().map(|key| {
            // safe because we always items in table and list are always 1:1
            let node = self.unit.lookup_table.get(key).unwrap();
            (&node.data, node.weight)
        })
    }
}

#[cfg(test)]
mod test_lru {
    use super::*;

    fn assert_lru<T: Copy + PartialEq + std::fmt::Debug, const N: usize>(
        lru: &Lru<T, N>,
        values: &[T],
        shard: usize,
    ) {
        let mut list_values = vec![];
        lru.iter_for_each(shard, |(v, _)| list_values.push(*v));
        assert_eq!(values, &list_values)
    }

    #[test]
    fn test_admit() {
        let lru = Lru::<_, 2>::with_capacity(30, 10);
        assert_eq!(lru.len(), 0);

        lru.admit(2, 2, 3);
        assert_eq!(lru.len(), 1);
        assert_eq!(lru.weight(), 3);

        lru.admit(2, 2, 1);
        assert_eq!(lru.len(), 1);
        assert_eq!(lru.weight(), 1);

        lru.admit(2, 2, 2); // admit again with different weight
        assert_eq!(lru.len(), 1);
        assert_eq!(lru.weight(), 2);

        lru.admit(3, 3, 3);
        lru.admit(4, 4, 4);

        assert_eq!(lru.weight(), 2 + 3 + 4);
        assert_eq!(lru.len(), 3);
    }

    #[test]
    fn test_promote() {
        let lru = Lru::<_, 2>::with_capacity(30, 10);

        lru.admit(2, 2, 2);
        lru.admit(3, 3, 3);
        lru.admit(4, 4, 4);
        lru.admit(5, 5, 5);
        lru.admit(6, 6, 6);
        assert_lru(&lru, &[6, 4, 2], 0);
        assert_lru(&lru, &[5, 3], 1);

        assert!(lru.promote(3));
        assert_lru(&lru, &[3, 5], 1);
        assert!(lru.promote(3));
        assert_lru(&lru, &[3, 5], 1);

        assert!(lru.promote(2));
        assert_lru(&lru, &[2, 6, 4], 0);

        assert!(!lru.promote(7)); // 7 doesn't exist
        assert_lru(&lru, &[2, 6, 4], 0);
        assert_lru(&lru, &[3, 5], 1);

        // promote 2 to top 1, already there
        assert!(lru.promote_top_n(2, 1));
        assert_lru(&lru, &[2, 6, 4], 0);

        // promote 4 to top 3, already there
        assert!(lru.promote_top_n(4, 3));
        assert_lru(&lru, &[2, 6, 4], 0);

        // promote 4 to top 2
        assert!(lru.promote_top_n(4, 2));
        assert_lru(&lru, &[4, 2, 6], 0);

        // promote 2 to top 1
        assert!(lru.promote_top_n(2, 1));
        assert_lru(&lru, &[2, 4, 6], 0);

        assert!(!lru.promote_top_n(7, 1)); // 7 doesn't exist
    }

    #[test]
    fn test_evict() {
        let lru = Lru::<_, 2>::with_capacity(14, 10);

        // same weight to make the random eviction less random
        lru.admit(2, 2, 2);
        lru.admit(3, 3, 2);
        lru.admit(4, 4, 4);
        lru.admit(5, 5, 4);
        lru.admit(6, 6, 2);
        lru.admit(7, 7, 2);

        assert_lru(&lru, &[6, 4, 2], 0);
        assert_lru(&lru, &[7, 5, 3], 1);

        assert_eq!(lru.weight(), 16);
        assert_eq!(lru.len(), 6);

        let evicted = lru.evict_to_limit();
        assert_eq!(lru.weight(), 14);
        assert_eq!(lru.len(), 5);
        assert_eq!(lru.evicted_weight(), 2);
        assert_eq!(lru.evicted_len(), 1);
        assert_eq!(evicted.len(), 1);
        assert_eq!(evicted[0].1, 2); //weight
        assert!(evicted[0].0 == 2 || evicted[0].0 == 3); //either 2 or 3 are evicted

        let lru = Lru::<_, 2>::with_capacity(6, 10);

        // same weight random eviction less random
        lru.admit(2, 2, 2);
        lru.admit(3, 3, 2);
        lru.admit(4, 4, 2);
        lru.admit(5, 5, 2);
        lru.admit(6, 6, 2);
        lru.admit(7, 7, 2);
        assert_eq!(lru.weight(), 12);
        assert_eq!(lru.len(), 6);

        let evicted = lru.evict_to_limit();
        assert_eq!(lru.weight(), 6);
        assert_eq!(lru.len(), 3);
        assert_eq!(lru.evicted_weight(), 6);
        assert_eq!(lru.evicted_len(), 3);
        assert_eq!(evicted.len(), 3);
    }

    #[test]
    fn test_increment_weight() {
        let lru = Lru::<_, 2>::with_capacity(6, 10);
        lru.admit(1, 1, 1);
        lru.increment_weight(1, 1, None);
        assert_eq!(lru.weight(), 1 + 1);

        lru.increment_weight(0, 1000, None);
        assert_eq!(lru.weight(), 1 + 1);

        lru.admit(2, 2, 2);
        lru.increment_weight(2, 2, None);
        assert_eq!(lru.weight(), 1 + 1 + 2 + 2);

        lru.increment_weight(2, 2, Some(3));
        assert_eq!(lru.weight(), 1 + 1 + 3);
    }

    #[test]
    fn test_remove() {
        let lru = Lru::<_, 2>::with_capacity(30, 10);
        lru.admit(2, 2, 2);
        lru.admit(3, 3, 3);
        lru.admit(4, 4, 4);
        lru.admit(5, 5, 5);
        lru.admit(6, 6, 6);

        assert_eq!(lru.weight(), 2 + 3 + 4 + 5 + 6);
        assert_eq!(lru.len(), 5);
        assert_lru(&lru, &[6, 4, 2], 0);
        assert_lru(&lru, &[5, 3], 1);

        let node = lru.remove(6).unwrap();
        assert_eq!(node.0, 6); // data
        assert_eq!(node.1, 6); // weight
        assert_eq!(lru.weight(), 2 + 3 + 4 + 5);
        assert_eq!(lru.len(), 4);
        assert_lru(&lru, &[4, 2], 0);

        let node = lru.remove(3).unwrap();
        assert_eq!(node.0, 3); // data
        assert_eq!(node.1, 3); // weight
        assert_eq!(lru.weight(), 2 + 4 + 5);
        assert_eq!(lru.len(), 3);
        assert_lru(&lru, &[5], 1);

        assert!(lru.remove(7).is_none());
    }

    #[test]
    fn test_peek() {
        let lru = Lru::<_, 2>::with_capacity(30, 10);
        lru.admit(2, 2, 2);
        lru.admit(3, 3, 3);
        lru.admit(4, 4, 4);

        assert!(lru.peek(4));
        assert!(lru.peek(3));
        assert!(lru.peek(2));

        assert_lru(&lru, &[4, 2], 0);
        assert_lru(&lru, &[3], 1);
    }

    #[test]
    fn test_insert_tail() {
        let lru = Lru::<_, 2>::with_capacity(30, 10);
        lru.admit(2, 2, 2);
        lru.admit(3, 3, 3);
        lru.admit(4, 4, 4);
        lru.admit(5, 5, 5);
        lru.admit(6, 6, 6);

        assert_eq!(lru.weight(), 2 + 3 + 4 + 5 + 6);
        assert_eq!(lru.len(), 5);
        assert_lru(&lru, &[6, 4, 2], 0);
        assert_lru(&lru, &[5, 3], 1);

        assert!(lru.insert_tail(7, 7, 7));
        assert_eq!(lru.weight(), 2 + 3 + 4 + 5 + 6 + 7);
        assert_eq!(lru.len(), 6);
        assert_lru(&lru, &[5, 3, 7], 1);

        // ignore existing ones
        assert!(!lru.insert_tail(6, 6, 7));
    }

    #[test]
    fn test_evict_to_limit_p2c_bias() {
        use rand::rngs::StdRng;
        use rand::SeedableRng;

        // Shard 0 starts with 50 items, shard 1 with 10 (all weight 1).
        // weight_limit=30 forces 30 evictions. P2C-by-length should pick
        // shard 0 (the longer one) most of the time, driving toward
        // balance. Expected share from shard 0: P2C ≈ 0.75 (P(shard 0)
        // = 3/4 per pick while it stays longer), uniform ≈ 0.50,
        // always-shortest ≈ 0.67 (capped by shard 1's 10 items),
        // always-longest ≈ 1.0. The (0.65..0.95) window distinguishes
        // P2C from uniform; the upper bound catches a degenerate
        // always-longest regression.
        const TRIALS: u64 = 50;
        let mut total_from_shard0 = 0usize;
        let mut total_evicted = 0usize;

        for seed in 0..TRIALS {
            let lru = Lru::<u64, 2>::with_capacity(30, 64);
            for k in 0..50u64 {
                // even keys → shard 0
                lru.admit(k * 2, k * 2, 1);
            }
            for k in 0..10u64 {
                // odd keys → shard 1
                lru.admit(k * 2 + 1, k * 2 + 1, 1);
            }
            assert_eq!(lru.weight(), 60);

            let mut rng = StdRng::seed_from_u64(seed);
            let evicted = lru.evict_to_limit_with_rng(&mut rng);
            assert!(
                lru.weight() <= 30,
                "post-eviction weight {} exceeds limit",
                lru.weight()
            );
            total_from_shard0 += evicted.iter().filter(|(k, _)| k % 2 == 0).count();
            total_evicted += evicted.len();
        }

        assert!(total_evicted > 1000, "too few evictions: {total_evicted}");
        let share = total_from_shard0 as f64 / total_evicted as f64;
        assert!(
            (0.65..0.95).contains(&share),
            "expected shard-0 eviction share in 0.65..0.95 (P2C ≈ 0.75); got {share}"
        );
    }

    #[test]
    fn test_evict_to_limit_break_on_empty_shards_over_limit() {
        // Force `weight` above the limit while every shard is empty
        // (simulating bookkeeping skew). The linear probe must wrap
        // around all N shards and exit cleanly.
        let lru = Lru::<u64, 4>::with_capacity(10, 16);
        lru.weight.fetch_add(100, Ordering::Relaxed);
        assert_eq!(lru.evict_to_limit().len(), 0);
    }

    #[test]
    fn test_watermark_eviction_with_zero_weight_items() {
        // All items have weight 0 so the weight-limit guard never fires;
        // only the length watermark drives eviction. P2C-by-length should
        // still reach the watermark regardless of weight values.
        let lru = Lru::<u64, 2>::with_capacity_and_watermark(usize::MAX / 2, 10, Some(2));
        for k in 0..6u64 {
            lru.insert_tail(k, k, 0);
        }
        assert_eq!(lru.len(), 6);
        assert_eq!(lru.weight(), 0);
        let evicted = lru.evict_to_limit();
        assert_eq!(lru.len(), 2);
        assert_eq!(evicted.len(), 4);
    }

    #[test]
    fn test_evict_to_limit_with_mostly_empty_shards() {
        // 7/8 shards empty: both random rolls land on empty shards ~77%
        // of the time, exercising the linear-probe fallback heavily.
        let lru = Lru::<u64, 8>::with_capacity(2, 16);
        for k in 0..8u64 {
            // multiples of 8 hash to shard 0
            lru.admit(k * 8, k * 8, 1);
        }
        assert_eq!(lru.weight(), 8);

        let evicted = lru.evict_to_limit();
        assert_eq!(lru.weight(), 2);
        assert_eq!(evicted.len(), 6);
        assert!(evicted.iter().all(|(k, _)| k % 8 == 0));
    }

    #[test]
    fn test_evict_to_limit_below_limit_returns_immediately() {
        // Smoke test: outer guard short-circuits when already under limit.
        let lru = Lru::<u64, 4>::with_capacity(0, 16);
        assert_eq!(lru.evict_to_limit().len(), 0);
    }

    #[test]
    fn test_evict_to_limit_n1() {
        // N=1 is a trivial special case in the selection logic; ensure
        // basic eviction still works.
        let lru = Lru::<u64, 1>::with_capacity(2, 16);
        for k in 0..5u64 {
            lru.admit(k, k, 1);
        }
        assert_eq!(lru.weight(), 5);
        let evicted = lru.evict_to_limit();
        assert_eq!(lru.weight(), 2);
        assert_eq!(evicted.len(), 3);
    }

    #[test]
    fn test_set_weight_limit_affects_eviction() {
        let lru = Lru::<u64, 1>::with_capacity(10, 16);
        for k in 0..5u64 {
            lru.admit(k, k, 2);
        }
        assert_eq!(lru.weight(), 10);
        assert_eq!(lru.weight_limit(), 10);

        lru.set_weight_limit(4);
        assert_eq!(lru.weight_limit(), 4);
        let evicted = lru.evict_to_limit();
        assert_eq!(lru.weight(), 4);
        assert_eq!(evicted.len(), 3);

        lru.set_weight_limit(20);
        assert_eq!(lru.evict_to_limit().len(), 0);
    }

    #[test]
    fn test_watermark_eviction() {
        const WEIGHT_LIMIT: usize = usize::MAX / 2;
        let lru = Lru::<u64, 2>::with_capacity_and_watermark(WEIGHT_LIMIT, 10, Some(4));

        // admit 6 items, each weight 1
        for k in [2u64, 3, 4, 5, 6, 7] {
            lru.admit(k, k, 1);
        }

        assert!(lru.weight() < WEIGHT_LIMIT);
        assert_eq!(lru.len(), 6);

        let evicted = lru.evict_to_limit();
        assert_eq!(lru.len(), 4);
        assert_eq!(evicted.len(), 2);
        assert_eq!(lru.evicted_len(), 2);
    }

    #[test]
    fn test_peek_lru() {
        let lru = Lru::<u32, 1>::with_capacity(10, 10);

        // empty shard
        assert!(lru.peek_lru(0).is_none());

        lru.admit(1, 10, 1);
        assert_eq!(lru.peek_lru(0).unwrap(), (10, 1));

        lru.admit(2, 20, 2);
        // key 1 is LRU tail
        assert_eq!(lru.peek_lru(0).unwrap(), (10, 1));

        // promote key 1
        lru.promote(1);
        // key 2 is now LRU tail
        assert_eq!(lru.peek_lru(0).unwrap(), (20, 2));

        // out-of-bounds returns None
        assert!(lru.peek_lru(999).is_none());
    }
}

#[cfg(test)]
mod test_lru_unit {
    use super::*;

    fn assert_lru<T: Copy + PartialEq + std::fmt::Debug>(lru: &LruUnit<T>, values: &[T]) {
        let list_values: Vec<_> = lru.iter().map(|(v, _)| *v).collect();
        assert_eq!(values, &list_values)
    }

    #[test]
    fn test_admit() {
        let mut lru = LruUnit::with_capacity(10);
        assert_eq!(lru.len(), 0);
        assert!(lru.peek(0).is_none());

        lru.admit(2, 2, 1);
        assert_eq!(lru.len(), 1);
        assert_eq!(lru.peek(2).unwrap(), &2);
        assert_eq!(lru.used_weight(), 1);

        lru.admit(2, 2, 2); // admit again with different weight
        assert_eq!(lru.used_weight(), 2);

        lru.admit(3, 3, 3);
        lru.admit(4, 4, 4);

        assert_eq!(lru.used_weight(), 2 + 3 + 4);
        assert_lru(&lru, &[4, 3, 2]);
    }

    #[test]
    fn test_access() {
        let mut lru = LruUnit::with_capacity(10);

        lru.admit(2, 2, 2);
        lru.admit(3, 3, 3);
        lru.admit(4, 4, 4);
        assert_lru(&lru, &[4, 3, 2]);

        assert!(lru.access(3));
        assert_lru(&lru, &[3, 4, 2]);
        assert!(lru.access(3));
        assert_lru(&lru, &[3, 4, 2]);
        assert!(lru.access(2));
        assert_lru(&lru, &[2, 3, 4]);

        assert!(!lru.access(5)); // 5 doesn't exist
        assert_lru(&lru, &[2, 3, 4]);

        assert!(!lru.need_promote(2, 1));
        assert!(lru.need_promote(3, 1));
        assert!(!lru.need_promote(4, 9999));
    }

    #[test]
    fn test_evict() {
        let mut lru = LruUnit::with_capacity(10);

        lru.admit(2, 2, 2);
        lru.admit(3, 3, 3);
        lru.admit(4, 4, 4);
        assert_lru(&lru, &[4, 3, 2]);

        assert!(lru.access(3));
        assert!(lru.access(3));
        assert!(lru.access(2));
        assert_lru(&lru, &[2, 3, 4]);

        assert_eq!(lru.used_weight(), 2 + 3 + 4);
        assert_eq!(lru.evict(), Some((4, 4)));
        assert_eq!(lru.used_weight(), 2 + 3);
        assert_lru(&lru, &[2, 3]);

        assert_eq!(lru.evict(), Some((3, 3)));
        assert_eq!(lru.used_weight(), 2);
        assert_lru(&lru, &[2]);

        assert_eq!(lru.evict(), Some((2, 2)));
        assert_eq!(lru.used_weight(), 0);
        assert_lru(&lru, &[]);

        assert_eq!(lru.evict(), None);
        assert_eq!(lru.used_weight(), 0);
        assert_lru(&lru, &[]);
    }

    #[test]
    fn test_increment_weight() {
        let mut lru = LruUnit::with_capacity(10);
        lru.admit(1, 1, 1);
        lru.increment_weight(1, 1, None);
        assert_eq!(lru.used_weight(), 1 + 1);

        lru.increment_weight(0, 1000, None);
        assert_eq!(lru.used_weight(), 1 + 1);

        lru.admit(2, 2, 2);
        lru.increment_weight(2, 2, None);
        assert_eq!(lru.used_weight(), 1 + 1 + 2 + 2);

        lru.admit(3, 3, 3);
        lru.increment_weight(3, 3, Some(5));
        assert_eq!(lru.used_weight(), 1 + 1 + 2 + 2 + 3 + 2);

        lru.increment_weight(3, 3, Some(3));
        assert_eq!(lru.used_weight(), 1 + 1 + 2 + 2 + 3);
    }

    #[test]
    fn test_remove() {
        let mut lru = LruUnit::with_capacity(10);

        lru.admit(2, 2, 2);
        lru.admit(3, 3, 3);
        lru.admit(4, 4, 4);
        lru.admit(5, 5, 5);
        assert_lru(&lru, &[5, 4, 3, 2]);

        assert!(lru.access(4));
        assert!(lru.access(3));
        assert!(lru.access(3));
        assert!(lru.access(2));
        assert_lru(&lru, &[2, 3, 4, 5]);

        assert_eq!(lru.used_weight(), 2 + 3 + 4 + 5);
        assert_eq!(lru.remove(2), Some((2, 2)));
        assert_eq!(lru.used_weight(), 3 + 4 + 5);
        assert_lru(&lru, &[3, 4, 5]);

        assert_eq!(lru.remove(4), Some((4, 4)));
        assert_eq!(lru.used_weight(), 3 + 5);
        assert_lru(&lru, &[3, 5]);

        assert_eq!(lru.remove(5), Some((5, 5)));
        assert_eq!(lru.used_weight(), 3);
        assert_lru(&lru, &[3]);

        assert_eq!(lru.remove(1), None);
        assert_eq!(lru.used_weight(), 3);
        assert_lru(&lru, &[3]);

        assert_eq!(lru.remove(3), Some((3, 3)));
        assert_eq!(lru.used_weight(), 0);
        assert_lru(&lru, &[]);
    }

    #[test]
    fn test_insert_tail() {
        let mut lru = LruUnit::with_capacity(10);
        assert_eq!(lru.len(), 0);
        assert!(lru.peek(0).is_none());

        assert!(lru.insert_tail(2, 2, 1));
        assert_eq!(lru.len(), 1);
        assert_eq!(lru.peek(2).unwrap(), &2);
        assert_eq!(lru.used_weight(), 1);

        assert!(!lru.insert_tail(2, 2, 2));
        assert!(lru.insert_tail(3, 3, 3));
        assert_eq!(lru.used_weight(), 1 + 3);
        assert_lru(&lru, &[2, 3]);

        assert!(lru.insert_tail(4, 4, 4));
        assert!(lru.insert_tail(5, 5, 5));
        assert_eq!(lru.used_weight(), 1 + 3 + 4 + 5);
        assert_lru(&lru, &[2, 3, 4, 5]);
    }

    #[test]
    fn test_peek_lru() {
        let mut lru = LruUnit::with_capacity(10);

        // empty returns None
        assert!(lru.peek_lru().is_none());

        // single item is both head and tail
        lru.admit(1, 10, 1);
        let (data, weight) = lru.peek_lru().unwrap();
        assert_eq!(*data, 10);
        assert_eq!(weight, 1);

        // second admission pushes first to tail
        lru.admit(2, 20, 2);
        let (data, _) = lru.peek_lru().unwrap();
        assert_eq!(*data, 10); // key 1 is LRU tail

        // promote key 1 — now key 2 is tail
        lru.access(1);
        let (data, _) = lru.peek_lru().unwrap();
        assert_eq!(*data, 20); // key 2 is now LRU tail

        // peek doesn't remove
        assert!(lru.peek_lru().is_some());
        assert!(lru.peek(1).is_some());
        assert!(lru.peek(2).is_some());
    }
}
