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

//! Concurrent storage backend

use super::{Bucket, Key};
use ahash::RandomState;
use quick_cache::sync::Cache;
use scc::HashIndex;

/// N-shard skip list. Memory efficient, constant time lookup on average, but a bit slower
/// than hash map
pub struct Compact<T>(Cache<Key, Bucket<T>>);

impl<T: Send + 'static + Clone> Compact<T> {
    /// Create a new [Compact]
    pub fn new(total_items: usize, items_per_shard: usize) -> Self {
        assert!(items_per_shard > 0);

        let shards = std::cmp::max(total_items / items_per_shard, 1);
        Self(Cache::new(shards))
    }

    pub fn get(&self, key: &Key) -> Option<Bucket<T>> {
        self.0.get(key)
    }

    pub fn get_map<V, F: FnOnce(Bucket<T>) -> V>(&self, key: &Key, f: F) -> Option<V> {
        let v = self.get(key);
        v.map(f)
    }

    fn insert(&self, key: Key, value: Bucket<T>) {
        self.0.insert(key, value);
    }

    fn remove(&self, key: &Key) {
        self.0.remove(key);
    }
}

// Concurrent hash map, fast but use more memory
pub struct Fast<T>(HashIndex<Key, Bucket<T>, RandomState>);

impl<T: Send + Sync + 'static + Clone> Fast<T> {
    pub fn new(total_items: usize) -> Self {
        Self(HashIndex::with_capacity_and_hasher(
            total_items,
            RandomState::new(),
        ))
    }

    pub fn get_map<V, F: FnOnce(&Bucket<T>) -> V>(&self, key: &Key, f: F) -> Option<V> {
        let guard = scc::ebr::Guard::new();
        self.0.peek(&key.clone(), &guard).map(|v| f(v))
    }

    fn insert(&self, key: Key, value: Bucket<T>) -> Option<()> {
        match self.0.insert(key, value) {
            Ok(_) => Some(()),
            Err(_) => None,
        }
    }

    fn remove(&self, key: &Key) {
        self.0.remove(key);
    }
}

pub enum Buckets<T> {
    Fast(Box<Fast<T>>),
    Compact(Compact<T>),
}

impl<T: Send + Sync + 'static + Clone> Buckets<T> {
    pub fn new_fast(items: usize) -> Self {
        Self::Fast(Box::new(Fast::new(items)))
    }

    pub fn new_compact(items: usize, items_per_shard: usize) -> Self {
        Self::Compact(Compact::new(items, items_per_shard))
    }

    pub fn insert(&self, key: Key, value: Bucket<T>) -> Option<()> {
        match self {
            Self::Compact(c) => Some(c.insert(key, value)),
            Self::Fast(f) => f.insert(key, value),
        }
    }

    pub fn remove(&self, key: &Key) {
        match self {
            Self::Compact(c) => c.remove(key),
            Self::Fast(f) => f.remove(key),
        }
    }

    pub fn get_map<V, F: FnOnce(&Bucket<T>) -> V>(&self, key: &Key, f: F) -> Option<V> {
        match self {
            Self::Compact(c) => c.get_map(key, |v| f(&v)),
            Self::Fast(c) => c.get_map(key, f),
        }
    }

    #[cfg(test)]
    pub fn get_queue(&self, key: &Key) -> Option<bool> {
        self.get_map(key, |v| v.queue.is_main())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fast() {
        let fast = Buckets::new_fast(10);

        assert!(fast.get_map(&1, |_| ()).is_none());

        let bucket = Bucket {
            queue: crate::Location::new_small(),
            weight: 1,
            uses: Default::default(),
            data: 1,
        };
        fast.insert(1, bucket);

        assert_eq!(fast.get_map(&1, |v| v.data), Some(1));

        fast.remove(&1);
        assert!(fast.get_map(&1, |_| ()).is_none());
    }

    #[test]
    fn test_compact() {
        let compact = Buckets::new_compact(10, 2);

        assert!(compact.get_map(&1, |_| ()).is_none());

        let bucket = Bucket {
            queue: crate::Location::new_small(),
            weight: 1,
            uses: Default::default(),
            data: 1,
        };
        compact.insert(1, bucket);

        assert_eq!(compact.get_map(&1, |v| v.data), Some(1));

        compact.remove(&1);
        assert!(compact.get_map(&1, |_| ()).is_none());
    }
}
