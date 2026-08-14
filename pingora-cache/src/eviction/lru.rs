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

//! A shared LRU cache manager

use super::{CacheEntryKey, CacheEntryKeyRef, EvictionManager};
#[cfg(test)]
use crate::key::CompactCacheKey;

use async_trait::async_trait;
use pingora_error::{ErrorType::*, OrErr, Result};
use pingora_lru::{persistence, Lru};
use serde::de::SeqAccess;
use serde::{Deserialize, Serialize};
use std::hash::{Hash, Hasher};
use std::time::SystemTime;

/// A shared LRU cache manager designed to manage a large volume of assets.
///
/// - Space optimized in-memory LRU (see [pingora_lru]).
/// - Instead of a single giant LRU, this struct shards the assets into `N` independent LRUs.
///
/// This allows [EvictionManager::save()] not to lock the entire cache manager while performing
/// serialization.
pub struct Manager<const N: usize>(Lru<CacheEntryKey, N>);

#[derive(Debug, Serialize, Deserialize)]
struct SerdeHelperNode(CacheEntryKey, usize);

impl<const N: usize> Manager<N> {
    /// Create a [Manager] with the given size limit and estimated per shard capacity.
    ///
    /// The `capacity` is for preallocating to avoid reallocation cost when the LRU grows.
    pub fn with_capacity(limit: usize, capacity: usize) -> Self {
        Manager(Lru::with_capacity(limit, capacity))
    }

    /// Create a [Manager] with an optional watermark in addition to weight limit.
    ///
    /// When `watermark` is set, the underlying LRU will also evict to keep total item count
    /// under or equal to that watermark.
    pub fn with_capacity_and_watermark(
        limit: usize,
        capacity: usize,
        watermark: Option<usize>,
    ) -> Self {
        Manager(Lru::with_capacity_and_watermark(limit, capacity, watermark))
    }

    /// Return the current total cache weight limit.
    pub fn weight_limit(&self) -> usize {
        self.0.weight_limit()
    }

    /// Set the total cache weight limit used by future eviction decisions.
    pub fn set_weight_limit(&self, limit: usize) {
        self.0.set_weight_limit(limit);
    }

    /// Get the number of shards
    pub fn shards(&self) -> usize {
        self.0.shards()
    }

    /// Get the weight (total size) of a specific shard
    pub fn shard_weight(&self, shard: usize) -> usize {
        self.0.shard_weight(shard)
    }

    /// Get the number of items in a specific shard. Best-effort
    /// lock-free read; see [`pingora_lru::Lru::shard_len`] for the
    /// consistency semantics.
    pub fn shard_len(&self, shard: usize) -> usize {
        self.0.shard_len(shard)
    }

    /// Get the shard index for a given cache entry
    ///
    /// This allows callers to know which shard was affected by an operation
    /// without acquiring any locks.
    pub fn get_shard_for_key(&self, key: &CacheEntryKey) -> usize {
        (u64key(key) % N as u64) as usize
    }

