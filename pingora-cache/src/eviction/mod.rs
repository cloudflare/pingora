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

//! Cache eviction module

use crate::key::CompactCacheKey;

use async_trait::async_trait;
use pingora_error::Result;
use serde::ser::SerializeTuple;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt::{Display, Formatter, Result as FmtResult};
use std::hash::{Hash, Hasher};
use std::time::SystemTime;

pub mod async_lru;
pub mod lru;
pub mod simple_lru;

/// Storage-defined identity for a cache entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct CacheEntryId(u64);

impl CacheEntryId {
    /// Construct an entry ID from its storage-defined value.
    pub fn new(value: u64) -> Self {
        Self(value)
    }

    /// Return the storage-defined value.
    pub fn get(self) -> u64 {
        self.0
    }
}

/// The identity of an entry tracked by an eviction manager.
///
/// Identified entries combine a logical cache key with storage-defined entry identity. Eviction
/// managers paired with storage that returns identified entries must preserve and compare the
/// complete key. Managers that use only the compact key cannot correctly account for such storage.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum CacheEntryKey {
    /// An entry identified only by its logical cache key.
    KeyOnly(CompactCacheKey),
    /// An entry with additional storage-defined identity.
    Identified {
        /// The logical cache key.
        key: CompactCacheKey,
        /// The storage-defined entry ID.
        id: CacheEntryId,
    },
}

impl Hash for CacheEntryKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        CacheEntryKeyRef::from(self).hash(state);
    }
}

impl Serialize for CacheEntryKey {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::KeyOnly(key) => key.serialize(serializer),
            Self::Identified { key, id } => {
                let mut tuple = serializer.serialize_tuple(2)?;
                tuple.serialize_element(key)?;
                tuple.serialize_element(id)?;
                tuple.end()
            }
        }
    }
}

impl<'de> Deserialize<'de> for CacheEntryKey {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        CacheEntryKeyRepr::deserialize(deserializer).map(|entry| match entry {
            CacheEntryKeyRepr::Identified(key, id) => Self::from_entry_id(key, id),
            CacheEntryKeyRepr::KeyOnly(key) => Self::KeyOnly(key),
        })
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum CacheEntryKeyRepr {
    Identified(CompactCacheKey, Option<CacheEntryId>),
    KeyOnly(CompactCacheKey),
}

impl Display for CacheEntryKey {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        write!(f, "{}", self.key())?;
        if let Some(id) = self.entry_id() {
            write!(f, ", entry ID: {:x}", id.get())?;
        }
        Ok(())
    }
}

impl Default for CacheEntryKey {
    fn default() -> Self {
        Self::KeyOnly(CompactCacheKey::default())
    }
}

impl CacheEntryKey {
    /// Construct an entry from its logical key and optional storage-defined identity.
    pub fn from_entry_id(key: CompactCacheKey, id: Option<CacheEntryId>) -> Self {
        match id {
            Some(id) => Self::Identified { key, id },
            None => Self::KeyOnly(key),
        }
    }

    /// Construct an entry identified only by its logical cache key.
    pub fn key_only(key: CompactCacheKey) -> Self {
        Self::KeyOnly(key)
    }

    /// Construct an entry with storage-defined identity.
    pub fn identified(key: CompactCacheKey, id: CacheEntryId) -> Self {
        Self::Identified { key, id }
    }

    /// Return the underlying cache key.
    pub fn key(&self) -> &CompactCacheKey {
        match self {
            Self::KeyOnly(key) | Self::Identified { key, .. } => key,
        }
    }

    /// Consume the eviction key and return its underlying cache key.
    pub fn into_key(self) -> CompactCacheKey {
        match self {
            Self::KeyOnly(key) | Self::Identified { key, .. } => key,
        }
    }

    /// Return the storage-defined entry ID, if present.
    pub fn entry_id(&self) -> Option<CacheEntryId> {
        match self {
            Self::KeyOnly(_) => None,
            Self::Identified { id, .. } => Some(*id),
        }
    }
}

/// A borrowed cache entry identity used for exact eviction-manager lookups.
///
/// This type hashes identically to the equivalent owned [`CacheEntryKey`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CacheEntryKeyRef<'a> {
    /// An entry identified only by its logical cache key.
    KeyOnly(&'a CompactCacheKey),
    /// An entry with additional storage-defined identity.
    Identified {
        /// The logical cache key.
        key: &'a CompactCacheKey,
        /// The storage-defined entry ID.
        id: CacheEntryId,
    },
}

impl<'a> CacheEntryKeyRef<'a> {
    /// Construct a borrowed entry identity from its logical key and storage-defined ID, if any.
    ///
    /// The ID must match the entry originally admitted to an eviction manager. A different ID is a
    /// different entry and will not remove or otherwise match the admitted entry.
    pub fn from_entry_id(key: &'a CompactCacheKey, id: Option<CacheEntryId>) -> Self {
        match id {
            Some(id) => Self::Identified { key, id },
            None => Self::KeyOnly(key),
        }
    }

    /// Return the underlying cache key.
    pub fn key(self) -> &'a CompactCacheKey {
        match self {
            Self::KeyOnly(key) | Self::Identified { key, .. } => key,
        }
    }

    /// Return the storage-defined entry ID, if present.
    pub fn entry_id(self) -> Option<CacheEntryId> {
        match self {
            Self::KeyOnly(_) => None,
            Self::Identified { id, .. } => Some(id),
        }
    }
}

impl Display for CacheEntryKeyRef<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        write!(f, "{}", self.key())?;
        if let Some(id) = self.entry_id() {
            write!(f, ", entry ID: {:x}", id.get())?;
        }
        Ok(())
    }
}

