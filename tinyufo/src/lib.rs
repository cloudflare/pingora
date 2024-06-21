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

//! A In-memory cache implementation with TinyLFU as the admission policy and [S3-FIFO](https://s3fifo.com/) as the eviction policy.
//!
//! TinyUFO improves cache hit ratio noticeably compared to LRU.
//!
//! TinyUFO is lock-free. It is very fast in the systems with a lot concurrent reads and/or writes

use ahash::RandomState;
use crossbeam_queue::SegQueue;
use std::marker::PhantomData;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::{
    AtomicBool, AtomicU8,
    Ordering::{Acquire, Relaxed, SeqCst},
};
mod buckets;
mod estimation;

use buckets::Buckets;
use estimation::TinyLfu;
use std::hash::Hash;

const SMALL: bool = false;
const MAIN: bool = true;

// Indicate which queue an item is located
#[derive(Debug, Default)]
struct Location(AtomicBool);

impl Location {
    fn new_small() -> Self {
        Self(AtomicBool::new(SMALL))
    }

    fn value(&self) -> bool {
        self.0.load(Relaxed)
    }

    fn is_main(&self) -> bool {
        self.value()
    }

    fn move_to_main(&self) {
        self.0.store(true, Relaxed);
    }
}

// We have 8 bits to spare but we still cap at 3. This is to make sure that the main queue
// in the worst case can find something to evict quickly
const USES_CAP: u8 = 3;

#[derive(Debug, Default)]
struct Uses(AtomicU8);

impl Uses {
    pub fn inc_uses(&self) -> u8 {
        loop {
            let uses = self.uses();
            if uses >= USES_CAP {
                return uses;
            }
            if let Err(new) = self.0.compare_exchange(uses, uses + 1, Acquire, Relaxed) {
                // someone else beat us to it
                if new >= USES_CAP {
                    // already above cap
                    return new;
                } // else, try again
            } else {
                return uses + 1;
            }
        }
    }

    // decrease uses, return the previous value
    pub fn decr_uses(&self) -> u8 {
        loop {
            let uses = self.uses();
            if uses == 0 {
                return 0;
            }
            if let Err(new) = self.0.compare_exchange(uses, uses - 1, Acquire, Relaxed) {
                // someone else beat us to it
                if new == 0 {
                    return 0;
                } // else, try again
            } else {
                return uses;
            }
        }
    }

    pub fn uses(&self) -> u8 {
        self.0.load(Relaxed)
    }
}

type Key = u64;
type Weight = u16;

/// The key-value pair returned from cache eviction
#[derive(Clone)]
pub struct KV<T> {
    /// NOTE: that we currently don't store the Actual key in the cache. This returned value
    /// is just the hash of it.
    pub key: Key,
    pub data: T,
    pub weight: Weight,
}

// the data and its metadata
pub struct Bucket<T> {
    uses: Uses,
    queue: Location,
    weight: Weight,
    data: T,
}

const SMALL_QUEUE_PERCENTAGE: f32 = 0.1;

struct FiFoQueues<T> {
    total_weight_limit: usize,

    small: SegQueue<Key>,
    small_weight: AtomicUsize,

    main: SegQueue<Key>,
    main_weight: AtomicUsize,

    // this replaces the ghost queue of S3-FIFO with similar goal: track the evicted assets
    estimator: TinyLfu,

    _t: PhantomData<T>,
}

