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

//! A shared LRU cache manager

use super::EvictionManager;
use crate::key::CompactCacheKey;

use async_trait::async_trait;
use pingora_error::{BError, ErrorType::*, OrErr, Result};
use pingora_lru::Lru;
use serde::de::SeqAccess;
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::hash::{Hash, Hasher};
use std::io::prelude::*;
use std::path::Path;
use std::time::SystemTime;

/// A shared LRU cache manager designed to manage a large volume of assets.
///
/// - Space optimized in-memory LRU (see [pingora_lru]).
/// - Instead of a single giant LRU, this struct shards the assets into `N` independent LRUs.
///
/// This allows [EvictionManager::save()] not to lock the entire cache manager while performing
/// serialization.
pub struct Manager<const N: usize>(Lru<CompactCacheKey, N>);

#[derive(Debug, Serialize, Deserialize)]
struct SerdeHelperNode(CompactCacheKey, usize);

impl<const N: usize> Manager<N> {
    /// Create a [Manager] with the given size limit and estimated per shard capacity.
    ///
    /// The `capacity` is for preallocating to avoid reallocation cost when the LRU grows.
    pub fn with_capacity(limit: usize, capacity: usize) -> Self {
        Manager(Lru::with_capacity(limit, capacity))
    }

    /// Serialize the given shard
    pub fn serialize_shard(&self, shard: usize) -> Result<Vec<u8>> {
        use rmp_serde::encode::Serializer;
        use serde::ser::SerializeSeq;
        use serde::ser::Serializer as _;

        assert!(shard < N);

        // NOTE: This could use a lot of memory to buffer the serialized data in memory
        // NOTE: This for loop could lock the LRU for too long
        let mut nodes = Vec::with_capacity(self.0.shard_len(shard));
        self.0.iter_for_each(shard, |(node, size)| {
            nodes.push(SerdeHelperNode(node.clone(), size));
        });
        let mut ser = Serializer::new(vec![]);
        let mut seq = ser
            .serialize_seq(Some(self.0.shard_len(shard)))
            .or_err(InternalError, "fail to serialize node")?;
        for node in nodes {
            seq.serialize_element(&node).unwrap(); // write to vec, safe
        }

        seq.end().or_err(InternalError, "when serializing LRU")?;
        Ok(ser.into_inner())
    }

    /// Deserialize a shard
    ///
    /// Shard number is not needed because the key itself will hash to the correct shard.
    pub fn deserialize_shard(&self, buf: &[u8]) -> Result<()> {
        use rmp_serde::decode::Deserializer;
        use serde::de::Deserializer as _;

        let mut de = Deserializer::new(buf);
        let visitor = InsertToManager { lru: self };
        de.deserialize_seq(visitor)
            .or_err(InternalError, "when deserializing LRU")?;
        Ok(())
    }
}

struct InsertToManager<'a, const N: usize> {
    lru: &'a Manager<N>,
}

impl<'de, const N: usize> serde::de::Visitor<'de> for InsertToManager<'_, N> {
    type Value = ();

    fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
        formatter.write_str("array of lru nodes")
    }

    fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        while let Some(node) = seq.next_element::<SerdeHelperNode>()? {
            let key = u64key(&node.0);
            self.lru.0.insert_tail(key, node.0, node.1); // insert in the back
        }
        Ok(())
    }
}

#[inline]
fn u64key(key: &CompactCacheKey) -> u64 {
    // note that std hash is not uniform, I'm not sure if ahash is also the case
    let mut hasher = ahash::AHasher::default();
    key.hash(&mut hasher);
    hasher.finish()
}

const FILE_NAME: &str = "lru.data";

#[inline]
fn err_str_path(s: &str, path: &Path) -> String {
    format!("{s} {}", path.display())
}

#[async_trait]
impl<const N: usize> EvictionManager for Manager<N> {
    fn total_size(&self) -> usize {
        self.0.weight()
    }
    fn total_items(&self) -> usize {
        self.0.len()
    }
    fn evicted_size(&self) -> usize {
        self.0.evicted_weight()
    }
    fn evicted_items(&self) -> usize {
        self.0.evicted_len()
    }

    fn admit(
        &self,
        item: CompactCacheKey,
        size: usize,
        _fresh_until: SystemTime,
    ) -> Vec<CompactCacheKey> {
        let key = u64key(&item);
        self.0.admit(key, item, size);
        self.0
            .evict_to_limit()
            .into_iter()
            .map(|(key, _weight)| key)
            .collect()
    }

    fn increment_weight(&self, item: CompactCacheKey, delta: usize) -> Vec<CompactCacheKey> {
        let key = u64key(&item);
        self.0.increment_weight(key, delta);
        self.0
            .evict_to_limit()
            .into_iter()
            .map(|(key, _weight)| key)
            .collect()
    }

    fn remove(&self, item: &CompactCacheKey) {
        let key = u64key(item);
        self.0.remove(key);
    }