    /// Peek at the least-recently-used key in the given shard without evicting it.
    ///
    /// Returns the cache entry at the LRU tail of the shard, or `None` if empty.
    /// Useful for reporting the eviction frontier (the age of the next item
    /// that would be evicted).
    pub fn peek_lru(&self, shard: usize) -> Option<CacheEntryKey> {
        self.0.peek_lru(shard).map(|(key, _weight)| key)
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
        // Use the captured snapshot length for the MessagePack array header.
        // Re-reading shard_len() here can race with concurrent mutations and
        // emit a length that does not match the serialized nodes.
        let mut seq = ser
            .serialize_seq(Some(nodes.len()))
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

    /// Peek the weight associated with a cache entry without changing its LRU order.
    pub fn peek_weight(&self, item: &CacheEntryKey) -> Option<usize> {
        let key = u64key(item);
        self.0.peek_weight(key)
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
fn u64key(key: &impl Hash) -> u64 {
    // note that std hash is not uniform, I'm not sure if ahash is also the case
    let mut hasher = ahash::AHasher::default();
    key.hash(&mut hasher);
    hasher.finish()
}

const FILE_NAME: &str = "lru.data";

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
        item: CacheEntryKey,
        size: usize,
        _fresh_until: SystemTime,
    ) -> Vec<CacheEntryKey> {
        let key = u64key(&item);
        self.0.admit(key, item, size);
        self.0
            .evict_to_limit()
            .into_iter()
            .map(|(key, _weight)| key)
            .collect()
    }

    fn increment_weight(
        &self,
        item: &CacheEntryKey,
        delta: usize,
        max_weight: Option<usize>,
    ) -> Vec<CacheEntryKey> {
        let key = u64key(item);
        self.0
            .increment_weight(key, || item.clone(), delta, max_weight);
        self.0
            .evict_to_limit()
            .into_iter()
            .map(|(key, _weight)| key)
            .collect()
    }

    fn remove(&self, item: CacheEntryKeyRef<'_>) {
        let key = u64key(&item);
        self.0.remove(key);
    }

    fn access(&self, item: &CacheEntryKey, size: usize, _fresh_until: SystemTime) -> bool {
        let key = u64key(item);
        if !self.0.promote(key) {
            self.0.admit(key, item.clone(), size);
            false
        } else {
            true
        }
    }

    fn peek(&self, item: &CacheEntryKey) -> bool {
        let key = u64key(item);
        self.0.peek(key)
    }

    async fn save(&self, dir_path: &str) -> Result<()> {
        persistence::save_shards(dir_path, FILE_NAME, N, |i| self.serialize_shard(i))
            .await
            .or_err(InternalError, "failed to save LRU")?;
        Ok(())
    }

    async fn load(&self, dir_path: &str) -> Result<()> {
        persistence::load_shards(dir_path, FILE_NAME, N, |_i, data| {
            self.deserialize_shard(data)
        })
        .await;
        Ok(())
    }
}

#[cfg(test)]
impl<const N: usize> Manager<N> {
    fn admit(
        &self,
        item: CompactCacheKey,
        size: usize,
        fresh_until: SystemTime,
    ) -> Vec<CompactCacheKey> {
        EvictionManager::admit(self, CacheEntryKey::key_only(item), size, fresh_until)
            .into_iter()
            .map(CacheEntryKey::into_key)
            .collect()
    }

    fn increment_weight(
        &self,
        item: &CompactCacheKey,
        delta: usize,
        max_weight: Option<usize>,
    ) -> Vec<CompactCacheKey> {
        EvictionManager::increment_weight(
            self,
            &CacheEntryKey::key_only(item.clone()),
            delta,
            max_weight,
        )
        .into_iter()
        .map(CacheEntryKey::into_key)
        .collect()
    }

    fn remove(&self, item: &CompactCacheKey) {
        EvictionManager::remove(self, CacheEntryKeyRef::from_entry_id(item, None));
    }

    fn access(&self, item: &CompactCacheKey, size: usize, fresh_until: SystemTime) -> bool {
        EvictionManager::access(
            self,
            &CacheEntryKey::key_only(item.clone()),
            size,
            fresh_until,
        )
    }

