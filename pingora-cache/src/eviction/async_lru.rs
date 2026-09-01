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

//! Async LRU eviction manager.
//!
//! [`Manager`] wraps [`AsyncLru`] and implements the [`EvictionManager`]
//! trait. Eviction happens asynchronously inside the actor tasks, so
//! [`EvictionManager::admit`] and [`EvictionManager::increment_weight`]
//! **always return empty `Vec`s** — callers should not expect synchronous
//! eviction results from these methods. Evicted items are instead handled
//! by the [`AsyncEvictionCallback`] provided at construction time.

use super::{CacheEntryKey, CacheEntryKeyRef, EvictionManager};
#[cfg(test)]
use crate::key::CompactCacheKey;

use async_trait::async_trait;
use pingora_core::server::ShutdownWatch;
use pingora_error::{ErrorType::*, OrErr, Result};
use pingora_lru::{
    async_lru::{hash_key, AsyncEvictionCallback, AsyncLru},
    persistence,
};
use serde::de::SeqAccess;

use std::sync::Arc;
use std::time::SystemTime;

/// Async LRU eviction manager.
///
/// (Nearly) drop-in replacement for [`super::lru::Manager`] that uses actor-based
/// shards instead of `RwLock`-based shards. Eviction is handled
/// asynchronously by dedicated eviction worker tasks.
///
/// Construct via the builder pattern: call [`Manager::builder`] with the
/// required arguments (weight limit, [`AsyncEvictionCallback`], and
/// [`ShutdownWatch`]), then chain optional setters before calling
/// [`ManagerBuilder::build`].
pub struct Manager<const N: usize> {
    lru: Arc<AsyncLru<CacheEntryKey, N>>,
}

impl<const N: usize> Manager<N> {
    /// Construct from a pre-built [`AsyncLru`].
    pub fn from_lru(lru: Arc<AsyncLru<CacheEntryKey, N>>) -> Self {
        Manager { lru }
    }
}

/// Builder for [`Manager`].
///
/// # Required
/// - `weight_limit` — maximum total weight before eviction
/// - `eviction_cb` — callback invoked for each evicted `(key, weight)` pair
/// - `shutdown` — watch receiver that signals actors to stop
///
/// # Example
/// ```ignore
/// let manager = Manager::<32>::builder(limit, eviction_cb, shutdown_rx, runtime_handle)
///     .capacity(10_000)
///     .build();
/// ```
pub struct ManagerBuilder<C, const N: usize> {
    weight_limit: usize,
    eviction_cb: Arc<C>,
    shutdown: ShutdownWatch,
    runtime: tokio::runtime::Handle,
    capacity: usize,
    len_watermark: Option<usize>,
    num_eviction_workers: usize,
}

impl<C, const N: usize> ManagerBuilder<C, N>
where
    C: AsyncEvictionCallback<CacheEntryKey>,
{
    /// Set the estimated per-shard capacity for preallocation.
    pub fn capacity(mut self, capacity: usize) -> Self {
        self.capacity = capacity;
        self
    }

    /// Set an optional item-count watermark.
    pub fn len_watermark(mut self, watermark: usize) -> Self {
        self.len_watermark = Some(watermark);
        self
    }

    /// Set the number of concurrent eviction worker tasks.
    /// Defaults to `N` (one per shard).
    pub fn num_eviction_workers(mut self, n: usize) -> Self {
        self.num_eviction_workers = n;
        self
    }

    /// Build the [`Manager`], spawning shard actors and eviction workers.
    ///
    /// Must be called within a tokio runtime context.
    pub fn build(self) -> Manager<N> {
        let mut lru_builder = AsyncLru::builder(
            self.weight_limit,
            self.eviction_cb,
            self.shutdown,
            self.runtime,
        )
        .capacity(self.capacity)
        .num_eviction_workers(self.num_eviction_workers);
        if let Some(w) = self.len_watermark {
            lru_builder = lru_builder.len_watermark(w);
        }
        Manager {
            lru: Arc::new(lru_builder.build()),
        }
    }
}

impl<const N: usize> Manager<N> {
    /// Create a builder for constructing a [`Manager`].
    ///
    /// This is the **only** way to construct a `Manager`. All four
    /// arguments are required:
    ///
    /// - `weight_limit` — maximum total weight before eviction.
    /// - `eviction_cb` — callback invoked for every evicted `(key, weight)`.
    /// - `shutdown` — [`ShutdownWatch`] that signals actors to stop.
    /// - `runtime` — [`tokio::runtime::Handle`] on which internal tasks
    ///   are spawned.
    pub fn builder<C>(
        weight_limit: usize,
        eviction_cb: Arc<C>,
        shutdown: ShutdownWatch,
        runtime: tokio::runtime::Handle,
    ) -> ManagerBuilder<C, N>
    where
        C: AsyncEvictionCallback<CacheEntryKey>,
    {
        ManagerBuilder {
            weight_limit,
            eviction_cb,
            shutdown,
            runtime,
            capacity: 0,
            len_watermark: None,
            num_eviction_workers: N.max(1),
        }
    }

