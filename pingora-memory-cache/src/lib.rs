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
use std::marker::PhantomData;
use std::time::{Duration, Instant};

use tinyufo::TinyUfo;

mod read_through;
pub use read_through::{Lookup, MultiLookup, RTCache};

#[derive(Debug, PartialEq, Eq)]
/// [CacheStatus] indicates the response type for a query.
pub enum CacheStatus {
    /// The key was found in the cache
    Hit,
    /// The key was not found.
    Miss,
    /// The key was found but it was expired.
    Expired,
    /// The key was not initially found but was found after awaiting a lock.
    LockHit,
}

impl CacheStatus {
    /// Return the string representation for [CacheStatus].
    pub fn as_str(&self) -> &str {
        match self {
            Self::Hit => "hit",
            Self::Miss => "miss",
            Self::Expired => "expired",
            Self::LockHit => "lock_hit",
        }
    }

    /// Returns whether this status represents a cache hit.
    pub fn is_hit(&self) -> bool {
        match self {
            CacheStatus::Hit | CacheStatus::LockHit => true,
            CacheStatus::Miss | CacheStatus::Expired => false,
        }
    }
}

#[derive(Debug, Clone)]
struct Node<T: Clone> {
    pub value: T,
    expire_on: Option<Instant>,
}

impl<T: Clone> Node<T> {
    fn new(value: T, ttl: Option<Duration>) -> Self {
        let expire_on = match ttl {
            Some(t) => Instant::now().checked_add(t),
            None => None,
        };
        Node { value, expire_on }
    }

    fn will_expire_at(&self, time: &Instant) -> bool {
        match self.expire_on.as_ref() {
            Some(t) => t <= time,
            None => false,
        }
    }

    fn is_expired(&self) -> bool {
        self.will_expire_at(&Instant::now())
    }
}

/// A high performant in-memory cache with S3-FIFO + TinyLFU
pub struct MemoryCache<K: Hash, T: Clone> {
    store: TinyUfo<u64, Node<T>>,
    _key_type: PhantomData<K>,
    pub(crate) hasher: RandomState,
}