    fn peek(&self, item: &CompactCacheKey) -> bool {
        EvictionManager::peek(self, &CacheEntryKey::key_only(item.clone()))
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::CacheKey;
    use std::path::Path;

    // we use shard (N) = 1 for eviction consistency in all tests

    #[test]
    fn test_admission() {
        let lru = Manager::<1>::with_capacity(4, 10);
        let key1 = CacheKey::new("a", "1").to_compact();
        let until = SystemTime::now(); // unused value as a placeholder
        let v = lru.admit(key1.clone(), 1, until);
        assert_eq!(v.len(), 0);
        let key2 = CacheKey::new("b", "1").to_compact();
        let v = lru.admit(key2.clone(), 2, until);
        assert_eq!(v.len(), 0);
        let key3 = CacheKey::new("c", "1").to_compact();
        let v = lru.admit(key3, 1, until);
        assert_eq!(v.len(), 0);

        // lru si full (4) now

        let key4 = CacheKey::new("d", "1").to_compact();
        let v = lru.admit(key4, 2, until);
        // need to reduce used by at least 2, both key1 and key2 are evicted to make room for 3
        assert_eq!(v.len(), 2);
        assert_eq!(v[0], key1);
        assert_eq!(v[1], key2);
    }

    #[test]
    fn test_identified_entries_are_distinct() {
        let lru = Manager::<1>::with_capacity(1, 2);
        let key = CacheKey::new("a", "1").to_compact();
        let first = CacheEntryKey::identified(key.clone(), crate::CacheEntryId::new(1));
        let second = CacheEntryKey::identified(key, crate::CacheEntryId::new(2));
        let until = SystemTime::now();

        assert!(EvictionManager::admit(&lru, first.clone(), 1, until).is_empty());
        assert_eq!(
            EvictionManager::admit(&lru, second.clone(), 1, until),
            vec![first]
        );
        assert_eq!(lru.peek_lru(0), Some(second));
    }

    #[test]
    fn test_set_weight_limit() {
        let lru = Manager::<1>::with_capacity(4, 10);
        let until = SystemTime::now();
        let key1 = CacheKey::new("a", "1").to_compact();
        let key2 = CacheKey::new("b", "1").to_compact();
        let key3 = CacheKey::new("c", "1").to_compact();
        assert_eq!(lru.weight_limit(), 4);
        assert!(lru.admit(key1.clone(), 1, until).is_empty());
        assert!(lru.admit(key2.clone(), 1, until).is_empty());

        lru.set_weight_limit(1);
        assert_eq!(lru.weight_limit(), 1);
        let evicted = lru.admit(key3, 1, until);
        assert_eq!(evicted, vec![key1, key2]);

        lru.set_weight_limit(10);
        assert_eq!(lru.weight_limit(), 10);
    }

    #[test]
    fn test_access() {
        let lru = Manager::<1>::with_capacity(4, 10);
        let key1 = CacheKey::new("a", "1").to_compact();
        let until = SystemTime::now(); // unused value as a placeholder
        let v = lru.admit(key1.clone(), 1, until);
        assert_eq!(v.len(), 0);
        let key2 = CacheKey::new("b", "1").to_compact();
        let v = lru.admit(key2.clone(), 2, until);
        assert_eq!(v.len(), 0);
        let key3 = CacheKey::new("c", "1").to_compact();
        let v = lru.admit(key3, 1, until);
        assert_eq!(v.len(), 0);

        // lru is full (4) now
        // make key1 most recently used
        lru.access(&key1, 1, until);
        assert_eq!(v.len(), 0);

        let key4 = CacheKey::new("d", "1").to_compact();
        let v = lru.admit(key4, 2, until);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0], key2);
    }

