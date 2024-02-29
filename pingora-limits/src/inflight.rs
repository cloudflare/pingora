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

//! The inflight module defines the [Inflight] type which estimates the count of events occurring
//! at any point in time.

use crate::estimator::Estimator;
use crate::{hash, RandomState};
use std::hash::Hash;
use std::sync::Arc;

/// An `Inflight` type tracks the frequency of actions that are actively occurring. When the value
/// is dropped from scope, the count will automatically decrease.
pub struct Inflight {
    estimator: Arc<Estimator>,
    hasher: RandomState,
}

// fixed parameters for simplicity: hashes: h, slots: n
// Time complexity for a lookup operation is O(h). Space complexity is O(h*n)
// False positive ratio is 1/(n^h)
// We choose a small h and a large n to keep lookup cheap and FP ratio low
const HASHES: usize = 4;
const SLOTS: usize = 8192;

impl Inflight {
    /// Create a new `Inflight`.
    pub fn new() -> Self {
        Inflight {
            estimator: Arc::new(Estimator::new(HASHES, SLOTS)),
            hasher: RandomState::new(),
        }
    }

    /// Increment `key` by the value given. The return value is a tuple of a [Guard] and the
    /// estimated count.
    pub fn incr<T: Hash>(&self, key: T, value: isize) -> (Guard, isize) {
        let guard = Guard {
            estimator: self.estimator.clone(),
            id: hash(key, &self.hasher),
            value,
        };
        let estimation = guard.incr();
        (guard, estimation)
    }
}

/// A `Guard` is returned when an `Inflight` key is incremented via [Inflight::incr].
pub struct Guard {
    estimator: Arc<Estimator>,
    // store the hash instead of the actual key to save space
    id: u64,
    value: isize,
}

impl Guard {
    /// Increment the key's value that the `Guard` was created from.
    pub fn incr(&self) -> isize {
        self.estimator.incr(self.id, self.value)
    }

    /// Get the estimated count of the key that the `Guard` was created from.
    pub fn get(&self) -> isize {
        self.estimator.get(self.id)
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        self.estimator.decr(self.id, self.value)
    }
}

impl std::fmt::Debug for Guard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Guard")
            .field("id", &self.id)
            .field("value", &self.value)
            // no need to dump shared estimator
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inflight_count() {
        let inflight = Inflight::new();
        let (g1, v) = inflight.incr("a", 1);
        assert_eq!(v, 1);
        let (g2, v) = inflight.incr("a", 2);
        assert_eq!(v, 3);

        drop(g1);

        assert_eq!(g2.get(), 2);

        drop(g2);

        let (_, v) = inflight.incr("a", 1);
        assert_eq!(v, 1);
    }
}
