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

//! An async read through cache where cache misses are populated via the provided
//! async callback.

use super::{CacheStatus, MemoryCache};

use async_trait::async_trait;
use log::warn;
use parking_lot::RwLock;
use pingora_error::{Error, ErrorTrait};
use std::collections::HashMap;
use std::hash::Hash;
use std::marker::PhantomData;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Semaphore;

struct CacheLock {
    pub lock_start: Instant,
    pub lock: Semaphore,
}

impl CacheLock {
    pub fn new_arc() -> Arc<Self> {
        Arc::new(CacheLock {
            lock: Semaphore::new(0),
            lock_start: Instant::now(),
        })
    }

    pub fn too_old(&self, age: Option<&Duration>) -> bool {
        match age {
            Some(t) => Instant::now() - self.lock_start > *t,
            None => false,
        }
    }
}

#[async_trait]
/// [Lookup] defines the caching behavior that the implementor needs. The `extra` field can be used
/// to define any additional metadata that the implementor uses to determine cache eligibility.
///
/// # Examples
///
/// ```ignore
/// use pingora_error::{ErrorTrait, Result};
/// use std::time::Duration;
///
/// struct MyLookup;
///
/// impl Lookup<usize, usize, ()> for MyLookup {
///     async fn lookup(
///         &self,
///         _key: &usize,
///         extra: Option<&()>,
///     ) -> Result<(usize, Option<Duration>), Box<dyn ErrorTrait + Send + Sync>> {
///         // Define your business logic here.
///         Ok(1, None)
///     }
/// }
/// ```
pub trait Lookup<K, T, S> {
    /// Return a value and an optional TTL for the given key.
    async fn lookup(
        key: &K,
        extra: Option<&S>,
    ) -> Result<(T, Option<Duration>), Box<dyn ErrorTrait + Send + Sync>>
    where
        K: 'async_trait,
        S: 'async_trait;
}

#[async_trait]
/// [MultiLookup] is similar to [Lookup]. Implement this trait if the system being queried support
/// looking up multiple keys in a single API call.
pub trait MultiLookup<K, T, S> {
    /// Like [Lookup::lookup] but for an arbitrary amount of keys.
    async fn multi_lookup(
        keys: &[&K],
        extra: Option<&S>,
    ) -> Result<Vec<(T, Option<Duration>)>, Box<dyn ErrorTrait + Send + Sync>>
    where
        K: 'async_trait,
        S: 'async_trait;
}

const LOOKUP_ERR_MSG: &str = "RTCache: lookup error";

/// A read-through in-memory cache on top of [MemoryCache]
///
/// Instead of providing a `put` function, [RTCache] requires a type which implements [Lookup] to
/// be automatically called during cache miss to populate the cache. This is useful when trying to
/// cache queries to external system such as DNS or databases.
///
/// Lookup coalescing is provided so that multiple concurrent lookups for the same key results
/// only in one lookup callback.
pub struct RTCache<K, T, CB, S>
where
    K: Hash + Send,
    T: Clone + Send,
{
    inner: MemoryCache<K, T>,
    _callback: PhantomData<CB>,
    lockers: RwLock<HashMap<u64, Arc<CacheLock>>>,
    lock_age: Option<Duration>,
    lock_timeout: Option<Duration>,
    phantom: PhantomData<S>,
}

impl<K, T, CB, S> RTCache<K, T, CB, S>
where
    K: Hash + Send,
    T: Clone + Send + Sync + 'static,
{
    /// Create a new [RTCache] of given size. `lock_age` defines how long a lock is valid for.
    /// `lock_timeout` is used to stop a lookup from holding on to the key for too long.
    pub fn new(size: usize, lock_age: Option<Duration>, lock_timeout: Option<Duration>) -> Self {
        RTCache {
            inner: MemoryCache::new(size),
            lockers: RwLock::new(HashMap::new()),
            _callback: PhantomData,
            lock_age,
            lock_timeout,
            phantom: PhantomData,
        }
    }
}

