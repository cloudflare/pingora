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

//! The estimator module contains a Count-Min Sketch type to help estimate the frequency of an item.

use crate::hash;
use crate::RandomState;
use std::hash::Hash;
use std::sync::atomic::{AtomicIsize, Ordering};

/// An implementation of a lock-free count–min sketch estimator. See the [wikipedia] page for more
/// information.
///
/// [wikipedia]: https://en.wikipedia.org/wiki/Count%E2%80%93min_sketch
pub struct Estimator {
    estimator: Box<[(Box<[AtomicIsize]>, RandomState)]>,
}

impl Estimator {
    /// Create a new `Estimator` with the given amount of hashes and columns (slots).
    pub fn new(hashes: usize, slots: usize) -> Self {
        Self {
            estimator: (0..hashes)
                .map(|_| (0..slots).map(|_| AtomicIsize::new(0)).collect::<Vec<_>>())
                .map(|slot| (slot.into_boxed_slice(), RandomState::new()))
                .collect::<Vec<_>>()
                .into_boxed_slice(),
        }
    }

    /// Increment `key` by the value given. Return the new estimated value as a result.
    /// Note: overflow can happen. When some of the internal counters overflow, a negative number
    /// will be returned. It is up to the caller to catch and handle this case.
    pub fn incr<T: Hash>(&self, key: T, value: isize) -> isize {
        self.estimator
            .iter()
            .fold(isize::MAX, |min, (slot, hasher)| {
                let hash = hash(&key, hasher) as usize;
                let counter = &slot[hash % slot.len()];
                // Overflow is allowed for simplicity
                let current = counter.fetch_add(value, Ordering::Relaxed);
                std::cmp::min(min, current + value)
            })
    }

    /// Decrement `key` by the value given.
    pub fn decr<T: Hash>(&self, key: T, value: isize) {
        for (slot, hasher) in self.estimator.iter() {
            let hash = hash(&key, hasher) as usize;
            let counter = &slot[hash % slot.len()];
            counter.fetch_sub(value, Ordering::Relaxed);
        }
    }

    /// Get the estimated frequency of `key`.
    pub fn get<T: Hash>(&self, key: T) -> isize {
        self.estimator
            .iter()
            .fold(isize::MAX, |min, (slot, hasher)| {
                let hash = hash(&key, hasher) as usize;
                let counter = &slot[hash % slot.len()];
                let current = counter.load(Ordering::Relaxed);
                std::cmp::min(min, current)
            })
    }

    /// Reset all values inside this `Estimator`.
    pub fn reset(&self) {
        self.estimator.iter().for_each(|(slot, _)| {
            slot.iter()
                .for_each(|counter| counter.store(0, Ordering::Relaxed))
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn incr() {
        let est = Estimator::new(8, 8);
        let v = est.incr("a", 1);
        assert_eq!(v, 1);
        let v = est.incr("b", 1);
        assert_eq!(v, 1);
        let v = est.incr("a", 2);
        assert_eq!(v, 3);
        let v = est.incr("b", 2);
        assert_eq!(v, 3);
    }

    #[test]
    fn desc() {
        let est = Estimator::new(8, 8);
        est.incr("a", 3);
        est.incr("b", 3);
        est.decr("a", 1);
        est.decr("b", 1);
        assert_eq!(est.get("a"), 2);
        assert_eq!(est.get("b"), 2);
    }

    #[test]
    fn get() {
        let est = Estimator::new(8, 8);
        est.incr("a", 1);
        est.incr("a", 2);
        est.incr("b", 1);
        est.incr("b", 2);
        assert_eq!(est.get("a"), 3);
        assert_eq!(est.get("b"), 3);
    }

    #[test]
    fn reset() {
        let est = Estimator::new(8, 8);
        est.incr("a", 1);
        est.incr("a", 2);
        est.incr("b", 1);
        est.incr("b", 2);
        est.decr("b", 1);
        est.reset();
        assert_eq!(est.get("a"), 0);
        assert_eq!(est.get("b"), 0);
    }
}