    fn access(&self, item: &CompactCacheKey, size: usize, _fresh_until: SystemTime) -> bool {
        let key = u64key(item);
        if !self.0.promote(key) {
            self.0.admit(key, item.clone(), size);
            false
        } else {
            true
        }
    }

    fn peek(&self, item: &CompactCacheKey) -> bool {
        let key = u64key(item);
        self.0.peek(key)
    }

    async fn save(&self, dir_path: &str) -> Result<()> {
        let dir_path_str = dir_path.to_owned();

        tokio::task::spawn_blocking(move || {
            let dir_path = Path::new(&dir_path_str);
            std::fs::create_dir_all(dir_path)
                .or_err_with(InternalError, || err_str_path("fail to create", dir_path))
        })
        .await
        .or_err(InternalError, "async blocking IO failure")??;

        for i in 0..N {
            let data = self.serialize_shard(i)?;
            let dir_path = dir_path.to_owned();
            tokio::task::spawn_blocking(move || {
                let file_path = Path::new(&dir_path).join(format!("{}.{i}", FILE_NAME));
                let mut file = File::create(&file_path)
                    .or_err_with(InternalError, || err_str_path("fail to create", &file_path))?;
                file.write_all(&data).or_err_with(InternalError, || {
                    err_str_path("fail to write to", &file_path)
                })
            })
            .await
            .or_err(InternalError, "async blocking IO failure")??;
        }
        Ok(())
    }

    async fn load(&self, dir_path: &str) -> Result<()> {
        // TODO: check the saved shards so that we load all the save files
        for i in 0..N {
            let dir_path = dir_path.to_owned();

            let data = tokio::task::spawn_blocking(move || {
                let file_path = Path::new(&dir_path).join(format!("{}.{i}", FILE_NAME));
                let mut file = File::open(&file_path)
                    .or_err_with(InternalError, || err_str_path("fail to open", &file_path))?;
                let mut buffer = Vec::with_capacity(8192);
                file.read_to_end(&mut buffer)
                    .or_err_with(InternalError, || {
                        err_str_path("fail to read from", &file_path)
                    })?;
                Ok::<Vec<u8>, BError>(buffer)
            })
            .await
            .or_err(InternalError, "async blocking IO failure")??;
            self.deserialize_shard(&data)?;
        }

        Ok(())
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::CacheKey;

    // we use shard (N) = 1 for eviction consistency in all tests

    #[test]
    fn test_admission() {
        let lru = Manager::<1>::with_capacity(4, 10);
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
        let lru = Manager::<1>::with_capacity(4, 10);
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
        let lru = Manager::<1>::with_capacity(4, 10);
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
        let lru = Manager::<1>::with_capacity(4, 10);
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
        let lru = Manager::<1>::with_capacity(4, 10);
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
    fn test_peek() {
        let lru = Manager::<1>::with_capacity(4, 10);
        let until = SystemTime::now(); // unused value as a placeholder

        let key1 = CacheKey::new("", "a", "1").to_compact();
        lru.access(&key1, 1, until);
        let key2 = CacheKey::new("", "b", "1").to_compact();
        lru.access(&key2, 2, until);
        assert!(lru.peek(&key1));
        assert!(lru.peek(&key2));
    }

    #[test]
    fn test_serde() {
        let lru = Manager::<1>::with_capacity(4, 10);
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
        let ser = lru.serialize_shard(0).unwrap();
        let lru2 = Manager::<1>::with_capacity(4, 10);
        lru2.deserialize_shard(&ser).unwrap();

        let key4 = CacheKey::new("", "d", "1").to_compact();
        let v = lru2.admit(key4, 2, until);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0], key2);
    }

    #[tokio::test]
    async fn test_save_to_disk() {
        let until = SystemTime::now(); // unused value as a placeholder
        let lru = Manager::<2>::with_capacity(10, 10);

        lru.admit(CacheKey::new("", "a", "1").to_compact(), 1, until);
        lru.admit(CacheKey::new("", "b", "1").to_compact(), 2, until);
        lru.admit(CacheKey::new("", "c", "1").to_compact(), 1, until);
        lru.admit(CacheKey::new("", "d", "1").to_compact(), 1, until);
        lru.admit(CacheKey::new("", "e", "1").to_compact(), 2, until);
        lru.admit(CacheKey::new("", "f", "1").to_compact(), 1, until);

        // load lru2 with lru's data
        lru.save("/tmp/test_lru_save").await.unwrap();
        let lru2 = Manager::<2>::with_capacity(4, 10);
        lru2.load("/tmp/test_lru_save").await.unwrap();

        let ser0 = lru.serialize_shard(0).unwrap();
        let ser1 = lru.serialize_shard(1).unwrap();

        assert_eq!(ser0, lru2.serialize_shard(0).unwrap());
        assert_eq!(ser1, lru2.serialize_shard(1).unwrap());
    }
}