impl<K, T, CB, S> RTCache<K, T, CB, S>
where
    K: Hash + Send,
    T: Clone + Send + Sync + 'static,
    CB: Lookup<K, T, S>,
{
    /// Query the cache for a given value. If it exists and no TTL is configured initially, it will
    /// use the `ttl` value given.
    pub async fn get(
        &self,
        key: &K,
        ttl: Option<Duration>,
        extra: Option<&S>,
    ) -> (Result<T, Box<Error>>, CacheStatus) {
        let (result, cache_state) = self.inner.get(key);
        if let Some(result) = result {
            /* cache hit */
            return (Ok(result), cache_state);
        }

        let hashed_key = self.inner.hasher.hash_one(key);

        /* Cache miss, try to lock the lookup. Check if there is already a lookup */
        let my_lock = {
            let lockers = self.lockers.read();
            /* clone the Arc */
            lockers.get(&hashed_key).cloned()
        }; // read lock dropped

        /* try insert a cache lock into locker */
        let (my_write, my_read) = match my_lock {
            // TODO: use a union
            Some(lock) => {
                /* There is an ongoing lookup to the same key */
                if lock.too_old(self.lock_age.as_ref()) {
                    (None, None)
                } else {
                    (None, Some(lock))
                }
            }
            None => {
                let mut lockers = self.lockers.write();
                match lockers.get(&hashed_key) {
                    Some(lock) => {
                        /* another lookup to the same key got the write lock to locker first */
                        if lock.too_old(self.lock_age.as_ref()) {
                            (None, None)
                        } else {
                            (None, Some(lock.clone()))
                        }
                    }
                    None => {
                        let new_lock = CacheLock::new_arc();
                        let new_lock2 = new_lock.clone();
                        lockers.insert(hashed_key, new_lock2);
                        (Some(new_lock), None)
                    }
                } // write lock dropped
            }
        };

        if my_read.is_some() {
            /* another task will do the lookup */

            let my_lock = my_read.unwrap();
            /* if available_permits > 0, writer is done */
            if my_lock.lock.available_permits() == 0 {
                /* block here to wait for writer to finish lookup */
                let lock_fut = my_lock.lock.acquire();
                let timed_out = match self.lock_timeout {
                    Some(t) => pingora_timeout::timeout(t, lock_fut).await.is_err(),
                    None => {
                        let _ = lock_fut.await;
                        false
                    }
                };
                if timed_out {
                    let value = CB::lookup(key, extra).await;
                    return match value {
                        Ok((v, _ttl)) => (Ok(v), cache_state),
                        Err(e) => {
                            let mut err = Error::new_str(LOOKUP_ERR_MSG);
                            err.set_cause(e);
                            (Err(err), cache_state)
                        }
                    };
                }
            } // permit returned here

            let (result, cache_state) = self.inner.get(key);
            if let Some(result) = result {
                /* cache lock hit, slow as a miss */
                (Ok(result), CacheStatus::LockHit)
            } else {
                /* probably error happen during the actual lookup */
                warn!(
                    "RTCache: no result after read lock, cache status: {:?}",
                    cache_state
                );
                match CB::lookup(key, extra).await {
                    Ok((v, new_ttl)) => {
                        self.inner.force_put(key, v.clone(), new_ttl.or(ttl));
                        (Ok(v), cache_state)
                    }
                    Err(e) => {
                        let mut err = Error::new_str(LOOKUP_ERR_MSG);
                        err.set_cause(e);
                        (Err(err), cache_state)
                    }
                }
            }
        } else {
            /* this one will do the look up, either because it gets the write lock or the read
             * lock age is reached */
            let value = CB::lookup(key, extra).await;
            let ret = match value {
                Ok((v, new_ttl)) => {
                    /* Don't put() if lock ago too old, to avoid too many concurrent writes */
                    if my_write.is_some() {
                        self.inner.force_put(key, v.clone(), new_ttl.or(ttl));
                    }
                    (Ok(v), cache_state) // the original cache_state: Miss or Expired
                }
                Err(e) => {
                    let mut err = Error::new_str(LOOKUP_ERR_MSG);
                    err.set_cause(e);
                    (Err(err), cache_state)
                }
            };
            if my_write.is_some() {
                /* add permit so that reader can start. Any number of permits will do,
                 * since readers will return permits right away. */
                my_write.unwrap().lock.add_permits(10);

                {
                    // remove the lock from locker
                    let mut lockers = self.lockers.write();
                    lockers.remove(&hashed_key);
                } // write lock dropped here
            }

            ret
        }
    }
}

