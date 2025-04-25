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

//! A simple LRU cache manager built on top of the `lru` crate

use super::EvictionManager;
use crate::key::CompactCacheKey;

use async_trait::async_trait;
use lru::LruCache;
use parking_lot::RwLock;
use pingora_error::{BError, ErrorType::*, OrErr, Result};
use serde::de::SeqAccess;
use serde::{Deserialize, Serialize};
use std::collections::hash_map::DefaultHasher;
use std::fs::File;
use std::hash::{Hash, Hasher};
use std::io::prelude::*;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::SystemTime;

#[derive(Debug, Deserialize, Serialize)]
struct Node {
    key: CompactCacheKey,
    size: usize,
}

/// A simple LRU eviction manager
///
/// The implementation is not optimized. All operations require global locks.
pub struct Manager {
    lru: RwLock<LruCache<u64, Node>>,
    limit: usize,
    used: AtomicUsize,
    items: AtomicUsize,
    evicted_size: AtomicUsize,
    evicted_items: AtomicUsize,
}

impl Manager {
    /// Create a new [Manager] with the given total size limit `limit`.
    pub fn new(limit: usize) -> Self {
        Manager {
            lru: RwLock::new(LruCache::unbounded()),
            limit,
            used: AtomicUsize::new(0),
            items: AtomicUsize::new(0),
            evicted_size: AtomicUsize::new(0),
            evicted_items: AtomicUsize::new(0),
        }
    }

    fn insert(&self, hash_key: u64, node: CompactCacheKey, size: usize, reverse: bool) {
        use std::cmp::Ordering::*;
        let node = Node { key: node, size };
        let old = {
            let mut lru = self.lru.write();
            let old = lru.push(hash_key, node);
            if reverse && old.is_none() {
                lru.demote(&hash_key);
            }
            old
        };
        if let Some(old) = old {
            // replacing a node, just need to update used size
            match size.cmp(&old.1.size) {
                Greater => self.used.fetch_add(size - old.1.size, Ordering::Relaxed),
                Less => self.used.fetch_sub(old.1.size - size, Ordering::Relaxed),
                Equal => 0, // same size, update nothing, use 0 to match other arms' type
            };
        } else {
            self.used.fetch_add(size, Ordering::Relaxed);
            self.items.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn increase_weight(&self, key: u64, delta: usize) {
        let mut lru = self.lru.write();
        let Some(node) = lru.get_key_value_mut(&key) else {
            return;
        };
        node.1.size += delta;
        self.used.fetch_add(delta, Ordering::Relaxed);
    }

    // evict items until the used capacity is below the limit
    fn evict(&self) -> Vec<CompactCacheKey> {
        if self.used.load(Ordering::Relaxed) <= self.limit {
            return vec![];
        }
        let mut to_evict = Vec::with_capacity(1); // we will at least pop 1 item
        while self.used.load(Ordering::Relaxed) > self.limit {
            if let Some((_, node)) = self.lru.write().pop_lru() {
                self.used.fetch_sub(node.size, Ordering::Relaxed);
                self.items.fetch_sub(1, Ordering::Relaxed);
                self.evicted_size.fetch_add(node.size, Ordering::Relaxed);
                self.evicted_items.fetch_add(1, Ordering::Relaxed);
                to_evict.push(node.key);
            } else {
                // lru empty
                return to_evict;
            }
        }
        to_evict
    }

    // This could use a lot of memory to buffer the serialized data in memory and could lock the LRU
    // for too long
    fn serialize(&self) -> Result<Vec<u8>> {
        use rmp_serde::encode::Serializer;
        use serde::ser::SerializeSeq;
        use serde::ser::Serializer as _;
        // NOTE: This could use a lot of memory to buffer the serialized data in memory
        let mut ser = Serializer::new(vec![]);
        // NOTE: This long for loop could lock the LRU for too long
        let lru = self.lru.read();
        let mut seq = ser
            .serialize_seq(Some(lru.len()))
            .or_err(InternalError, "fail to serialize node")?;
        for item in lru.iter() {
            seq.serialize_element(item.1).unwrap(); // write to vec, safe
        }
        seq.end().or_err(InternalError, "when serializing LRU")?;
        Ok(ser.into_inner())
    }

    fn deserialize(&self, buf: &[u8]) -> Result<()> {
        use rmp_serde::decode::Deserializer;
        use serde::de::Deserializer as _;
        let mut de = Deserializer::new(buf);
        let visitor = InsertToManager { lru: self };
        de.deserialize_seq(visitor)
            .or_err(InternalError, "when deserializing LRU")?;
        Ok(())
    }
}

struct InsertToManager<'a> {
    lru: &'a Manager,
}

impl<'de> serde::de::Visitor<'de> for InsertToManager<'_> {
    type Value = ();

    fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
        formatter.write_str("array of lru nodes")
    }

    fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        while let Some(node) = seq.next_element::<Node>()? {
            let key = u64key(&node.key);
            self.lru.insert(key, node.key, node.size, true); // insert in the back
        }
        Ok(())
    }
}

