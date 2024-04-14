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

//! Cache lock

use crate::key::CacheHashKey;

use crate::hashtable::ConcurrentHashTable;
use pingora_timeout::timeout;
use std::sync::Arc;

const N_SHARDS: usize = 16;

/// The global cache locking manager
pub struct CacheLock {
    lock_table: ConcurrentHashTable<LockStub, N_SHARDS>,
    timeout: Duration, // fixed timeout value for now
}

/// A struct representing locked cache access
#[derive(Debug)]
pub enum Locked {
    /// The writer is allowed to fetch the asset
    Write(WritePermit),
    /// The reader waits for the writer to fetch the asset
    Read(ReadLock),
}

impl Locked {
    /// Is this a write lock
    pub fn is_write(&self) -> bool {
        matches!(self, Self::Write(_))
    }
}

impl CacheLock {
    /// Create a new [CacheLock] with the given lock timeout
    ///
    /// When the timeout is reached, the read locks are automatically unlocked
    pub fn new(timeout: Duration) -> Self {
        CacheLock {
            lock_table: ConcurrentHashTable::new(),
            timeout,
        }
    }

    /// Try to lock a cache fetch
    ///
    /// Users should call after a cache miss before fetching the asset.
    /// The returned [Locked] will tell the caller either to fetch or wait.
    pub fn lock<K: CacheHashKey>(&self, key: &K) -> Locked {
        let hash = key.combined_bin();
        let key = u128::from_be_bytes(hash); // endianness doesn't matter
        let table = self.lock_table.get(key);
        if let Some(lock) = table.read().get(&key) {
            // already has an ongoing request
            if lock.0.lock_status() != LockStatus::Dangling {
                return Locked::Read(lock.read_lock());
            }
            // Dangling: the previous writer quit without unlocking the lock. Requests should
            // compete for the write lock again.
        }

        let (permit, stub) = WritePermit::new(self.timeout);
        let mut table = table.write();
        // check again in case another request already added it
        if let Some(lock) = table.get(&key) {
            if lock.0.lock_status() != LockStatus::Dangling {
                return Locked::Read(lock.read_lock());
            }
        }
        table.insert(key, stub);
        Locked::Write(permit)
    }

    /// Release a lock for the given key
    ///
    /// When the write lock is dropped without being released, the read lock holders will consider
    /// it to be failed so that they will compete for the write lock again.
    pub fn release<K: CacheHashKey>(&self, key: &K, reason: LockStatus) {
        let hash = key.combined_bin();
        let key = u128::from_be_bytes(hash); // endianness doesn't matter
        if let Some(lock) = self.lock_table.write(key).remove(&key) {
            // make sure that the caller didn't forget to unlock it
            if lock.0.locked() {
                lock.0.unlock(reason);
            }
        }
    }
}

use std::sync::atomic::{AtomicU8, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::Semaphore;

/// Status which the read locks could possibly see.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum LockStatus {
    /// Waiting for the writer to populate the asset
    Waiting,
    /// The writer finishes, readers can start
    Done,
    /// The writer encountered error, such as network issue. A new writer will be elected.
    TransientError,
    /// The writer observed that no cache lock is needed (e.g., uncacheable), readers should start
    /// to fetch independently without a new writer
    GiveUp,
    /// The write lock is dropped without being unlocked
    Dangling,
    /// The lock is held for too long
    Timeout,
}

impl From<LockStatus> for u8 {
    fn from(l: LockStatus) -> u8 {
        match l {
            LockStatus::Waiting => 0,
            LockStatus::Done => 1,
            LockStatus::TransientError => 2,
            LockStatus::GiveUp => 3,
            LockStatus::Dangling => 4,
            LockStatus::Timeout => 5,
        }
    }
}

impl From<u8> for LockStatus {
    fn from(v: u8) -> Self {
        match v {
            0 => Self::Waiting,
            1 => Self::Done,
            2 => Self::TransientError,
            3 => Self::GiveUp,
            4 => Self::Dangling,
            5 => Self::Timeout,
            _ => Self::GiveUp, // placeholder
        }
    }
}

#[derive(Debug)]
struct LockCore {
    pub lock_start: Instant,
    pub timeout: Duration,
    pub(super) lock: Semaphore,
    // use u8 for Atomic enum
    lock_status: AtomicU8,
}

impl LockCore {
    pub fn new_arc(timeout: Duration) -> Arc<Self> {
        Arc::new(LockCore {
            lock: Semaphore::new(0),
            timeout,
            lock_start: Instant::now(),
            lock_status: AtomicU8::new(LockStatus::Waiting.into()),
        })
    }

    fn locked(&self) -> bool {
        self.lock.available_permits() == 0
    }

    fn unlock(&self, reason: LockStatus) {
        self.lock_status.store(reason.into(), Ordering::SeqCst);
        // Any small positive number will do, 10 is used for RwLock as well.
        // No need to wake up all at once.
        self.lock.add_permits(10);
    }