impl<K, T, CB, S> RTCache<K, T, CB, S>
where
    K: Hash + Send,
    T: Clone + Send + Sync + 'static,
    CB: MultiLookup<K, T, S>,
{
    /// Same behavior as [RTCache::get] but for an arbitrary amount of keys.
    ///
    /// If there are keys that are missing from the cache, `multi_lookup` is invoked to populate the
    /// cache before returning the final results. This is useful if your type supports batch
    /// queries.
    ///
    /// To avoid dead lock for the same key across concurrent `multi_get` calls,
    /// this function does not provide lookup coalescing.
    pub async fn multi_get<'a, I>(
        &self,
        keys: I,
        ttl: Option<Duration>,
        extra: Option<&S>,
    ) -> Result<Vec<(T, CacheStatus)>, Box<Error>>
    where
        I: Iterator<Item = &'a K>,
        K: 'a,
    {
        let size = keys.size_hint().0;
        let (hits, misses) = self.inner.multi_get_with_miss(keys);
        let mut final_results = Vec::with_capacity(size);
        let miss_results = if !misses.is_empty() {
            match CB::multi_lookup(&misses, extra).await {
                Ok(miss_results) => {
                    // assert! here to prevent index panic when building results,
                    // final_results has the full list of misses but miss_results might not
                    assert!(
                        miss_results.len() == misses.len(),
                        "multi_lookup() failed to return the matching number of results"
                    );
                    /* put the misses into cache */
                    for item in misses.iter().zip(miss_results.iter()) {
                        self.inner
                            .force_put(item.0, (item.1).0.clone(), (item.1).1.or(ttl));
                    }
                    miss_results
                }
                Err(e) => {
                    /* NOTE: we give up the hits when encounter lookup error */
                    let mut err = Error::new_str(LOOKUP_ERR_MSG);
                    err.set_cause(e);
                    return Err(err);
                }
            }
        } else {
            vec![] // to make the rest code simple, allocating one unused empty vec should be fine
        };
        /* fill in final_result */
        let mut n_miss = 0;
        for item in hits {
            match item.0 {
                Some(v) => final_results.push((v, item.1)),
                None => {
                    final_results // miss_results.len() === #None in result (asserted above)
                    .push((miss_results[n_miss].0.clone(), CacheStatus::Miss));
                    n_miss += 1;
                }
            }
        }
        Ok(final_results)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomic::AtomicI32;
    use std::sync::atomic;

    #[derive(Clone, Debug)]
    struct ExtraOpt {
        error: bool,
        empty: bool,
        delay_for: Option<Duration>,
        used: Arc<AtomicI32>,
    }

    struct TestCB();

    #[async_trait]
    impl Lookup<i32, i32, ExtraOpt> for TestCB {
        async fn lookup(
            _key: &i32,
            extra: Option<&ExtraOpt>,
        ) -> Result<(i32, Option<Duration>), Box<dyn ErrorTrait + Send + Sync>> {
            // this function returns #lookup_times
            let mut used = 0;
            if let Some(e) = extra {
                used = e.used.fetch_add(1, atomic::Ordering::Relaxed) + 1;
                if e.error {
                    return Err(Error::new_str("test error"));
                }
                if let Some(delay_for) = e.delay_for {
                    tokio::time::sleep(delay_for).await;
                }
            }
            Ok((used, None))
        }
    }

    #[async_trait]
    impl MultiLookup<i32, i32, ExtraOpt> for TestCB {
        async fn multi_lookup(
            keys: &[&i32],
            extra: Option<&ExtraOpt>,
        ) -> Result<Vec<(i32, Option<Duration>)>, Box<dyn ErrorTrait + Send + Sync>> {
            let mut resp = vec![];
            if let Some(extra) = extra {
                if extra.empty {
                    return Ok(resp);
                }
            }
            for key in keys {
                resp.push((**key, None));
            }
            Ok(resp)
        }
    }

    #[tokio::test]
    async fn test_basic_get() {
        let cache: RTCache<i32, i32, TestCB, ExtraOpt> = RTCache::new(10, None, None);
        let opt = Some(ExtraOpt {
            error: false,
            empty: false,
            delay_for: None,
            used: Arc::new(AtomicI32::new(0)),
        });
        let (res, hit) = cache.get(&1, None, opt.as_ref()).await;
        assert_eq!(res.unwrap(), 1);
        assert_eq!(hit, CacheStatus::Miss);
        let (res, hit) = cache.get(&1, None, opt.as_ref()).await;
        assert_eq!(res.unwrap(), 1);
        assert_eq!(hit, CacheStatus::Hit);
    }

    #[tokio::test]
    async fn test_basic_get_error() {
        let cache: RTCache<i32, i32, TestCB, ExtraOpt> = RTCache::new(10, None, None);
        let opt1 = Some(ExtraOpt {
            error: true,
            empty: false,
            delay_for: None,
            used: Arc::new(AtomicI32::new(0)),
        });
        let (res, hit) = cache.get(&-1, None, opt1.as_ref()).await;
        assert!(res.is_err());
        assert_eq!(hit, CacheStatus::Miss);
    }

    #[tokio::test]
    async fn test_concurrent_get() {
        let cache: RTCache<i32, i32, TestCB, ExtraOpt> = RTCache::new(10, None, None);
        let cache = Arc::new(cache);
        let opt = Some(ExtraOpt {
            error: false,
            empty: false,
            delay_for: None,
            used: Arc::new(AtomicI32::new(0)),
        });
        let cache_c = cache.clone();
        let opt1 = opt.clone();
        // concurrent gets, only 1 will call the callback
        let t1 = tokio::spawn(async move {
            let (res, _hit) = cache_c.get(&1, None, opt1.as_ref()).await;
            res.unwrap()
        });
        let cache_c = cache.clone();
        let opt2 = opt.clone();
        let t2 = tokio::spawn(async move {
            let (res, _hit) = cache_c.get(&1, None, opt2.as_ref()).await;
            res.unwrap()
        });
        let opt3 = opt.clone();
        let cache_c = cache.clone();
        let t3 = tokio::spawn(async move {
            let (res, _hit) = cache_c.get(&1, None, opt3.as_ref()).await;
            res.unwrap()
        });
        let (r1, r2, r3) = tokio::join!(t1, t2, t3);
        assert_eq!(r1.unwrap(), 1);
        assert_eq!(r2.unwrap(), 1);
        assert_eq!(r3.unwrap(), 1);
    }

    #[tokio::test]
    async fn test_concurrent_get_error() {
        let cache: RTCache<i32, i32, TestCB, ExtraOpt> = RTCache::new(10, None, None);
        let cache = Arc::new(cache);
        let cache_c = cache.clone();
        let opt1 = Some(ExtraOpt {
            error: true,
            empty: false,
            delay_for: None,
            used: Arc::new(AtomicI32::new(0)),
        });
        let opt2 = opt1.clone();
        let opt3 = opt1.clone();
        // concurrent gets, only 1 will call the callback
        let t1 = tokio::spawn(async move {
            let (res, _hit) = cache_c.get(&-1, None, opt1.as_ref()).await;
            res.is_err()
        });
        let cache_c = cache.clone();
        let t2 = tokio::spawn(async move {
            let (res, _hit) = cache_c.get(&-1, None, opt2.as_ref()).await;
            res.is_err()
        });
        let cache_c = cache.clone();
        let t3 = tokio::spawn(async move {
            let (res, _hit) = cache_c.get(&-1, None, opt3.as_ref()).await;
            res.is_err()
        });
        let (r1, r2, r3) = tokio::join!(t1, t2, t3);
        assert!(r1.unwrap());
        assert!(r2.unwrap());
        assert!(r3.unwrap());
    }

    #[tokio::test]
    async fn test_concurrent_get_different_value() {
        let cache: RTCache<i32, i32, TestCB, ExtraOpt> = RTCache::new(10, None, None);
        let cache = Arc::new(cache);
        let opt1 = Some(ExtraOpt {
            error: false,
            empty: false,
            delay_for: None,
            used: Arc::new(AtomicI32::new(0)),
        });
        let opt2 = opt1.clone();
        let opt3 = opt1.clone();
        let cache_c = cache.clone();
        // concurrent gets to different keys, no locks, all will call the cb
        let t1 = tokio::spawn(async move {
            let (res, _hit) = cache_c.get(&1, None, opt1.as_ref()).await;
            res.unwrap()
        });
        let cache_c = cache.clone();
        let t2 = tokio::spawn(async move {
            let (res, _hit) = cache_c.get(&3, None, opt2.as_ref()).await;
            res.unwrap()
        });
        let cache_c = cache.clone();
        let t3 = tokio::spawn(async move {
            let (res, _hit) = cache_c.get(&5, None, opt3.as_ref()).await;
            res.unwrap()
        });
        let (r1, r2, r3) = tokio::join!(t1, t2, t3);
        // 1 lookup + 2 lookups + 3 lookups, order not matter
        assert_eq!(r1.unwrap() + r2.unwrap() + r3.unwrap(), 6);
    }

    #[tokio::test]
    async fn test_get_lock_age() {
        // 1 sec lock age
        let cache: RTCache<i32, i32, TestCB, ExtraOpt> =
            RTCache::new(10, Some(Duration::from_secs(1)), None);
        let cache = Arc::new(cache);
        let counter = Arc::new(AtomicI32::new(0));
        let opt1 = Some(ExtraOpt {
            error: false,
            empty: false,
            delay_for: Some(Duration::from_secs(2)),
            used: counter.clone(),
        });

        let opt2 = Some(ExtraOpt {
            error: false,
            empty: false,
            delay_for: None,
            used: counter.clone(),
        });
        let opt3 = opt2.clone();
        let cache_c = cache.clone();
        // t1 will be delay for 2 sec
        let t1 = tokio::spawn(async move {
            let (res, _hit) = cache_c.get(&1, None, opt1.as_ref()).await;
            res.unwrap()
        });
        // start t2 and t3 1.5 seconds later, since lock age is 1 sec, there will be no lock
        tokio::time::sleep(Duration::from_secs_f32(1.5)).await;
        let cache_c = cache.clone();
        let t2 = tokio::spawn(async move {
            let (res, _hit) = cache_c.get(&1, None, opt2.as_ref()).await;
            res.unwrap()
        });
        let cache_c = cache.clone();
        let t3 = tokio::spawn(async move {
            let (res, _hit) = cache_c.get(&1, None, opt3.as_ref()).await;
            res.unwrap()
        });
        let (r1, r2, r3) = tokio::join!(t1, t2, t3);
        // 1 lookup + 2 lookups + 3 lookups, order not matter
        assert_eq!(r1.unwrap() + r2.unwrap() + r3.unwrap(), 6);
    }

    #[tokio::test]
    async fn test_get_lock_timeout() {
        // 1 sec lock timeout
        let cache: RTCache<i32, i32, TestCB, ExtraOpt> =
            RTCache::new(10, None, Some(Duration::from_secs(1)));
        let cache = Arc::new(cache);
        let counter = Arc::new(AtomicI32::new(0));
        let opt1 = Some(ExtraOpt {
            error: false,
            empty: false,
            delay_for: Some(Duration::from_secs(2)),
            used: counter.clone(),
        });
        let opt2 = Some(ExtraOpt {
            error: false,
            empty: false,
            delay_for: None,
            used: counter.clone(),
        });
        let opt3 = opt2.clone();
        let cache_c = cache.clone();
        // t1 will be delay for 2 sec
        let t1 = tokio::spawn(async move {
            let (res, _hit) = cache_c.get(&1, None, opt1.as_ref()).await;
            res.unwrap()
        });
        // since lock timeout is 1 sec, t2 and t3 will do their own lookup after 1 sec
        let cache_c = cache.clone();
        let t2 = tokio::spawn(async move {
            let (res, _hit) = cache_c.get(&1, None, opt2.as_ref()).await;
            res.unwrap()
        });
        let cache_c = cache.clone();
        let t3 = tokio::spawn(async move {
            let (res, _hit) = cache_c.get(&1, None, opt3.as_ref()).await;
            res.unwrap()
        });
        let (r1, r2, r3) = tokio::join!(t1, t2, t3);
        // 1 lookup + 2 lookups + 3 lookups, order not matter
        assert_eq!(r1.unwrap() + r2.unwrap() + r3.unwrap(), 6);
    }

    #[tokio::test]
    async fn test_multi_get() {
        let cache: RTCache<i32, i32, TestCB, ExtraOpt> = RTCache::new(10, None, None);
        let counter = Arc::new(AtomicI32::new(0));
        let opt1 = Some(ExtraOpt {
            error: false,
            empty: false,
            delay_for: Some(Duration::from_secs(2)),
            used: counter.clone(),
        });
        // make 1 a hit first
        let (res, hit) = cache.get(&1, None, opt1.as_ref()).await;
        assert_eq!(res.unwrap(), 1);
        assert_eq!(hit, CacheStatus::Miss);
        let (res, hit) = cache.get(&1, None, opt1.as_ref()).await;
        assert_eq!(res.unwrap(), 1);
        assert_eq!(hit, CacheStatus::Hit);
        // 1 hit 2 miss 3 miss
        let resp = cache
            .multi_get([1, 2, 3].iter(), None, opt1.as_ref())
            .await
            .unwrap();
        assert_eq!(resp[0].0, 1);
        assert_eq!(resp[0].1, CacheStatus::Hit);
        assert_eq!(resp[1].0, 2);
        assert_eq!(resp[1].1, CacheStatus::Miss);
        assert_eq!(resp[2].0, 3);
        assert_eq!(resp[2].1, CacheStatus::Miss);
        // all hits after a fetch
        let resp = cache
            .multi_get([1, 2, 3].iter(), None, opt1.as_ref())
            .await
            .unwrap();
        assert_eq!(resp[0].0, 1);
        assert_eq!(resp[0].1, CacheStatus::Hit);
        assert_eq!(resp[1].0, 2);
        assert_eq!(resp[1].1, CacheStatus::Hit);
        assert_eq!(resp[2].0, 3);
        assert_eq!(resp[2].1, CacheStatus::Hit);
    }

    #[tokio::test]
    #[should_panic(expected = "multi_lookup() failed to return the matching number of results")]
    async fn test_inconsistent_miss_results() {
        // force an empty result
        let opt1 = Some(ExtraOpt {
            error: false,
            empty: true,
            delay_for: None,
            used: Arc::new(AtomicI32::new(0)),
        });
        let cache: RTCache<i32, i32, TestCB, ExtraOpt> = RTCache::new(10, None, None);
        cache
            .multi_get([4, 5, 6].iter(), None, opt1.as_ref())
            .await
            .unwrap();
    }
}