#[inline]
fn u64key(key: &CompactCacheKey) -> u64 {
    let mut hasher = DefaultHasher::new();
    key.hash(&mut hasher);
    hasher.finish()
}

const FILE_NAME: &str = "simple_lru.data";

#[async_trait]
impl EvictionManager for Manager {
    fn total_size(&self) -> usize {
        self.used.load(Ordering::Relaxed)
    }
    fn total_items(&self) -> usize {
        self.items.load(Ordering::Relaxed)
    }
    fn evicted_size(&self) -> usize {
        self.evicted_size.load(Ordering::Relaxed)
    }
    fn evicted_items(&self) -> usize {
        self.evicted_items.load(Ordering::Relaxed)
    }

    fn admit(
        &self,
        item: CompactCacheKey,
        size: usize,
        _fresh_until: SystemTime,
    ) -> Vec<CompactCacheKey> {
        let key = u64key(&item);
        self.insert(key, item, size, false);
        self.evict()
    }

    fn increment_weight(&self, item: CompactCacheKey, delta: usize) -> Vec<CompactCacheKey> {
        let key = u64key(&item);
        self.increase_weight(key, delta);
        self.evict()
    }

    fn remove(&self, item: &CompactCacheKey) {
        let key = u64key(item);
        let node = self.lru.write().pop(&key);
        if let Some(n) = node {
            self.used.fetch_sub(n.size, Ordering::Relaxed);
            self.items.fetch_sub(1, Ordering::Relaxed);
        }
    }

    fn access(&self, item: &CompactCacheKey, size: usize, _fresh_until: SystemTime) -> bool {
        let key = u64key(item);
        if self.lru.write().get(&key).is_none() {
            self.insert(key, item.clone(), size, false);
            false
        } else {
            true
        }
    }

    fn peek(&self, item: &CompactCacheKey) -> bool {
        let key = u64key(item);
        self.lru.read().peek(&key).is_some()
    }

    async fn save(&self, dir_path: &str) -> Result<()> {
        let data = self.serialize()?;
        let dir_str = dir_path.to_owned();
        tokio::task::spawn_blocking(move || {
            let dir_path = Path::new(&dir_str);
            std::fs::create_dir_all(dir_path)
                .or_err_with(InternalError, || format!("fail to create {dir_str}"))?;
            let file_path = dir_path.join(FILE_NAME);
            let mut file = File::create(&file_path).or_err_with(InternalError, || {
                format!("fail to create {}", file_path.display())
            })?;
            file.write_all(&data).or_err_with(InternalError, || {
                format!("fail to write to {}", file_path.display())
            })
        })
        .await
        .or_err(InternalError, "async blocking IO failure")?
    }

