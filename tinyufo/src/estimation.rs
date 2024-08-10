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

use ahash::RandomState;
use std::hash::Hash;
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};

struct Estimator {
    estimator: Box<[(Box<[AtomicU8]>, RandomState)]>,
}

impl Estimator {
    fn optimal_paras(items: usize) -> (usize, usize) {
        use std::cmp::max;
        // derived from https://en.wikipedia.org/wiki/Count%E2%80%93min_sketch
        // width = ceil(e / ε)
        // depth = ceil(ln(1 − δ) / ln(1 / 2))
        let error_range = 1.0 / (items as f64);
        let failure_probability = 1.0 / (items as f64);
        (
            max((std::f64::consts::E / error_range).ceil() as usize, 16),
            max((failure_probability.ln() / 0.5f64.ln()).ceil() as usize, 2),
        )
    }

    fn optimal(items: usize) -> Self {
        let (slots, hashes) = Self::optimal_paras(items);
        Self::new(hashes, slots, RandomState::new)
    }

    fn compact(items: usize) -> Self {
        let (slots, hashes) = Self::optimal_paras(items / 100);
        Self::new(hashes, slots, RandomState::new)
    }

    #[cfg(test)]
    fn seeded(items: usize) -> Self {
        let (slots, hashes) = Self::optimal_paras(items);
        Self::new(hashes, slots, || RandomState::with_seeds(2, 3, 4, 5))
    }

    #[cfg(test)]
    fn seeded_compact(items: usize) -> Self {
        let (slots, hashes) = Self::optimal_paras(items / 100);
        Self::new(hashes, slots, || RandomState::with_seeds(2, 3, 4, 5))
    }

    /// Create a new `Estimator` with the given amount of hashes and columns (slots) using
    /// the given random source.
    pub fn new(hashes: usize, slots: usize, random: impl Fn() -> RandomState) -> Self {
        let mut estimator = Vec::with_capacity(hashes);
        for _ in 0..hashes {
            let mut slot = Vec::with_capacity(slots);
            for _ in 0..slots {
                slot.push(AtomicU8::new(0));
            }
            estimator.push((slot.into_boxed_slice(), random()));
        }

        Estimator {
            estimator: estimator.into_boxed_slice(),
        }
    }

    pub fn incr<T: Hash>(&self, key: T) -> u8 {
        let mut min = u8::MAX;
        for (slot, hasher) in self.estimator.iter() {
            let hash = hasher.hash_one(&key) as usize;
            let counter = &slot[hash % slot.len()];
            let (_current, new) = incr_no_overflow(counter);
            min = std::cmp::min(min, new);
        }
        min
    }

    /// Get the estimated frequency of `key`.
    pub fn get<T: Hash>(&self, key: T) -> u8 {
        let mut min = u8::MAX;
        for (slot, hasher) in self.estimator.iter() {
            let hash = hasher.hash_one(&key) as usize;
            let counter = &slot[hash % slot.len()];
            let current = counter.load(Ordering::Relaxed);
            min = std::cmp::min(min, current);
        }
        min
    }

    /// right shift all values inside this `Estimator`.
    pub fn age(&self, shift: u8) {
        for (slot, _) in self.estimator.iter() {
            for counter in slot.iter() {
                // we don't CAS because the only update between the load and store
                // is fetch_add(1), which should be fine to miss/ignore
                let c = counter.load(Ordering::Relaxed);
                counter.store(c >> shift, Ordering::Relaxed);
            }
        }
    }
}

fn incr_no_overflow(var: &AtomicU8) -> (u8, u8) {
    loop {
        let current = var.load(Ordering::Relaxed);
        if current == u8::MAX {
            return (current, current);
        }
        let new = if current == u8::MAX - 1 {
            u8::MAX
        } else {
            current + 1
        };
        if let Err(new) = var.compare_exchange(current, new, Ordering::Acquire, Ordering::Relaxed) {
            // someone else beat us to it
            if new == u8::MAX {
                // already max
                return (current, new);
            } // else, try again
        } else {
            return (current, new);
        }
    }
}

// bare-minimum TinyLfu with CM-Sketch, no doorkeeper for now
pub(crate) struct TinyLfu {
    estimator: Estimator,
    window_counter: AtomicUsize,
    window_limit: usize,
}

impl TinyLfu {
    pub fn get<T: Hash>(&self, key: T) -> u8 {
        self.estimator.get(key)
    }

    pub fn incr<T: Hash>(&self, key: T) -> u8 {
        let window_size = self.window_counter.fetch_add(1, Ordering::Relaxed);
        // When window_size concurrently increases, only one resets the window and age the estimator.
        // > self.window_limit * 2 is a safety net in case for whatever reason window_size grows
        // out of control
        if window_size == self.window_limit || window_size > self.window_limit * 2 {
            self.window_counter.store(0, Ordering::Relaxed);
            self.estimator.age(1); // right shift 1 bit
        }
        self.estimator.incr(key)
    }

    // because we use 8-bits counters, window size can be 256 * the cache size
    pub fn new(cache_size: usize) -> Self {
        Self {
            estimator: Estimator::optimal(cache_size),
            window_counter: Default::default(),
            // 8x: just a heuristic to balance the memory usage and accuracy
            window_limit: cache_size * 8,
        }
    }

    pub fn new_compact(cache_size: usize) -> Self {
        Self {
            estimator: Estimator::compact(cache_size),
            window_counter: Default::default(),
            // 8x: just a heuristic to balance the memory usage and accuracy
            window_limit: cache_size * 8,
        }
    }

    #[cfg(test)]
    pub fn new_seeded(cache_size: usize) -> Self {
        Self {
            estimator: Estimator::seeded(cache_size),
            window_counter: Default::default(),
            // 8x: just a heuristic to balance the memory usage and accuracy
            window_limit: cache_size * 8,
        }
    }

    #[cfg(test)]
    pub fn new_compact_seeded(cache_size: usize) -> Self {
        Self {
            estimator: Estimator::seeded_compact(cache_size),
            window_counter: Default::default(),
            // 8x: just a heuristic to balance the memory usage and accuracy
            window_limit: cache_size * 8,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cmk_paras() {
        let (slots, hashes) = Estimator::optimal_paras(1_000_000);
        // just smoke check some standard input
        assert_eq!(slots, 2718282);
        assert_eq!(hashes, 20);
    }

    #[test]
    fn test_tiny_lfu() {
        let tiny = TinyLfu::new(1);
        assert_eq!(tiny.get(1), 0);
        assert_eq!(tiny.incr(1), 1);
        assert_eq!(tiny.incr(1), 2);
        assert_eq!(tiny.get(1), 2);

        // Might have hash collisions for the others, need to
        // get() before can assert on the incr() value.
        let two = tiny.get(2);
        assert_eq!(tiny.incr(2), two + 1);
        assert_eq!(tiny.incr(2), two + 2);
        assert_eq!(tiny.get(2), two + 2);

        let three = tiny.get(3);
        assert_eq!(tiny.incr(3), three + 1);
        assert_eq!(tiny.incr(3), three + 2);
        assert_eq!(tiny.incr(3), three + 3);
        assert_eq!(tiny.incr(3), three + 4);

        // 8 incr(), now resets on next incr
        // can only assert they are greater than or equal
        // to the incr() we do per key.

        assert!(tiny.incr(3) >= 3); // had 4, reset to 2, added another.
        assert!(tiny.incr(1) >= 2); // had 2, reset to 1, added another.
        assert!(tiny.incr(2) >= 2); // had 2, reset to 1, added another.
    }
}
