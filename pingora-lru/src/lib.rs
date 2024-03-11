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
use std::sync::atomic::{AtomicUsize, Ordering};

/// The LRU with `N` shards
pub struct Lru<T, const N: usize> {
    units: [RwLock<LruUnit<T>>; N],
    weight: AtomicUsize,
    weight_limit: usize,
    len: AtomicUsize,
    evicted_weight: AtomicUsize,
    evicted_len: AtomicUsize,
}

impl<T, const N: usize> Lru<T, N> {
    /// Create an [Lru] with the given weight limit and predicted capacity.
    ///
    /// The capacity is per shard (for simplicity). So the total capacity = capacity * N
    pub fn with_capacity(weight_limit: usize, capacity: usize) -> Self {
        // use the unsafe code from ArrayVec just to init the array
        let mut units = arrayvec::ArrayVec::<_, N>::new();
        for _ in 0..N {
            units.push(RwLock::new(LruUnit::with_capacity(capacity)));
        }
        Lru {
            // we did init all N elements so safe to unwrap
            // map_err because unwrap() requires LruUnit to TODO: impl Debug
            units: units.into_inner().map_err(|_| "").unwrap(),
            weight: AtomicUsize::new(0),
            weight_limit,
            len: AtomicUsize::new(0),
            evicted_weight: AtomicUsize::new(0),
            evicted_len: AtomicUsize::new(0),
        }
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
                self.len.fetch_add(1, Ordering::Relaxed);
            }
        }
        shard
    }

    /// Promote the key to the head of the LRU
    ///
    /// Return `true` if the key exists.
    pub fn promote(&self, key: u64) -> bool {
        self.units[get_shard(key, N)].write().access(key)
    }

    /// Promote to the top n of the LRU
    ///
    /// This function is a bit more efficient in terms of reducing lock contention because it
    /// will acquire a write lock only if the key is outside top n but only acquires a read lock
    /// when the key is already in the top n.
    ///
    /// Return false if the item doesn't exist
    pub fn promote_top_n(&self, key: u64, top: usize) -> bool {
        let unit = &self.units[get_shard(key, N)];
        if !unit.read().need_promote(key, top) {
            return true;
        }
        unit.write().access(key)
    }

    /// Evict at most one item from the given shard
    ///
    /// Return the evicted asset and its size if there is anything to evict
    pub fn evict_shard(&self, shard: u64) -> Option<(T, usize)> {
        let evicted = self.units[get_shard(shard, N)].write().evict();
        if let Some((_, weight)) = evicted.as_ref() {
            self.weight.fetch_sub(*weight, Ordering::Relaxed);
            self.len.fetch_sub(1, Ordering::Relaxed);
            self.evicted_weight.fetch_add(*weight, Ordering::Relaxed);
            self.evicted_len.fetch_add(1, Ordering::Relaxed);
        }
        evicted
    }

    /// Evict the [Lru] until the overall weight is below the limit.
    ///
    /// Return a list of evicted items.
    ///
    /// The evicted items are randomly selected from all the shards.
    pub fn evict_to_limit(&self) -> Vec<(T, usize)> {
        let mut evicted = vec![];
        let mut initial_weight = self.weight();
        let mut shard_seed = rand::random(); // start from a random shard
        let mut empty_shard = 0;

        // Entries can be admitted or removed from the LRU by others during the loop below
        // Track initial_weight not to over evict due to entries admitted after the loop starts
        // self.weight() is also used not to over evict due to some entries are removed by others
        while initial_weight > self.weight_limit
            && self.weight() > self.weight_limit
            && empty_shard < N
        {
            if let Some(i) = self.evict_shard(shard_seed) {
                initial_weight -= i.1;
                evicted.push(i)
            } else {
                empty_shard += 1;
            }
            // move on to the next shard
            shard_seed += 1;
        }
        evicted
    }

    /// Remove the given asset
    pub fn remove(&self, key: u64) -> Option<(T, usize)> {
        let removed = self.units[get_shard(key, N)].write().remove(key);
        if let Some((_, weight)) = removed.as_ref() {
            self.weight.fetch_sub(*weight, Ordering::Relaxed);
            self.len.fetch_sub(1, Ordering::Relaxed);
        }
        removed
    }

    /// Insert the item to the tail of this LRU
    ///
    /// Useful to recreate an LRU in most-to-least order
    pub fn insert_tail(&self, key: u64, data: T, weight: usize) -> bool {
        if self.units[get_shard(key, N)]
            .write()
            .insert_tail(key, data, weight)
        {
            self.weight.fetch_add(weight, Ordering::Relaxed);
            self.len.fetch_add(1, Ordering::Relaxed);
            true
        } else {
            false
        }
    }

    /// Check existence of a key without changing the order in LRU
    pub fn peek(&self, key: u64) -> bool {
        self.units[get_shard(key, N)].read().peek(key).is_some()
    }

    /// Return the current total weight
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

    /// Get the number of items inside a shard
    pub fn shard_len(&self, shard: usize) -> usize {
        self.units[shard].read().len()
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

    pub fn peek(&self, key: u64) -> Option<&T> {
        self.lookup_table.get(&key).map(|n| &n.data)
    }

    // admin into LRU, return old weight if there was any
    pub fn admit(&mut self, key: u64, data: T, weight: usize) -> usize {
        if let Some(node) = self.lookup_table.get_mut(&key) {
            let old_weight = node.weight;
            if weight != old_weight {
                self.used_weight += weight;
                self.used_weight -= old_weight;
                node.weight = weight;
            }
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

impl<'a, T> DoubleEndedIterator for LruUnitIter<'a, T> {
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
        // NOTE: there is a low chance this test would fail see the TODO in evict_to_limit
        assert_eq!(lru.weight(), 6);
        assert_eq!(lru.len(), 3);
        assert_eq!(lru.evicted_weight(), 6);
        assert_eq!(lru.evicted_len(), 3);
        assert_eq!(evicted.len(), 3);
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
}