impl<T: Clone + Send + Sync + 'static> FiFoQueues<T> {
    fn admit(
        &self,
        key: Key,
        data: T,
        weight: u16,
        ignore_lfu: bool,
        buckets: &Buckets<T>,
    ) -> Vec<KV<T>> {
        // Note that we only use TinyLFU during cache admission but not cache read.
        // So effectively we mostly sketch the popularity of less popular assets.
        // In this way the sketch is a bit more accurate on these assets.
        // Also we don't need another separated window cache to address the sparse burst issue as
        // this sketch doesn't favor very popular assets much.
        let new_freq = self.estimator.incr(key);

        assert!(weight > 0);
        let new_bucket = {
            let Some((uses, queue, weight)) = buckets.get_map(&key, |bucket| {
                // the item exists, in case weight changes
                let old_weight = bucket.weight;
                let uses = bucket.uses.inc_uses();

                fn update_atomic(weight: &AtomicUsize, old: u16, new: u16) {
                    if old == new {
                        return;
                    }
                    if old > new {
                        weight.fetch_sub((old - new) as usize, SeqCst);
                    } else {
                        weight.fetch_add((new - old) as usize, SeqCst);
                    }
                }
                let queue = bucket.queue.is_main();
                if queue == MAIN {
                    update_atomic(&self.main_weight, old_weight, weight);
                } else {
                    update_atomic(&self.small_weight, old_weight, weight);
                }
                (uses, queue, weight)
            }) else {
                let mut evicted = self.evict_to_limit(weight, buckets);
                // TODO: figure out the right way to compare frequencies of different weights across
                // many evicted assets. For now TinyLFU is only used when only evicting 1 item.
                let (key, data, weight) = if !ignore_lfu && evicted.len() == 1 {
                    // Apply the admission algorithm of TinyLFU: compare the incoming new item
                    // and the evicted one. The more popular one is admitted to cache
                    let evicted_first = &evicted[0];
                    let evicted_freq = self.estimator.get(evicted_first.key);
                    if evicted_freq > new_freq {
                        // put it back
                        let first = evicted.pop().expect("just check non-empty");
                        // return the put value
                        evicted.push(KV { key, data, weight });
                        (first.key, first.data, first.weight)
                    } else {
                        (key, data, weight)
                    }
                } else {
                    (key, data, weight)
                };

                let bucket = Bucket {
                    queue: Location::new_small(),
                    weight,
                    uses: Default::default(), // 0
                    data,
                };
                let old = buckets.insert(key, bucket);
                if old.is_none() {
                    // Always push key first before updating weight
                    // If doing the other order, another concurrent thread might not
                    // find things to evict
                    self.small.push(key);
                    self.small_weight.fetch_add(weight as usize, SeqCst);
                } // else: two threads are racing adding the item
                  // TODO: compare old.weight and update accordingly
                return evicted;
            };
            Bucket {
                queue: Location(queue.into()),
                weight,
                uses: Uses(uses.into()),
                data,
            }
        };

        // replace the existing one
        buckets.insert(key, new_bucket);

        // NOTE: there is a chance that the item itself is evicted if it happens to be the one selected
        // by the algorithm. We could avoid this by checking if the item is in the returned evicted items,
        // and then add it back. But to keep the code simple we just allow it to happen.
        self.evict_to_limit(0, buckets)
    }

    // the `extra_weight` is to essentially tell the cache to reserve that amount of weight for
    // admission. It is used when calling `evict_to_limit` before admitting the asset itself.
    fn evict_to_limit(&self, extra_weight: Weight, buckets: &Buckets<T>) -> Vec<KV<T>> {
        let mut evicted = if self.total_weight_limit
            < self.small_weight.load(SeqCst) + self.main_weight.load(SeqCst) + extra_weight as usize
        {
            Vec::with_capacity(1)
        } else {
            vec![]
        };
        while self.total_weight_limit
            < self.small_weight.load(SeqCst) + self.main_weight.load(SeqCst) + extra_weight as usize
        {
            if let Some(evicted_item) = self.evict_one(buckets) {
                evicted.push(evicted_item);
            } else {
                break;
            }
        }

        evicted
    }

    fn evict_one(&self, buckets: &Buckets<T>) -> Option<KV<T>> {
        let evict_small = self.small_weight_limit() <= self.small_weight.load(SeqCst);

        if evict_small {
            let evicted = self.evict_one_from_small(buckets);
            // evict_one_from_small could just promote everything to main without evicting any
            // so need to evict_one_from_main if nothing evicted
            if evicted.is_some() {
                return evicted;
            }
        }
        self.evict_one_from_main(buckets)
    }

    fn small_weight_limit(&self) -> usize {
        (self.total_weight_limit as f32 * SMALL_QUEUE_PERCENTAGE).floor() as usize + 1
    }

    fn evict_one_from_small(&self, buckets: &Buckets<T>) -> Option<KV<T>> {
        loop {
            let Some(to_evict) = self.small.pop() else {
                // empty queue, this is caught between another pop() and fetch_sub()
                return None;
            };

            let v = buckets
                .get_map(&to_evict, |bucket| {
                    let weight = bucket.weight;
                    self.small_weight.fetch_sub(weight as usize, SeqCst);

                    if bucket.uses.uses() > 1 {
                        // move to main
                        bucket.queue.move_to_main();
                        self.main.push(to_evict);
                        self.main_weight.fetch_add(weight as usize, SeqCst);
                        // continue until find one to evict
                        None
                    } else {
                        let data = bucket.data.clone();
                        let weight = bucket.weight;
                        buckets.remove(&to_evict);
                        Some(KV {
                            key: to_evict,
                            data,
                            weight,
                        })
                    }
                })
                .flatten();
            if v.is_some() {
                // found the one to evict, break
                return v;
            }
        }
    }

    fn evict_one_from_main(&self, buckets: &Buckets<T>) -> Option<KV<T>> {
        loop {
            let to_evict = self.main.pop()?;

            if let Some(v) = buckets
                .get_map(&to_evict, |bucket| {
                    if bucket.uses.decr_uses() > 0 {
                        // put it back
                        self.main.push(to_evict);
                        // continue the loop
                        None
                    } else {
                        // evict
                        let weight = bucket.weight;
                        self.main_weight.fetch_sub(weight as usize, SeqCst);
                        let data = bucket.data.clone();
                        buckets.remove(&to_evict);
                        Some(KV {
                            key: to_evict,
                            data,
                            weight,
                        })
                    }
                })
                .flatten()
            {
                // found the one to evict, break
                return Some(v);
            }
        }
    }
}

