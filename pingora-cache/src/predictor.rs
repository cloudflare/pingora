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

//! Cacheability Predictor

use crate::hashtable::{ConcurrentLruCache, LruShard};

pub type CustomReasonPredicate = fn(&'static str) -> bool;

/// Cacheability Predictor
///
/// Remembers previously uncacheable assets.
/// Allows bypassing cache / cache lock early based on historical precedent.
///
/// NOTE: to simply avoid caching requests with certain characteristics,
/// add checks in request_cache_filter to avoid enabling cache in the first place.
/// The predictor's bypass mechanism handles cases where the request _looks_ cacheable
/// but its previous responses suggest otherwise. The request _could_ be cacheable in the future.
pub struct Predictor<const N_SHARDS: usize> {
    uncacheable_keys: ConcurrentLruCache<(), N_SHARDS>,
    skip_custom_reasons_fn: Option<CustomReasonPredicate>,
}

use crate::{key::CacheHashKey, CacheKey, NoCacheReason};
use log::debug;

/// The cache predictor trait.
///
/// This trait allows user defined predictor to replace [Predictor].
pub trait CacheablePredictor {
    /// Return true if likely cacheable, false if likely not.
    fn cacheable_prediction(&self, key: &CacheKey) -> bool;

    /// Mark cacheable to allow next request to cache.
    /// Returns false if the key was already marked cacheable.
    fn mark_cacheable(&self, key: &CacheKey) -> bool;

    /// Mark uncacheable to actively bypass cache on the next request.
    /// May skip marking on certain NoCacheReasons.
    /// Returns None if we skipped marking uncacheable.
    /// Returns Some(false) if the key was already marked uncacheable.
    fn mark_uncacheable(&self, key: &CacheKey, reason: NoCacheReason) -> Option<bool>;
}

// This particular bit of `where [LruShard...; N]: Default` nonsense arises from
// ConcurrentLruCache needing this trait bound, which in turns arises from the Rust
// compiler not being able to guarantee that all array sizes N implement `Default`.
// See https://github.com/rust-lang/rust/issues/61415
impl<const N_SHARDS: usize> Predictor<N_SHARDS>
where
    [LruShard<()>; N_SHARDS]: Default,
{
    /// Create a new Predictor with `N_SHARDS * shard_capacity` total capacity for
    /// uncacheable cache keys.
    ///
    /// - `shard_capacity`: defines number of keys remembered as uncacheable per LRU shard.
    /// - `skip_custom_reasons_fn`: an optional predicate used in `mark_uncacheable`
    ///   that can customize which `Custom` `NoCacheReason`s ought to be remembered as uncacheable.
    ///   If the predicate returns true, then the predictor will skip remembering the current
    ///   cache key as uncacheable (and avoid bypassing cache on the next request).
    pub fn new(
        shard_capacity: usize,
        skip_custom_reasons_fn: Option<CustomReasonPredicate>,
    ) -> Predictor<N_SHARDS> {
        Predictor {
            uncacheable_keys: ConcurrentLruCache::<(), N_SHARDS>::new(shard_capacity),
            skip_custom_reasons_fn,
        }
    }
}

impl<const N_SHARDS: usize> CacheablePredictor for Predictor<N_SHARDS>
where
    [LruShard<()>; N_SHARDS]: Default,
{
    fn cacheable_prediction(&self, key: &CacheKey) -> bool {
        // variance key is ignored because this check happens before cache lookup
        let hash = key.primary_bin();
        let key = u128::from_be_bytes(hash); // Endianness doesn't matter

        // Note: LRU updated in mark_* functions only,
        // as we assume the caller always updates the cacheability of the response later
        !self.uncacheable_keys.read(key).contains(&key)
    }

    fn mark_cacheable(&self, key: &CacheKey) -> bool {
        // variance key is ignored because cacheable_prediction() is called before cache lookup
        // where the variance key is unknown
        let hash = key.primary_bin();
        let key = u128::from_be_bytes(hash);

        let cache = self.uncacheable_keys.get(key);
        if !cache.read().contains(&key) {
            // not in uncacheable list, nothing to do
            return true;
        }

        let mut cache = cache.write();
        cache.pop(&key);
        debug!("bypassed request became cacheable");
        false
    }

    fn mark_uncacheable(&self, key: &CacheKey, reason: NoCacheReason) -> Option<bool> {
        // only mark as uncacheable for the future on certain reasons,
        // (e.g. InternalErrors)
        use NoCacheReason::*;
        match reason {
            // CacheLockGiveUp: the writer will set OriginNotCache (if applicable)
            // readers don't need to do it
            NeverEnabled | StorageError | InternalError | Deferred | CacheLockGiveUp
            | CacheLockTimeout => {
                return None;
            }
            // Skip certain NoCacheReason::Custom according to user
            Custom(reason) if self.skip_custom_reasons_fn.map_or(false, |f| f(reason)) => {
                return None;
            }
            Custom(_) | OriginNotCache | ResponseTooLarge => { /* mark uncacheable for these only */
            }
        }

        // variance key is ignored because cacheable_prediction() is called before cache lookup
        // where the variance key is unknown
        let hash = key.primary_bin();
        let key = u128::from_be_bytes(hash);

        let mut cache = self.uncacheable_keys.get(key).write();
        // put() returns Some(old_value) if the key existed, else None
        let new_key = cache.put(key, ()).is_none();
        if new_key {
            debug!("request marked uncacheable");
        }
        Some(new_key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_mark_cacheability() {
        let predictor = Predictor::<1>::new(10, None);
        let key = CacheKey::new("a", "b", "c");
        // cacheable if no history
        assert!(predictor.cacheable_prediction(&key));

        // don't remember internal / storage errors
        predictor.mark_uncacheable(&key, NoCacheReason::InternalError);
        assert!(predictor.cacheable_prediction(&key));
        predictor.mark_uncacheable(&key, NoCacheReason::StorageError);
        assert!(predictor.cacheable_prediction(&key));

        // origin explicitly said uncacheable
        predictor.mark_uncacheable(&key, NoCacheReason::OriginNotCache);
        assert!(!predictor.cacheable_prediction(&key));

        // mark cacheable again
        predictor.mark_cacheable(&key);
        assert!(predictor.cacheable_prediction(&key));
    }

    #[test]
    fn test_custom_skip_predicate() {
        let predictor = Predictor::<1>::new(
            10,
            Some(|custom_reason| matches!(custom_reason, "Skipping")),
        );
        let key = CacheKey::new("a", "b", "c");
        // cacheable if no history
        assert!(predictor.cacheable_prediction(&key));

        // custom predicate still uses default skip reasons
        predictor.mark_uncacheable(&key, NoCacheReason::InternalError);
        assert!(predictor.cacheable_prediction(&key));

        // other custom reasons can still be marked uncacheable
        predictor.mark_uncacheable(&key, NoCacheReason::Custom("DontCacheMe"));
        assert!(!predictor.cacheable_prediction(&key));

        let key = CacheKey::new("a", "c", "d");
        assert!(predictor.cacheable_prediction(&key));
        // specific custom reason is skipped
        predictor.mark_uncacheable(&key, NoCacheReason::Custom("Skipping"));
        assert!(predictor.cacheable_prediction(&key));
    }

    #[test]
    fn test_mark_uncacheable_lru() {
        let predictor = Predictor::<1>::new(3, None);
        let key1 = CacheKey::new("a", "b", "c");
        predictor.mark_uncacheable(&key1, NoCacheReason::OriginNotCache);
        assert!(!predictor.cacheable_prediction(&key1));

        let key2 = CacheKey::new("a", "bc", "c");
        predictor.mark_uncacheable(&key2, NoCacheReason::OriginNotCache);
        assert!(!predictor.cacheable_prediction(&key2));

        let key3 = CacheKey::new("a", "cd", "c");
        predictor.mark_uncacheable(&key3, NoCacheReason::OriginNotCache);
        assert!(!predictor.cacheable_prediction(&key3));

        // promote / reinsert key1
        predictor.mark_uncacheable(&key1, NoCacheReason::OriginNotCache);

        let key4 = CacheKey::new("a", "de", "c");
        predictor.mark_uncacheable(&key4, NoCacheReason::OriginNotCache);
        assert!(!predictor.cacheable_prediction(&key4));

        // key 1 was recently used
        assert!(!predictor.cacheable_prediction(&key1));
        // key 2 was evicted
        assert!(predictor.cacheable_prediction(&key2));
        assert!(!predictor.cacheable_prediction(&key3));
        assert!(!predictor.cacheable_prediction(&key4));
    }
}