    /// Return the total number of shards (`N`).
    pub fn shards(&self) -> usize {
        self.lru.shards()
    }

    /// Query the exact weight of a specific shard via request/response to its
    /// actor. Returns `None` if `shard >= N`.
    pub async fn shard_weight(&self, shard: usize) -> Option<usize> {
        self.lru.shard_weight(shard).await
    }

    /// Get the number of items in a specific shard. Lock-free, best-effort
    /// (`Relaxed` atomic load).
    pub fn shard_len(&self, shard: usize) -> usize {
        self.lru.shard_len(shard)
    }

    /// Compute the shard index for a given cache entry using the same hash
    /// function used internally by the LRU.
    pub fn get_shard_for_key(&self, key: &CacheEntryKey) -> usize {
        (hash_key(key) % N as u64) as usize
    }

    /// Peek at the least-recently-used key in the given shard. Async —
    /// sends a request/response message to the shard actor. Returns `None`
    /// if `shard >= N` or the shard is empty.
    pub async fn peek_lru(&self, shard: usize) -> Option<CacheEntryKey> {
        self.lru.peek_lru(shard).await.map(|(key, _weight)| key)
    }

    /// Peek the weight of a cache entry without promoting it. Lock-free.
    /// Returns `None` if the entry is absent.
    pub fn peek_weight(&self, item: &CacheEntryKey) -> Option<usize> {
        self.lru.peek_weight(item)
    }

    /// Serialize the contents of a single shard to MessagePack bytes.
    ///
    /// Snapshots the shard via the actor (cloning all keys), then serializes
    /// the `(key, weight)` pairs as a msgpack sequence.
    pub async fn serialize_shard(&self, shard: usize) -> Result<Vec<u8>> {
        use rmp_serde::encode::Serializer;
        use serde::ser::SerializeSeq;
        use serde::ser::Serializer as _;

        let items = self.lru.snapshot_shard(shard).await.unwrap_or_default();
        let mut ser = Serializer::new(vec![]);
        let mut seq = ser
            .serialize_seq(Some(items.len()))
            .or_err(InternalError, "fail to serialize node")?;
        for item in &items {
            seq.serialize_element(item)
                .or_err(InternalError, "when serializing LRU element")?;
        }
        seq.end().or_err(InternalError, "when serializing LRU")?;
        Ok(ser.into_inner())
    }

    /// Deserialize a shard buffer into a list of `(key, weight)` pairs.
    fn deserialize_shard(buf: &[u8]) -> Result<Vec<(CacheEntryKey, usize)>> {
        use rmp_serde::decode::Deserializer;
        use serde::de::Deserializer as _;

        let mut de = Deserializer::new(buf);
        let visitor = CollectItems;
        de.deserialize_seq(visitor)
            .or_err(InternalError, "when deserializing async LRU")
    }

    /// Insert deserialized items into the LRU at the tail.
    fn load_shard(&self, items: Vec<(CacheEntryKey, usize)>) {
        for (key, weight) in items {
            self.lru.insert_tail(key, weight);
        }
    }
}

struct CollectItems;

impl<'de> serde::de::Visitor<'de> for CollectItems {
    type Value = Vec<(CacheEntryKey, usize)>;

    fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
        formatter.write_str("array of (key, weight) tuples")
    }

    fn visit_seq<A>(self, mut seq: A) -> std::result::Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut items = Vec::with_capacity(seq.size_hint().unwrap_or(0));
        while let Some(item) = seq.next_element::<(CacheEntryKey, usize)>()? {
            items.push(item);
        }
        Ok(items)
    }
}

const FILE_NAME: &str = "lru.data";

#[async_trait]
impl<const N: usize> EvictionManager for Manager<N> {
    /// Total weight across all shards. Lock-free.
    fn total_size(&self) -> usize {
        self.lru.weight()
    }
    /// Total item count across all shards. Lock-free.
    fn total_items(&self) -> usize {
        self.lru.len()
    }
    /// Accumulated weight of all evicted items since construction. Lock-free.
    fn evicted_size(&self) -> usize {
        self.lru.evicted_weight()
    }
    /// Accumulated count of all evicted items since construction. Lock-free.
    fn evicted_items(&self) -> usize {
        self.lru.evicted_len()
    }

    /// Admit a cache key with the given size. Fire-and-forget.
    ///
    /// **Always returns `vec![]`** — eviction is handled asynchronously by
    /// the eviction workers and delivered via the [`AsyncEvictionCallback`].
    fn admit(
        &self,
        item: CacheEntryKey,
        size: usize,
        _fresh_until: SystemTime,
    ) -> Vec<CacheEntryKey> {
        self.lru.admit(item, size);
        vec![]
    }