/// [TinyUfo] cache
pub struct TinyUfo<K, T> {
    queues: FiFoQueues<T>,
    buckets: Buckets<T>,
    random_status: RandomState,
    _k: PhantomData<K>,
}
impl<K: Hash, T: Clone + Send + Sync + 'static> TinyUfo<K, T> {
    /// Create a new TinyUfo cache with the given weight limit and the given
    /// size limit of the ghost queue.
    pub fn new(total_weight_limit: usize, estimated_size: usize) -> Self {
        let queues = FiFoQueues {
            small: SegQueue::new(),
            small_weight: 0.into(),
            main: SegQueue::new(),
            main_weight: 0.into(),
            total_weight_limit,
            estimator: TinyLfu::new(estimated_size),
            _t: PhantomData,
        };
        TinyUfo {
            queues,
            buckets: Buckets::new_fast(estimated_size),
            random_status: RandomState::new(),
            _k: PhantomData,
        }
    }

    /// Create a new TinyUfo cache but with more memory efficient data structures.
    /// The trade-off is that the the get() is slower by a constant factor.
    /// The cache hit ratio could be higher as this type of TinyUFO allows to store
    /// more assets with the same memory.
    pub fn new_compact(total_weight_limit: usize, estimated_size: usize) -> Self {
        let queues = FiFoQueues {
            small: SegQueue::new(),
            small_weight: 0.into(),
            main: SegQueue::new(),
            main_weight: 0.into(),
            total_weight_limit,
            estimator: TinyLfu::new_compact(estimated_size),
            _t: PhantomData,
        };
        TinyUfo {
            queues,
            buckets: Buckets::new_compact(estimated_size, 32),
            random_status: RandomState::new(),
            _k: PhantomData,
        }
    }

    // TODO: with_capacity()

    /// Read the given key
    ///
    /// Return Some(T) if the key exists
    pub fn get(&self, key: &K) -> Option<T> {
        let key = self.random_status.hash_one(key);
        self.buckets.get_map(&key, |p| {
            p.uses.inc_uses();
            p.data.clone()
        })
    }

    /// Put the key value to the [TinyUfo]
    ///
    /// Return a list of [KV] of key and `T` that are evicted
    pub fn put(&self, key: K, data: T, weight: Weight) -> Vec<KV<T>> {
        let key = self.random_status.hash_one(&key);
        self.queues.admit(key, data, weight, false, &self.buckets)
    }

    /// Always put the key value to the [TinyUfo]
    ///
    /// Return a list of [KV] of key and `T` that are evicted
    ///
    /// Similar to [Self::put] but guarantee the assertion of the asset.
    /// In [Self::put], the TinyLFU check may reject putting the current asset if it is less
    /// popular than the once being evicted.
    ///
    /// In some real world use cases, a few reads to the same asset may be pending for the put action
    /// to be finished so that they can read the asset from cache. Neither the above behaviors are ideal
    /// for this use case.
    ///
    /// Compared to [Self::put], the hit ratio when using this function is reduced by about 0.5pp or less in
    /// under zipf workloads.
    pub fn force_put(&self, key: K, data: T, weight: Weight) -> Vec<KV<T>> {
        let key = self.random_status.hash_one(&key);
        self.queues.admit(key, data, weight, true, &self.buckets)
    }

    #[cfg(test)]
    fn peek_queue(&self, key: K) -> Option<bool> {
        let key = self.random_status.hash_one(&key);
        self.buckets.get_queue(&key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_uses() {
        let uses: Uses = Default::default();
        assert_eq!(uses.uses(), 0);
        uses.inc_uses();
        assert_eq!(uses.uses(), 1);
        for _ in 0..USES_CAP {
            uses.inc_uses();
        }
        assert_eq!(uses.uses(), USES_CAP);

        for _ in 0..USES_CAP + 2 {
            uses.decr_uses();
        }
        assert_eq!(uses.uses(), 0);
    }

    #[test]
    fn test_evict_from_small() {
        let mut cache = TinyUfo::new(5, 5);
        cache.random_status = RandomState::with_seeds(2, 3, 4, 5);
        cache.queues.estimator = TinyLfu::new_seeded(5);

        cache.put(1, 1, 1);
        cache.put(2, 2, 2);
        cache.put(3, 3, 2);
        // cache full now

        assert_eq!(cache.peek_queue(1), Some(SMALL));
        assert_eq!(cache.peek_queue(2), Some(SMALL));
        assert_eq!(cache.peek_queue(3), Some(SMALL));

        let evicted = cache.put(4, 4, 3);
        assert_eq!(evicted.len(), 2);
        assert_eq!(evicted[0].data, 1);
        assert_eq!(evicted[1].data, 2);

        assert_eq!(cache.peek_queue(1), None);
        assert_eq!(cache.peek_queue(2), None);
        assert_eq!(cache.peek_queue(3), Some(SMALL));
    }

    #[test]
    fn test_evict_from_small_to_main() {
        let mut cache = TinyUfo::new(5, 5);
        cache.random_status = RandomState::with_seeds(2, 3, 4, 5);
        cache.queues.estimator = TinyLfu::new_seeded(5);

        cache.put(1, 1, 1);
        cache.put(2, 2, 2);
        cache.put(3, 3, 2);
        // cache full now

        cache.get(&1);
        cache.get(&1); // 1 will be moved to main during next eviction

        assert_eq!(cache.peek_queue(1), Some(SMALL));
        assert_eq!(cache.peek_queue(2), Some(SMALL));
        assert_eq!(cache.peek_queue(3), Some(SMALL));

        let evicted = cache.put(4, 4, 2);
        assert_eq!(evicted.len(), 1);
        assert_eq!(evicted[0].weight, 2);

        assert_eq!(cache.peek_queue(1), Some(MAIN));
        // either 2, 3, or 4 was evicted. Check evicted for which.
        let mut remaining = vec![2, 3, 4];
        remaining.remove(
            remaining
                .iter()
                .position(|x| *x == evicted[0].data)
                .unwrap(),
        );
        assert_eq!(cache.peek_queue(evicted[0].key), None);
        for k in remaining {
            assert_eq!(cache.peek_queue(k), Some(SMALL));
        }
    }

    #[test]
    fn test_evict_reentry() {
        let mut cache = TinyUfo::new(5, 5);
        cache.random_status = RandomState::with_seeds(2, 3, 4, 5);
        cache.queues.estimator = TinyLfu::new_seeded(5);

        cache.put(1, 1, 1);
        cache.put(2, 2, 2);
        cache.put(3, 3, 2);
        // cache full now

        assert_eq!(cache.peek_queue(1), Some(SMALL));
        assert_eq!(cache.peek_queue(2), Some(SMALL));
        assert_eq!(cache.peek_queue(3), Some(SMALL));

        let evicted = cache.put(4, 4, 1);
        assert_eq!(evicted.len(), 1);
        assert_eq!(evicted[0].data, 1);

        assert_eq!(cache.peek_queue(1), None);
        assert_eq!(cache.peek_queue(2), Some(SMALL));
        assert_eq!(cache.peek_queue(3), Some(SMALL));
        assert_eq!(cache.peek_queue(4), Some(SMALL));

        let evicted = cache.put(1, 1, 1);
        assert_eq!(evicted.len(), 1);
        assert_eq!(evicted[0].data, 2);

        assert_eq!(cache.peek_queue(1), Some(SMALL));
        assert_eq!(cache.peek_queue(2), None);
        assert_eq!(cache.peek_queue(3), Some(SMALL));
        assert_eq!(cache.peek_queue(4), Some(SMALL));
    }

    #[test]
    fn test_evict_entry_denied() {
        let mut cache = TinyUfo::new(5, 5);
        cache.random_status = RandomState::with_seeds(2, 3, 4, 5);
        cache.queues.estimator = TinyLfu::new_seeded(5);

        cache.put(1, 1, 1);
        cache.put(2, 2, 2);
        cache.put(3, 3, 2);
        // cache full now

        assert_eq!(cache.peek_queue(1), Some(SMALL));
        assert_eq!(cache.peek_queue(2), Some(SMALL));
        assert_eq!(cache.peek_queue(3), Some(SMALL));

        // trick: put a few times to bump their frequencies
        cache.put(1, 1, 1);
        cache.put(2, 2, 2);
        cache.put(3, 3, 2);

        let evicted = cache.put(4, 4, 1);
        assert_eq!(evicted.len(), 1);
        assert_eq!(evicted[0].data, 4); // 4 is returned

        assert_eq!(cache.peek_queue(1), Some(SMALL));
        assert_eq!(cache.peek_queue(2), Some(SMALL));
        assert_eq!(cache.peek_queue(3), Some(SMALL));
        assert_eq!(cache.peek_queue(4), None);
    }

    #[test]
    fn test_force_put() {
        let mut cache = TinyUfo::new(5, 5);
        cache.random_status = RandomState::with_seeds(2, 3, 4, 5);
        cache.queues.estimator = TinyLfu::new_seeded(5);

        cache.put(1, 1, 1);
        cache.put(2, 2, 2);
        cache.put(3, 3, 2);
        // cache full now

        assert_eq!(cache.peek_queue(1), Some(SMALL));
        assert_eq!(cache.peek_queue(2), Some(SMALL));
        assert_eq!(cache.peek_queue(3), Some(SMALL));

        // trick: put a few times to bump their frequencies
        cache.put(1, 1, 1);
        cache.put(2, 2, 2);
        cache.put(3, 3, 2);

        // force put will replace 1 with 4 even through 1 is more popular
        let evicted = cache.force_put(4, 4, 1);
        assert_eq!(evicted.len(), 1);
        assert_eq!(evicted[0].data, 1); // 1 is returned

        assert_eq!(cache.peek_queue(1), None);
        assert_eq!(cache.peek_queue(2), Some(SMALL));
        assert_eq!(cache.peek_queue(3), Some(SMALL));
        assert_eq!(cache.peek_queue(4), Some(SMALL));
    }

    #[test]
    fn test_evict_from_main() {
        let mut cache = TinyUfo::new(5, 5);
        cache.random_status = RandomState::with_seeds(2, 3, 4, 5);
        cache.queues.estimator = TinyLfu::new_seeded(5);

        cache.put(1, 1, 1);
        cache.put(2, 2, 2);
        cache.put(3, 3, 2);
        // cache full now

        // all 3 will qualify to main
        cache.get(&1);
        cache.get(&1);
        cache.get(&2);
        cache.get(&2);
        cache.get(&3);
        cache.get(&3);

        let evicted = cache.put(4, 4, 1);
        assert_eq!(evicted.len(), 1);
        assert_eq!(evicted[0].data, 1);

        // 1 kicked from main
        assert_eq!(cache.peek_queue(1), None);
        assert_eq!(cache.peek_queue(2), Some(MAIN));
        assert_eq!(cache.peek_queue(3), Some(MAIN));
        assert_eq!(cache.peek_queue(4), Some(SMALL));

        let evicted = cache.put(1, 1, 1);
        assert_eq!(evicted.len(), 1);
        assert_eq!(evicted[0].data, 4);

        assert_eq!(cache.peek_queue(1), Some(SMALL));
        assert_eq!(cache.peek_queue(2), Some(MAIN));
        assert_eq!(cache.peek_queue(3), Some(MAIN));
        assert_eq!(cache.peek_queue(4), None);
    }

    #[test]
    fn test_evict_from_small_compact() {
        let mut cache = TinyUfo::new(5, 5);
        cache.random_status = RandomState::with_seeds(2, 3, 4, 5);
        cache.queues.estimator = TinyLfu::new_compact_seeded(5);

        cache.put(1, 1, 1);
        cache.put(2, 2, 2);
        cache.put(3, 3, 2);
        // cache full now

        assert_eq!(cache.peek_queue(1), Some(SMALL));
        assert_eq!(cache.peek_queue(2), Some(SMALL));
        assert_eq!(cache.peek_queue(3), Some(SMALL));

        let evicted = cache.put(4, 4, 3);
        assert_eq!(evicted.len(), 2);
        assert_eq!(evicted[0].data, 1);
        assert_eq!(evicted[1].data, 2);

        assert_eq!(cache.peek_queue(1), None);
        assert_eq!(cache.peek_queue(2), None);
        assert_eq!(cache.peek_queue(3), Some(SMALL));
    }

    #[test]
    fn test_evict_from_small_to_main_compact() {
        let mut cache = TinyUfo::new(5, 5);
        cache.random_status = RandomState::with_seeds(2, 3, 4, 5);
        cache.queues.estimator = TinyLfu::new_compact_seeded(5);

        cache.put(1, 1, 1);
        cache.put(2, 2, 2);
        cache.put(3, 3, 2);
        // cache full now

        cache.get(&1);
        cache.get(&1); // 1 will be moved to main during next eviction

        assert_eq!(cache.peek_queue(1), Some(SMALL));
        assert_eq!(cache.peek_queue(2), Some(SMALL));
        assert_eq!(cache.peek_queue(3), Some(SMALL));

        let evicted = cache.put(4, 4, 2);
        assert_eq!(evicted.len(), 1);
        assert_eq!(evicted[0].weight, 2);

        assert_eq!(cache.peek_queue(1), Some(MAIN));
        // either 2, 3, or 4 was evicted. Check evicted for which.
        let mut remaining = vec![2, 3, 4];
        remaining.remove(
            remaining
                .iter()
                .position(|x| *x == evicted[0].data)
                .unwrap(),
        );
        assert_eq!(cache.peek_queue(evicted[0].key), None);
        for k in remaining {
            assert_eq!(cache.peek_queue(k), Some(SMALL));
        }
    }
}