    fn lock_status(&self) -> LockStatus {
        self.lock_status.load(Ordering::Relaxed).into()
    }
}

// all 3 structs below are just Arc<LockCore> with different interfaces

/// ReadLock: the requests who get it need to wait until it is released
#[derive(Debug)]
pub struct ReadLock(Arc<LockCore>);

impl ReadLock {
    /// Wait for the writer to release the lock
    pub async fn wait(&self) {
        if !self.locked() || self.expired() {
            return;
        }

        // TODO: should subtract now - start so that the lock don't wait beyond start + timeout
        // Also need to be careful not to wake everyone up at the same time
        // (maybe not an issue because regular cache lock release behaves that way)
        let _ = timeout(self.0.timeout, self.0.lock.acquire()).await;
        // permit is returned to Semaphore right away
    }

    /// Test if it is still locked
    pub fn locked(&self) -> bool {
        self.0.locked()
    }

    /// Whether the lock is expired, e.g., the writer has been holding the lock for too long
    pub fn expired(&self) -> bool {
        // NOTE: this whether the lock is currently expired
        // not whether it was timed out during wait()
        self.0.lock_start.elapsed() >= self.0.timeout
    }

    /// The current status of the lock
    pub fn lock_status(&self) -> LockStatus {
        let status = self.0.lock_status();
        if matches!(status, LockStatus::Waiting) && self.expired() {
            LockStatus::Timeout
        } else {
            status
        }
    }
}

/// WritePermit: requires who get it need to populate the cache and then release it
#[derive(Debug)]
pub struct WritePermit(Arc<LockCore>);

impl WritePermit {
    fn new(timeout: Duration) -> (WritePermit, LockStub) {
        let lock = LockCore::new_arc(timeout);
        let stub = LockStub(lock.clone());
        (WritePermit(lock), stub)
    }

    fn unlock(&self, reason: LockStatus) {
        self.0.unlock(reason)
    }
}

impl Drop for WritePermit {
    fn drop(&mut self) {
        // Writer exited without properly unlocking. We let others to compete for the write lock again
        if self.0.locked() {
            self.unlock(LockStatus::Dangling);
        }
    }
}

struct LockStub(Arc<LockCore>);
impl LockStub {
    pub fn read_lock(&self) -> ReadLock {
        ReadLock(self.0.clone())
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::CacheKey;

    #[test]
    fn test_get_release() {
        let cache_lock = CacheLock::new(Duration::from_secs(1000));
        let key1 = CacheKey::new("", "a", "1");
        let locked1 = cache_lock.lock(&key1);
        assert!(locked1.is_write()); // write permit
        let locked2 = cache_lock.lock(&key1);
        assert!(!locked2.is_write()); // read lock
        cache_lock.release(&key1, LockStatus::Done);
        let locked3 = cache_lock.lock(&key1);
        assert!(locked3.is_write()); // write permit again
    }

    #[tokio::test]
    async fn test_lock() {
        let cache_lock = CacheLock::new(Duration::from_secs(1000));
        let key1 = CacheKey::new("", "a", "1");
        let permit = match cache_lock.lock(&key1) {
            Locked::Write(w) => w,
            _ => panic!(),
        };
        let lock = match cache_lock.lock(&key1) {
            Locked::Read(r) => r,
            _ => panic!(),
        };
        assert!(lock.locked());
        let handle = tokio::spawn(async move {
            lock.wait().await;
            assert_eq!(lock.lock_status(), LockStatus::Done);
        });
        permit.unlock(LockStatus::Done);
        handle.await.unwrap(); // check lock is unlocked and the task is returned
    }

    #[tokio::test]
    async fn test_lock_timeout() {
        let cache_lock = CacheLock::new(Duration::from_secs(1));
        let key1 = CacheKey::new("", "a", "1");
        let permit = match cache_lock.lock(&key1) {
            Locked::Write(w) => w,
            _ => panic!(),
        };
        let lock = match cache_lock.lock(&key1) {
            Locked::Read(r) => r,
            _ => panic!(),
        };
        assert!(lock.locked());

        let handle = tokio::spawn(async move {
            // timed out
            lock.wait().await;
            assert_eq!(lock.lock_status(), LockStatus::Timeout);
        });

        tokio::time::sleep(Duration::from_secs(2)).await;

        // expired lock
        let lock2 = match cache_lock.lock(&key1) {
            Locked::Read(r) => r,
            _ => panic!(),
        };
        assert!(lock2.locked());
        assert_eq!(lock2.lock_status(), LockStatus::Timeout);
        lock2.wait().await;
        assert_eq!(lock2.lock_status(), LockStatus::Timeout);

        permit.unlock(LockStatus::Done);
        handle.await.unwrap();
    }
}