    #[test]
    fn test_remove() {
        let lru = Manager::<1>::with_capacity(4, 10);
        let key1 = CacheKey::new("a", "1").to_compact();
        let until = SystemTime::now(); // unused value as a placeholder
        let v = lru.admit(key1.clone(), 1, until);
        assert_eq!(v.len(), 0);
        let key2 = CacheKey::new("b", "1").to_compact();
        let v = lru.admit(key2.clone(), 2, until);
        assert_eq!(v.len(), 0);
        let key3 = CacheKey::new("c", "1").to_compact();
        let v = lru.admit(key3, 1, until);
        assert_eq!(v.len(), 0);

        // lru is full (4) now
        // remove key1
        lru.remove(&key1);

        // key2 is the least recently used one now
        let key4 = CacheKey::new("d", "1").to_compact();
        let v = lru.admit(key4, 2, until);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0], key2);
    }

    #[test]
    fn test_access_add() {
        let lru = Manager::<1>::with_capacity(4, 10);
        let until = SystemTime::now(); // unused value as a placeholder

        let key1 = CacheKey::new("a", "1").to_compact();
        lru.access(&key1, 1, until);
        let key2 = CacheKey::new("b", "1").to_compact();
        lru.access(&key2, 2, until);
        let key3 = CacheKey::new("c", "1").to_compact();
        lru.access(&key3, 2, until);

        let key4 = CacheKey::new("d", "1").to_compact();
        let v = lru.admit(key4, 2, until);
        // need to reduce used by at least 2, both key1 and key2 are evicted to make room for 3
        assert_eq!(v.len(), 2);
        assert_eq!(v[0], key1);
        assert_eq!(v[1], key2);
    }

    #[test]
    fn test_increment_weight_adds_missing_item() {
        let lru = Manager::<1>::with_capacity(4, 10);

        let key1 = CacheKey::new("a", "1").to_compact();
        assert!(lru.increment_weight(&key1, 2, None).is_empty());
        assert!(lru.peek(&key1));
        assert_eq!(lru.total_size(), 2);
        assert_eq!(lru.total_items(), 1);

        let key2 = CacheKey::new("b", "1").to_compact();
        let evicted = lru.increment_weight(&key2, 100, Some(3));
        assert_eq!(evicted, vec![key1]);
        assert!(lru.peek(&key2));
        assert_eq!(lru.total_size(), 3);
        assert_eq!(lru.total_items(), 1);
    }

    #[test]
    fn test_increment_weight_admits_zero_and_does_not_shrink() {
        let lru = Manager::<1>::with_capacity(10, 10);

        let key1 = CacheKey::new("a", "1").to_compact();
        assert!(lru.increment_weight(&key1, 0, None).is_empty());
        assert!(lru.peek(&key1));
        assert_eq!(lru.total_size(), 1);

        let key2 = CacheKey::new("b", "1").to_compact();
        assert!(lru.increment_weight(&key2, 3, None).is_empty());
        assert!(lru.increment_weight(&key2, 100, Some(2)).is_empty());
        assert_eq!(lru.total_size(), 4);
        assert_eq!(lru.total_items(), 2);
    }

    #[test]
    fn test_admit_update() {
        let lru = Manager::<1>::with_capacity(4, 10);
        let key1 = CacheKey::new("a", "1").to_compact();
        let until = SystemTime::now(); // unused value as a placeholder
        let v = lru.admit(key1.clone(), 1, until);
        assert_eq!(v.len(), 0);
        let key2 = CacheKey::new("b", "1").to_compact();
        let v = lru.admit(key2.clone(), 2, until);
        assert_eq!(v.len(), 0);
        let key3 = CacheKey::new("c", "1").to_compact();
        let v = lru.admit(key3, 1, until);
        assert_eq!(v.len(), 0);

        // lru is full (4) now
        // update key2 to reduce its size by 1
        let v = lru.admit(key2, 1, until);
        assert_eq!(v.len(), 0);

        // lru is not full anymore
        let key4 = CacheKey::new("d", "1").to_compact();
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

        let key1 = CacheKey::new("a", "1").to_compact();
        lru.access(&key1, 1, until);
        let key2 = CacheKey::new("b", "1").to_compact();
        lru.access(&key2, 2, until);
        assert!(lru.peek(&key1));
        assert!(lru.peek(&key2));
    }

    #[test]
    fn test_serde() {
        let lru = Manager::<1>::with_capacity(4, 10);
        let key1 = CacheKey::new("a", "1").to_compact();
        let until = SystemTime::now(); // unused value as a placeholder
        let v = lru.admit(key1.clone(), 1, until);
        assert_eq!(v.len(), 0);
        let key2 = CacheKey::new("b", "1").to_compact();
        let v = lru.admit(key2.clone(), 2, until);
        assert_eq!(v.len(), 0);
        let key3 = CacheKey::new("c", "1").to_compact();
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

        let key4 = CacheKey::new("d", "1").to_compact();
        let v = lru2.admit(key4, 2, until);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0], key2);
    }

    #[tokio::test]
    async fn test_save_to_disk() {
        let until = SystemTime::now(); // unused value as a placeholder
        let lru = Manager::<2>::with_capacity(10, 10);

        lru.admit(CacheKey::new("a", "1").to_compact(), 1, until);
        lru.admit(CacheKey::new("b", "1").to_compact(), 2, until);
        lru.admit(CacheKey::new("c", "1").to_compact(), 1, until);
        lru.admit(CacheKey::new("d", "1").to_compact(), 1, until);
        lru.admit(CacheKey::new("e", "1").to_compact(), 2, until);
        lru.admit(CacheKey::new("f", "1").to_compact(), 1, until);

        // load lru2 with lru's data
        lru.save("/tmp/test_lru_save").await.unwrap();
        let lru2 = Manager::<2>::with_capacity(4, 10);
        lru2.load("/tmp/test_lru_save").await.unwrap();

        let ser0 = lru.serialize_shard(0).unwrap();
        let ser1 = lru.serialize_shard(1).unwrap();

        assert_eq!(ser0, lru2.serialize_shard(0).unwrap());
        assert_eq!(ser1, lru2.serialize_shard(1).unwrap());
    }

    #[tokio::test]
    async fn test_load_no_shards() {
        // Loading from an empty directory should succeed with an empty LRU.
        let test_dir = "/tmp/test_lru_no_shards";
        let _ = std::fs::remove_dir_all(test_dir);
        std::fs::create_dir_all(test_dir).unwrap();

        let lru = Manager::<4>::with_capacity(10, 10);
        lru.load(test_dir).await.unwrap();
        assert_eq!(lru.total_items(), 0);
        assert_eq!(lru.total_size(), 0);

        std::fs::remove_dir_all(test_dir).unwrap();
    }

    #[tokio::test]
    async fn test_load_partial_shards() {
        // A subset of shard files is missing on disk. Load should succeed and
        // populate the LRU from only the shards that exist; missing shards are
        // treated as empty rather than aborting the load.
        let test_dir = "/tmp/test_lru_partial_shards";
        let _ = std::fs::remove_dir_all(test_dir);
        std::fs::create_dir_all(test_dir).unwrap();

        let until = SystemTime::now();
        let src = Manager::<4>::with_capacity(100, 100);
        for i in 0..16 {
            src.admit(CacheKey::new(format!("k{i}"), "1").to_compact(), 1, until);
        }
        src.save(test_dir).await.unwrap();
        let baseline = src.total_items();
        assert!(baseline > 0);

        // Remove half the shard files.
        std::fs::remove_file(format!("{test_dir}/lru.data.1")).unwrap();
        std::fs::remove_file(format!("{test_dir}/lru.data.3")).unwrap();

        let dst = Manager::<4>::with_capacity(100, 100);
        dst.load(test_dir).await.unwrap();
        // Some entries loaded, but fewer than the baseline.
        assert!(dst.total_items() > 0);
        assert!(dst.total_items() < baseline);

        std::fs::remove_dir_all(test_dir).unwrap();
    }

    #[tokio::test]
    async fn test_save_partial_failure_continues() {
        // A per-shard rename failure must not abort the whole save. We simulate
        // by pre-creating a directory at one shard's final path so that shard's
        // atomic rename fails while the others succeed.
        let test_dir = "/tmp/test_lru_save_partial_fail";
        let _ = std::fs::remove_dir_all(test_dir);
        std::fs::create_dir_all(test_dir).unwrap();
        std::fs::create_dir(format!("{test_dir}/lru.data.1")).unwrap();

        let until = SystemTime::now();
        let src = Manager::<4>::with_capacity(100, 100);
        for i in 0..16 {
            src.admit(CacheKey::new(format!("k{i}"), "1").to_compact(), 1, until);
        }
        src.save(test_dir).await.unwrap();

        // Shards 0, 2, 3 should be regular files; shard 1 is still a directory.
        for i in [0, 2, 3] {
            let p = format!("{test_dir}/lru.data.{i}");
            let meta = std::fs::metadata(&p).unwrap();
            assert!(meta.is_file(), "shard {i} should be a regular file");
            assert!(meta.len() > 0, "shard {i} should be non-empty");
        }
        assert!(std::fs::metadata(format!("{test_dir}/lru.data.1"))
            .unwrap()
            .is_dir());

        std::fs::remove_dir(format!("{test_dir}/lru.data.1")).unwrap();
        std::fs::remove_dir_all(test_dir).unwrap();
    }

    #[tokio::test]
    async fn test_save_total_failure_returns_err() {
        // If every shard fails to save, the function must return Err so callers
        // can alarm on the catastrophic case.
        let test_dir = "/tmp/test_lru_save_total_fail";
        let _ = std::fs::remove_dir_all(test_dir);
        std::fs::create_dir_all(test_dir).unwrap();
        for i in 0..4 {
            std::fs::create_dir(format!("{test_dir}/lru.data.{i}")).unwrap();
        }

        let until = SystemTime::now();
        let src = Manager::<4>::with_capacity(100, 100);
        src.admit(CacheKey::new("k", "1").to_compact(), 1, until);

        let err = src.save(test_dir).await.unwrap_err();
        assert!(
            err.to_string().contains("All 4 shards failed to save"),
            "unexpected error message: {err}"
        );

        for i in 0..4 {
            std::fs::remove_dir(format!("{test_dir}/lru.data.{i}")).unwrap();
        }
        std::fs::remove_dir_all(test_dir).unwrap();
    }

    #[tokio::test]
    async fn test_load_unreadable_shard_continues() {
        // A shard path that opens successfully but fails to read (here a
        // directory at the shard path) should not abort load of other shards.
        let test_dir = "/tmp/test_lru_unreadable_shard";
        let _ = std::fs::remove_dir_all(test_dir);
        std::fs::create_dir_all(test_dir).unwrap();

        let until = SystemTime::now();
        let src = Manager::<4>::with_capacity(100, 100);
        for i in 0..16 {
            src.admit(CacheKey::new(format!("k{i}"), "1").to_compact(), 1, until);
        }
        src.save(test_dir).await.unwrap();

        // Replace one shard with a directory so File::open succeeds but
        // read_to_end returns an error.
        std::fs::remove_file(format!("{test_dir}/lru.data.2")).unwrap();
        std::fs::create_dir(format!("{test_dir}/lru.data.2")).unwrap();

        let dst = Manager::<4>::with_capacity(100, 100);
        dst.load(test_dir).await.unwrap();
        // Entries from the other 3 shards still loaded.
        assert!(dst.total_items() > 0);

        std::fs::remove_dir(format!("{test_dir}/lru.data.2")).unwrap();
        std::fs::remove_dir_all(test_dir).unwrap();
    }

    #[tokio::test]
    async fn test_load_corrupt_shard_continues() {
        // A corrupt shard file should not abort load of the remaining shards.
        let test_dir = "/tmp/test_lru_corrupt_shard";
        let _ = std::fs::remove_dir_all(test_dir);
        std::fs::create_dir_all(test_dir).unwrap();

        let until = SystemTime::now();
        let src = Manager::<4>::with_capacity(100, 100);
        for i in 0..16 {
            src.admit(CacheKey::new(format!("k{i}"), "1").to_compact(), 1, until);
        }
        src.save(test_dir).await.unwrap();

        // Truncate one shard to non-empty garbage so deserialize fails.
        std::fs::write(format!("{test_dir}/lru.data.2"), b"not valid msgpack").unwrap();

        let dst = Manager::<4>::with_capacity(100, 100);
        dst.load(test_dir).await.unwrap();
        // We should have loaded entries from the other 3 shards.
        assert!(dst.total_items() > 0);

        std::fs::remove_dir_all(test_dir).unwrap();
    }

    #[tokio::test]
    async fn test_save_then_load_roundtrip_with_remove() {
        // After load with missing shards, a subsequent save must persist every
        // shard file again so the next load is complete.
        let test_dir = "/tmp/test_lru_save_after_partial_load";
        let _ = std::fs::remove_dir_all(test_dir);
        std::fs::create_dir_all(test_dir).unwrap();

        let until = SystemTime::now();
        let src = Manager::<4>::with_capacity(100, 100);
        for i in 0..16 {
            src.admit(CacheKey::new(format!("k{i}"), "1").to_compact(), 1, until);
        }
        src.save(test_dir).await.unwrap();
        std::fs::remove_file(format!("{test_dir}/lru.data.1")).unwrap();

        let dst = Manager::<4>::with_capacity(100, 100);
        dst.load(test_dir).await.unwrap();
        dst.save(test_dir).await.unwrap();

        for i in 0..4 {
            assert!(
                Path::new(&format!("{test_dir}/lru.data.{i}")).exists(),
                "shard {i} should exist after save"
            );
        }

        std::fs::remove_dir_all(test_dir).unwrap();
    }

    #[tokio::test]
    async fn test_temp_file_cleanup() {
        let test_dir = "/tmp/test_lru_cleanup";
        let dir_path = Path::new(test_dir);

        // Create test directory
        std::fs::create_dir_all(dir_path).unwrap();

        // Create some fake temp files
        let temp_files = [
            "lru.data.0.12345678.tmp",
            "lru.data.1.abcdef00.tmp",
            "other_file.tmp", // Should not be removed
            "lru.data.2",     // Should not be removed
        ];

        for file in temp_files {
            let file_path = dir_path.join(file);
            std::fs::write(&file_path, b"test").unwrap();
        }

        let lru = Manager::<3>::with_capacity(10, 10);
        lru.load(test_dir).await.unwrap();

        // Check results
        assert!(!dir_path.join("lru.data.0.12345678.tmp").exists());
        assert!(!dir_path.join("lru.data.1.abcdef00.tmp").exists());
        assert!(dir_path.join("other_file.tmp").exists()); // Should remain
        assert!(dir_path.join("lru.data.2").exists()); // Should remain

        // Cleanup test directory
        std::fs::remove_dir_all(dir_path).unwrap();
    }

    #[test]
    fn test_peek_lru() {
        let lru = Manager::<1>::with_capacity(20, 20);
        let until = SystemTime::now();

        // empty shard returns None
        assert!(lru.peek_lru(0).is_none());

        let key1 = CacheKey::new("a", "1").to_compact();
        lru.admit(key1.clone(), 1, until);
        // single item: it's both the head and the tail
        assert_eq!(
            lru.peek_lru(0).unwrap(),
            CacheEntryKey::key_only(key1.clone())
        );

        // admit more keys to push key1 to the tail
        let key2 = CacheKey::new("b", "1").to_compact();
        lru.admit(key2.clone(), 1, until);
        for i in 0..5 {
            lru.admit(CacheKey::new(format!("f{i}"), "1").to_compact(), 1, until);
        }
        // key1 is the LRU tail (admitted first)
        assert_eq!(
            lru.peek_lru(0).unwrap(),
            CacheEntryKey::key_only(key1.clone())
        );

        // promote key1 — now key2 becomes the tail
        lru.access(&key1, 1, until);
        assert_eq!(
            lru.peek_lru(0).unwrap(),
            CacheEntryKey::key_only(key2.clone())
        );

        // peek_lru should not remove the item
        assert_eq!(
            lru.peek_lru(0).unwrap(),
            CacheEntryKey::key_only(key2.clone())
        );
        assert!(lru.peek(&key2));

        // out-of-bounds shard returns None
        assert!(lru.peek_lru(999).is_none());
    }
}