impl<'a> From<&'a CacheEntryKey> for CacheEntryKeyRef<'a> {
    fn from(entry: &'a CacheEntryKey) -> Self {
        Self::from_entry_id(entry.key(), entry.entry_id())
    }
}

/// The trait that a cache eviction algorithm needs to implement.
///
/// NOTE: these trait methods require &self not &mut self, which means concurrency should
/// be handled the implementations internally.
#[async_trait]
pub trait EvictionManager: Send + Sync {
    /// Total size of the cache in bytes tracked by this eviction manager
    fn total_size(&self) -> usize;
    /// Number of assets tracked by this eviction manager
    fn total_items(&self) -> usize;
    /// Number of bytes that are already evicted
    ///
    /// The accumulated number is returned to play well with Prometheus counter metric type.
    fn evicted_size(&self) -> usize;
    /// Number of assets that are already evicted
    ///
    /// The accumulated number is returned to play well with Prometheus counter metric type.
    fn evicted_items(&self) -> usize;

    /// Admit an item
    ///
    /// Return one or more items to evict. The sizes of these items are deducted
    /// from the total size already. The caller needs to make sure that these assets are actually
    /// removed from the storage.
    ///
    /// If the item is already admitted, A. update its freshness; B. if the new size is larger than the
    /// existing one, Some(_) might be returned for the caller to evict.
    fn admit(
        &self,
        item: CacheEntryKey,
        size: usize,
        fresh_until: SystemTime,
    ) -> Vec<CacheEntryKey>;

    /// Adjust an item's weight upwards by a delta. If the item is not already admitted,
    /// track it with the delta as its initial weight, capped by `max_weight`, and floored to 1.
    ///
    /// An optional `max_weight` hint indicates the known max weight of the current key in case the
    /// weight should not be incremented above this amount. This hint will not shrink an item whose
    /// current weight already exceeds `max_weight`.
    ///
    /// Return one or more items to evict. The sizes of these items are deducted
    /// from the total size already. The caller needs to make sure that these assets are actually
    /// removed from the storage.
    fn increment_weight(
        &self,
        item: &CacheEntryKey,
        delta: usize,
        max_weight: Option<usize>,
    ) -> Vec<CacheEntryKey>;

    /// Remove an item from the eviction manager.
    ///
    /// The size of the item will be deducted. Implementations must require the complete identity to
    /// match the originally admitted entry; a key-only identity must not match an identified entry
    /// with the same logical key.
    fn remove(&self, item: CacheEntryKeyRef<'_>);

    /// Access an item that should already be in cache.
    ///
    /// If the item is not tracked by this [EvictionManager], track it but no eviction will happen.
    ///
    /// The call used for asking the eviction manager to track the assets that are already admitted
    /// in the cache storage system.
    fn access(&self, item: &CacheEntryKey, size: usize, fresh_until: SystemTime) -> bool;

    /// Peek into the manager to see if the item is already tracked by the system
    ///
    /// This function should have no side-effect on the asset itself. For example, for LRU, this
    /// method shouldn't change the popularity of the asset being peeked.
    fn peek(&self, item: &CacheEntryKey) -> bool;

    /// Serialize to save the state of this eviction manager to disk
    ///
    /// This function is for preserving the eviction manager's state across server restarts.
    ///
    /// `dir_path` define the directory on disk that the data should use.
    // dir_path is &str no AsRef<Path> so that trait objects can be used
    async fn save(&self, dir_path: &str) -> Result<()>;

    /// The counterpart of [Self::save()].
    async fn load(&self, dir_path: &str) -> Result<()>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CacheKey;

    #[test]
    fn cache_entry_key_serde_preserves_identity() {
        let key = CacheKey::new("entry", "1").to_compact();
        let legacy = rmp_serde::to_vec(&key).unwrap();
        let key_only = CacheEntryKey::key_only(key.clone());
        assert_eq!(rmp_serde::to_vec(&key_only).unwrap(), legacy);
        assert_eq!(
            rmp_serde::from_slice::<CacheEntryKey>(&legacy).unwrap(),
            key_only
        );
        let previous_key_only =
            rmp_serde::to_vec(&(key.clone(), Option::<CacheEntryId>::None)).unwrap();
        assert_eq!(
            rmp_serde::from_slice::<CacheEntryKey>(&previous_key_only).unwrap(),
            key_only
        );

        let entry = CacheEntryKey::identified(key, CacheEntryId::new(7));

        let serialized = rmp_serde::to_vec(&entry).unwrap();
        let deserialized = rmp_serde::from_slice(&serialized).unwrap();

        assert_eq!(entry, deserialized);
    }

    #[test]
    fn owned_and_borrowed_entry_keys_hash_identically() {
        use ahash::AHasher;
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        // This compares structural identity through each hasher, not hash stability across versions.
        let key = CacheKey::new("entry", "1").to_compact();
        for entry in [
            CacheEntryKey::key_only(key.clone()),
            CacheEntryKey::identified(key, CacheEntryId::new(7)),
        ] {
            let mut owned = DefaultHasher::new();
            entry.hash(&mut owned);
            let mut borrowed = DefaultHasher::new();
            CacheEntryKeyRef::from(&entry).hash(&mut borrowed);
            assert_eq!(owned.finish(), borrowed.finish());

            let mut owned = AHasher::default();
            entry.hash(&mut owned);
            let mut borrowed = AHasher::default();
            CacheEntryKeyRef::from(&entry).hash(&mut borrowed);
            assert_eq!(owned.finish(), borrowed.finish());
        }
    }
}