impl<K: Hash, T: Clone + Send + Sync + 'static> MemoryCache<K, T> {
    /// Create a new [MemoryCache] with the given size.
    pub fn new(size: usize) -> Self {
        MemoryCache {
            store: TinyUfo::new(size, size),
            _key_type: PhantomData,
            hasher: RandomState::new(),
        }
    }

    /// Fetch the key and return its value in addition to a [CacheStatus].
    pub fn get(&self, key: &K) -> (Option<T>, CacheStatus) {
        let hashed_key = self.hasher.hash_one(key);

        if let Some(n) = self.store.get(&hashed_key) {
            if !n.is_expired() {
                (Some(n.value), CacheStatus::Hit)
            } else {
                // TODO: consider returning the staled value
                (None, CacheStatus::Expired)
            }
        } else {
            (None, CacheStatus::Miss)
        }
    }

    /// Insert a key and value pair with an optional TTL into the cache.
    ///
    /// An item with zero TTL of zero will not be inserted.
    pub fn put(&self, key: &K, value: T, ttl: Option<Duration>) {
        if let Some(t) = ttl {
            if t.is_zero() {
                return;
            }
        }
        let hashed_key = self.hasher.hash_one(key);
        let node = Node::new(value, ttl);
        // weight is always 1 for now
        self.store.put(hashed_key, node, 1);
    }

    pub(crate) fn force_put(&self, key: &K, value: T, ttl: Option<Duration>) {
        if let Some(t) = ttl {
            if t.is_zero() {
                return;
            }
        }
        let hashed_key = self.hasher.hash_one(key);
        let node = Node::new(value, ttl);
        // weight is always 1 for now
        self.store.force_put(hashed_key, node, 1);
    }

    /// This is equivalent to [MemoryCache::get] but for an arbitrary amount of keys.
    pub fn multi_get<'a, I>(&self, keys: I) -> Vec<(Option<T>, CacheStatus)>
    where
        I: Iterator<Item = &'a K>,
        K: 'a,
    {
        let mut resp = Vec::with_capacity(keys.size_hint().0);
        for key in keys {
            resp.push(self.get(key));
        }
        resp
    }

    /// Same as [MemoryCache::multi_get] but returns the keys that are missing from the cache.
    pub fn multi_get_with_miss<'a, I>(&self, keys: I) -> (Vec<(Option<T>, CacheStatus)>, Vec<&'a K>)
    where
        I: Iterator<Item = &'a K>,
        K: 'a,
    {
        let mut resp = Vec::with_capacity(keys.size_hint().0);
        let mut missed = Vec::with_capacity(keys.size_hint().0 / 2);
        for key in keys {
            let (lookup, cache_status) = self.get(key);
            if lookup.is_none() {
                missed.push(key);
            }
            resp.push((lookup, cache_status));
        }
        (resp, missed)
    }

    // TODO: evict expired first
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread::sleep;

    #[test]
    fn test_get() {
        let cache: MemoryCache<i32, ()> = MemoryCache::new(10);
        let (res, hit) = cache.get(&1);
        assert_eq!(res, None);
        assert_eq!(hit, CacheStatus::Miss);
    }

    #[test]
    fn test_put_get() {
        let cache: MemoryCache<i32, i32> = MemoryCache::new(10);
        let (res, hit) = cache.get(&1);
        assert_eq!(res, None);
        assert_eq!(hit, CacheStatus::Miss);
        cache.put(&1, 2, None);
        let (res, hit) = cache.get(&1);
        assert_eq!(res.unwrap(), 2);
        assert_eq!(hit, CacheStatus::Hit);
    }

    #[test]
    fn test_get_expired() {
        let cache: MemoryCache<i32, i32> = MemoryCache::new(10);
        let (res, hit) = cache.get(&1);
        assert_eq!(res, None);
        assert_eq!(hit, CacheStatus::Miss);
        cache.put(&1, 2, Some(Duration::from_secs(1)));
        sleep(Duration::from_millis(1100));
        let (res, hit) = cache.get(&1);
        assert_eq!(res, None);
        assert_eq!(hit, CacheStatus::Expired);
    }

    #[test]
    fn test_eviction() {
        let cache: MemoryCache<i32, i32> = MemoryCache::new(2);
        cache.put(&1, 2, None);
        cache.put(&2, 4, None);
        cache.put(&3, 6, None);
        let (res, hit) = cache.get(&1);
        assert_eq!(res, None);
        assert_eq!(hit, CacheStatus::Miss);
        let (res, hit) = cache.get(&2);
        assert_eq!(res.unwrap(), 4);
        assert_eq!(hit, CacheStatus::Hit);
        let (res, hit) = cache.get(&3);
        assert_eq!(res.unwrap(), 6);
        assert_eq!(hit, CacheStatus::Hit);
    }

    #[test]
    fn test_multi_get() {
        let cache: MemoryCache<i32, i32> = MemoryCache::new(10);
        cache.put(&2, -2, None);
        let keys: Vec<i32> = vec![1, 2, 3];
        let resp = cache.multi_get(keys.iter());
        assert_eq!(resp[0].0, None);
        assert_eq!(resp[0].1, CacheStatus::Miss);
        assert_eq!(resp[1].0.unwrap(), -2);
        assert_eq!(resp[1].1, CacheStatus::Hit);
        assert_eq!(resp[2].0, None);
        assert_eq!(resp[2].1, CacheStatus::Miss);

        let (resp, missed) = cache.multi_get_with_miss(keys.iter());
        assert_eq!(resp[0].0, None);
        assert_eq!(resp[0].1, CacheStatus::Miss);
        assert_eq!(resp[1].0.unwrap(), -2);
        assert_eq!(resp[1].1, CacheStatus::Hit);
        assert_eq!(resp[2].0, None);
        assert_eq!(resp[2].1, CacheStatus::Miss);
        assert_eq!(missed[0], &1);
        assert_eq!(missed[1], &3);
    }
}