    /// Increment a cache key's weight, admitting it if needed. Fire-and-forget.
    ///
    /// **Always returns `vec![]`** — eviction is handled asynchronously by
    /// the eviction workers and delivered via the [`AsyncEvictionCallback`].
    fn increment_weight(
        &self,
        item: &CacheEntryKey,
        delta: usize,
        max_weight: Option<usize>,
    ) -> Vec<CacheEntryKey> {
        self.lru.increment_weight(item, delta, max_weight);
        vec![]
    }

    /// Remove a cache key from the LRU. Fire-and-forget — enqueued on the
    /// shard's unbounded channel, so the message is never dropped (it fails
    /// only if the actor is gone, i.e. during shutdown).
    fn remove(&self, item: CacheEntryKeyRef<'_>) {
        self.lru.remove_by_hash(hash_key(&item));
    }

    /// Record an access to a cache key. If the key already exists it is
    /// promoted to the head of the LRU; otherwise it is admitted with the
    /// given `size`. Returns `true` if the key was already present (promoted),
    /// `false` if it was newly admitted.
    fn access(&self, item: &CacheEntryKey, size: usize, _fresh_until: SystemTime) -> bool {
        if self.lru.promote(item) {
            true
        } else {
            self.lru.admit(item.clone(), size);
            false
        }
    }

    /// Check whether a cache key exists in the LRU without promoting it.
    /// Lock-free.
    fn peek(&self, item: &CacheEntryKey) -> bool {
        self.lru.peek(item)
    }

    /// Persist all shards sequentially to the given directory.
    ///
    /// Each shard is snapshotted via the actor, serialized to MessagePack,
    /// and written to `{dir_path}/lru.data.{shard_index}` using atomic rename.
    async fn save(&self, dir_path: &str) -> Result<()> {
        persistence::save_shards_async(dir_path, FILE_NAME, N, |i| self.serialize_shard(i))
            .await
            .or_err(InternalError, "failed to save async LRU")?;
        Ok(())
    }

    /// Load the current manager's shard files from the given directory.
    ///
    /// Each file is deserialized from MessagePack, and the resulting items are
    /// inserted at the tail of the LRU.
    async fn load(&self, dir_path: &str) -> Result<()> {
        persistence::load_shards(dir_path, FILE_NAME, N, |_i, data| {
            Self::deserialize_shard(data).map(|items| self.load_shard(items))
        })
        .await;
        Ok(())
    }
}

#[cfg(test)]
impl<const N: usize> Manager<N> {
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::key::CacheKey;

    #[tokio::test(flavor = "multi_thread")]
    async fn increment_weight_admits_missing_key() {
        let (_shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let manager = Manager::<1>::builder(
            100,
            Arc::new(|_key: CacheEntryKey, _weight| async {}),
            shutdown_rx,
            tokio::runtime::Handle::current(),
        )
        .build();
        let key = CacheKey::new("missing", "1").to_compact();

        manager.increment_weight(&key, 7, Some(10));
        manager.shard_weight(0).await;

        assert_eq!(manager.peek_weight(&CacheEntryKey::key_only(key)), Some(7));
        assert_eq!(manager.total_items(), 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn remove_requires_complete_identity() {
        let (_shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let manager = Manager::<1>::builder(
            100,
            Arc::new(|_key: CacheEntryKey, _weight| async {}),
            shutdown_rx,
            tokio::runtime::Handle::current(),
        )
        .build();
        let key = CacheKey::new("identified", "1").to_compact();
        let id = crate::eviction::CacheEntryId::new(42);

        let _ = EvictionManager::admit(
            &manager,
            CacheEntryKey::identified(key.clone(), id),
            1,
            SystemTime::now(),
        );
        // A shard query waits for the actor to process preceding messages on its FIFO channel.
        manager.shard_weight(0).await;

        EvictionManager::remove(&manager, CacheEntryKeyRef::from_entry_id(&key, None));
        manager.shard_weight(0).await;
        assert_eq!(manager.total_size(), 1);

        EvictionManager::remove(&manager, CacheEntryKeyRef::from_entry_id(&key, Some(id)));
        manager.shard_weight(0).await;
        assert_eq!(manager.total_size(), 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn save_returns_error_when_all_shards_fail() {
        let dir = std::env::temp_dir().join(format!(
            "pingora-async-lru-total-save-failure-{}-{}",
            std::process::id(),
            rand::random::<u32>()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        for shard in 0..2 {
            // Renaming a temporary file over a directory fails.
            std::fs::create_dir(dir.join(format!("{FILE_NAME}.{shard}"))).unwrap();
        }

        let (_shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let manager = Manager::<2>::builder(
            100,
            Arc::new(|_key: CacheEntryKey, _weight| async {}),
            shutdown_rx,
            tokio::runtime::Handle::current(),
        )
        .build();

        let error = manager.save(dir.to_str().unwrap()).await.unwrap_err();
        assert!(
            error.to_string().contains("failed to save async LRU"),
            "unexpected error: {error}"
        );

        std::fs::remove_dir_all(dir).unwrap();
    }
}