    async fn load(&self, dir_path: &str) -> Result<()> {
        let dir_path = dir_path.to_owned();
        let data = tokio::task::spawn_blocking(move || {
            let file_path = Path::new(&dir_path).join(FILE_NAME);
            let mut file = File::open(file_path.clone()).or_err_with(InternalError, || {
                format!("fail to open {}", file_path.display())
            })?;
            let mut buffer = Vec::with_capacity(8192);
            file.read_to_end(&mut buffer)
                .or_err(InternalError, "fail to read from {file_path}")?;
            Ok::<Vec<u8>, BError>(buffer)
        })
        .await
        .or_err(InternalError, "async blocking IO failure")??;
        self.deserialize(&data)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::CacheKey;

    #[test]
    fn test_admission() {
        let lru = Manager::new(4);
        let key1 = CacheKey::new("", "a", "1").to_compact();
        let until = SystemTime::now(); // unused value as a placeholder
        let v = lru.admit(key1.clone(), 1, until);
        assert_eq!(v.len(), 0);
        let key2 = CacheKey::new("", "b", "1").to_compact();
        let v = lru.admit(key2.clone(), 2, until);
        assert_eq!(v.len(), 0);
        let key3 = CacheKey::new("", "c", "1").to_compact();
        let v = lru.admit(key3, 1, until);
        assert_eq!(v.len(), 0);

        // lru si full (4) now

        let key4 = CacheKey::new("", "d", "1").to_compact();
        let v = lru.admit(key4, 2, until);
        // need to reduce used by at least 2, both key1 and key2 are evicted to make room for 3
        assert_eq!(v.len(), 2);
        assert_eq!(v[0], key1);
        assert_eq!(v[1], key2);
    }

    #[test]
    fn test_access() {
        let lru = Manager::new(4);
        let key1 = CacheKey::new("", "a", "1").to_compact();
        let until = SystemTime::now(); // unused value as a placeholder
        let v = lru.admit(key1.clone(), 1, until);
        assert_eq!(v.len(), 0);
        let key2 = CacheKey::new("", "b", "1").to_compact();
        let v = lru.admit(key2.clone(), 2, until);
        assert_eq!(v.len(), 0);
        let key3 = CacheKey::new("", "c", "1").to_compact();
        let v = lru.admit(key3, 1, until);
        assert_eq!(v.len(), 0);

        // lru is full (4) now
        // make key1 most recently used
        lru.access(&key1, 1, until);
        assert_eq!(v.len(), 0);

        let key4 = CacheKey::new("", "d", "1").to_compact();
        let v = lru.admit(key4, 2, until);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0], key2);
    }

    #[test]
    fn test_remove() {
        let lru = Manager::new(4);
        let key1 = CacheKey::new("", "a", "1").to_compact();
        let until = SystemTime::now(); // unused value as a placeholder
        let v = lru.admit(key1.clone(), 1, until);
        assert_eq!(v.len(), 0);
        let key2 = CacheKey::new("", "b", "1").to_compact();
        let v = lru.admit(key2.clone(), 2, until);
        assert_eq!(v.len(), 0);
        let key3 = CacheKey::new("", "c", "1").to_compact();
        let v = lru.admit(key3, 1, until);
        assert_eq!(v.len(), 0);

        // lru is full (4) now
        // remove key1
        lru.remove(&key1);

        // key2 is the least recently used one now
        let key4 = CacheKey::new("", "d", "1").to_compact();
        let v = lru.admit(key4, 2, until);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0], key2);
    }

    #[test]
    fn test_access_add() {
        let lru = Manager::new(4);
        let until = SystemTime::now(); // unused value as a placeholder

        let key1 = CacheKey::new("", "a", "1").to_compact();
        lru.access(&key1, 1, until);
        let key2 = CacheKey::new("", "b", "1").to_compact();
        lru.access(&key2, 2, until);
        let key3 = CacheKey::new("", "c", "1").to_compact();
        lru.access(&key3, 2, until);

        let key4 = CacheKey::new("", "d", "1").to_compact();
        let v = lru.admit(key4, 2, until);
        // need to reduce used by at least 2, both key1 and key2 are evicted to make room for 3
        assert_eq!(v.len(), 2);
        assert_eq!(v[0], key1);
        assert_eq!(v[1], key2);
    }

    #[test]
    fn test_admit_update() {
        let lru = Manager::new(4);
        let key1 = CacheKey::new("", "a", "1").to_compact();
        let until = SystemTime::now(); // unused value as a placeholder
        let v = lru.admit(key1.clone(), 1, until);
        assert_eq!(v.len(), 0);
        let key2 = CacheKey::new("", "b", "1").to_compact();
        let v = lru.admit(key2.clone(), 2, until);
        assert_eq!(v.len(), 0);
        let key3 = CacheKey::new("", "c", "1").to_compact();
        let v = lru.admit(key3, 1, until);
        assert_eq!(v.len(), 0);

        // lru is full (4) now
        // update key2 to reduce its size by 1
        let v = lru.admit(key2, 1, until);
        assert_eq!(v.len(), 0);

        // lru is not full anymore
        let key4 = CacheKey::new("", "d", "1").to_compact();
        let v = lru.admit(key4.clone(), 1, until);
        assert_eq!(v.len(), 0);

        // make key4 larger
        let v = lru.admit(key4, 2, until);
        // need to evict now
        assert_eq!(v.len(), 1);
        assert_eq!(v[0], key1);
    }

    #[test]
    fn test_serde() {
        let lru = Manager::new(4);
        let key1 = CacheKey::new("", "a", "1").to_compact();
        let until = SystemTime::now(); // unused value as a placeholder
        let v = lru.admit(key1.clone(), 1, until);
        assert_eq!(v.len(), 0);
        let key2 = CacheKey::new("", "b", "1").to_compact();
        let v = lru.admit(key2.clone(), 2, until);
        assert_eq!(v.len(), 0);
        let key3 = CacheKey::new("", "c", "1").to_compact();
        let v = lru.admit(key3, 1, until);
        assert_eq!(v.len(), 0);

        // lru is full (4) now
        // make key1 most recently used
        lru.access(&key1, 1, until);
        assert_eq!(v.len(), 0);

        // load lru2 with lru's data
        let ser = lru.serialize().unwrap();
        let lru2 = Manager::new(4);
        lru2.deserialize(&ser).unwrap();

        let key4 = CacheKey::new("", "d", "1").to_compact();
        let v = lru2.admit(key4, 2, until);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0], key2);
    }

    #[tokio::test]
    async fn test_save_to_disk() {
        let lru = Manager::new(4);
        let key1 = CacheKey::new("", "a", "1").to_compact();
        let until = SystemTime::now(); // unused value as a placeholder
        let v = lru.admit(key1.clone(), 1, until);
        assert_eq!(v.len(), 0);
        let key2 = CacheKey::new("", "b", "1").to_compact();
        let v = lru.admit(key2.clone(), 2, until);
        assert_eq!(v.len(), 0);
        let key3 = CacheKey::new("", "c", "1").to_compact();
        let v = lru.admit(key3, 1, until);
        assert_eq!(v.len(), 0);

        // lru is full (4) now
        // make key1 most recently used
        lru.access(&key1, 1, until);
        assert_eq!(v.len(), 0);

        // load lru2 with lru's data
        lru.save("/tmp/test_simple_lru_save").await.unwrap();
        let lru2 = Manager::new(4);
        lru2.load("/tmp/test_simple_lru_save").await.unwrap();

        let key4 = CacheKey::new("", "d", "1").to_compact();
        let v = lru2.admit(key4, 2, until);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0], key2);
    }
}
