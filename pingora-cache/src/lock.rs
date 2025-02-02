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

//! Cache lock

use crate::{hashtable::ConcurrentHashTable, key::CacheHashKey, CacheKey};

use pingora_timeout::timeout;
use std::sync::Arc;

pub type CacheKeyLockImpl = (dyn CacheKeyLock + Send + Sync);

pub trait CacheKeyLock {
    /// Try to lock a cache fetch
    ///
    /// Users should call after a cache miss before fetching the asset.
    /// The returned [Locked] will tell the caller either to fetch or wait.
    fn lock(&self, key: &CacheKey) -> Locked;

    /// Release a lock for the given key
    ///
    /// When the write lock is dropped without being released, the read lock holders will consider
    /// it to be failed so that they will compete for the write lock again.
    fn release(&self, key: &CacheKey, permit: WritePermit, reason: LockStatus);
}

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
    pub fn new_boxed(timeout: Duration) -> Box<Self> {
        Box::new(CacheLock {
            lock_table: ConcurrentHashTable::new(),
            timeout,
        })
    }

    /// Create a new [CacheLock] with the given lock timeout
    ///
    /// When the timeout is reached, the read locks are automatically unlocked
    pub fn new(timeout: Duration) -> Self {
        CacheLock {
            lock_table: ConcurrentHashTable::new(),
            timeout,
        }
    }
}

impl CacheKeyLock for CacheLock {
    fn lock(&self, key: &CacheKey) -> Locked {
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

        let mut table = table.write();
        // check again in case another request already added it
        if let Some(lock) = table.get(&key) {
            if lock.0.lock_status() != LockStatus::Dangling {
                return Locked::Read(lock.read_lock());
            }
        }
        let (permit, stub) = WritePermit::new(self.timeout);
        table.insert(key, stub);
        Locked::Write(permit)
    }

    fn release(&self, key: &CacheKey, mut permit: WritePermit, reason: LockStatus) {
        let hash = key.combined_bin();
        let key = u128::from_be_bytes(hash); // endianness doesn't matter
        if let Some(_lock) = self.lock_table.write(key).remove(&key) {
            // make sure that the caller didn't forget to unlock it
            permit.unlock(reason);
        }
    }
}

use log::warn;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::{Duration, Instant};
use strum::IntoStaticStr;
use tokio::sync::Semaphore;

/// Status which the read locks could possibly see.
#[derive(Debug, Copy, Clone, PartialEq, Eq, IntoStaticStr)]
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
pub struct LockCore {
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

    pub fn locked(&self) -> bool {
        self.lock.available_permits() == 0
    }

    pub fn unlock(&self, reason: LockStatus) {
        self.lock_status.store(reason.into(), Ordering::SeqCst);
        // Any small positive number will do, 10 is used for RwLock as well.
        // No need to wake up all at once.
        self.lock.add_permits(10);
    }

    pub fn lock_status(&self) -> LockStatus {
        self.lock_status.load(Ordering::SeqCst).into()
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

        // TODO: need to be careful not to wake everyone up at the same time
        // (maybe not an issue because regular cache lock release behaves that way)
        if let Some(duration) = self.0.timeout.checked_sub(self.0.lock_start.elapsed()) {
            match timeout(duration, self.0.lock.acquire()).await {
                Ok(Ok(_)) => { // permit is returned to Semaphore right away
                }
                Ok(Err(e)) => {
                    warn!("error acquiring semaphore {e:?}")
                }
                Err(_) => {
                    self.0
                        .lock_status
                        .store(LockStatus::Timeout.into(), Ordering::SeqCst);
                }
            }
        }
    }

    /// Test if it is still locked
    pub fn locked(&self) -> bool {
        self.0.locked()
    }

    /// Whether the lock is expired, e.g., the writer has been holding the lock for too long
    pub fn expired(&self) -> bool {
        // NOTE: this is whether the lock is currently expired
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
pub struct WritePermit {
    lock: Arc<LockCore>,
    finished: bool,
}

impl WritePermit {
    pub fn new(timeout: Duration) -> (WritePermit, LockStub) {
        let lock = LockCore::new_arc(timeout);
        let stub = LockStub(lock.clone());
        (
            WritePermit {
                lock,
                finished: false,
            },
            stub,
        )
    }

    pub fn unlock(&mut self, reason: LockStatus) {
        self.finished = true;
        self.lock.unlock(reason);
    }
}

impl Drop for WritePermit {
    fn drop(&mut self) {
        // Writer exited without properly unlocking. We let others to compete for the write lock again
        if !self.finished {
            debug_assert!(false, "Dangling cache lock started!");
            self.unlock(LockStatus::Dangling);
        }
    }
}

pub struct LockStub(pub Arc<LockCore>);
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
        let cache_lock = CacheLock::new_boxed(Duration::from_secs(1000));
        let key1 = CacheKey::new("", "a", "1");
        let locked1 = cache_lock.lock(&key1);
        assert!(locked1.is_write()); // write permit
        let locked2 = cache_lock.lock(&key1);
        assert!(!locked2.is_write()); // read lock
        if let Locked::Write(permit) = locked1 {
            cache_lock.release(&key1, permit, LockStatus::Done);
        }
        let locked3 = cache_lock.lock(&key1);
        assert!(locked3.is_write()); // write permit again
        if let Locked::Write(permit) = locked3 {
            cache_lock.release(&key1, permit, LockStatus::Done);
        }
    }

    #[tokio::test]
    async fn test_lock() {
        let cache_lock = CacheLock::new_boxed(Duration::from_secs(1000));
        let key1 = CacheKey::new("", "a", "1");
        let mut permit = match cache_lock.lock(&key1) {
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
        let cache_lock = CacheLock::new_boxed(Duration::from_secs(1));
        let key1 = CacheKey::new("", "a", "1");
        let mut permit = match cache_lock.lock(&key1) {
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

    #[tokio::test]
    async fn test_lock_concurrent() {
        let _ = env_logger::builder().is_test(true).try_init();
        // Test that concurrent attempts to compete for a lock run without issues
        let cache_lock = Arc::new(CacheLock::new_boxed(Duration::from_secs(1)));
        let key1 = CacheKey::new("", "a", "1");

        let mut handles = vec![];

        const READERS: usize = 30;
        for _ in 0..READERS {
            let key1 = key1.clone();
            let cache_lock = cache_lock.clone();
            // simulate a cache lookup / lock attempt loop
            handles.push(tokio::spawn(async move {
                // timed out
                loop {
                    match cache_lock.lock(&key1) {
                        Locked::Write(permit) => {
                            let _ = tokio::time::sleep(Duration::from_millis(5)).await;
                            cache_lock.release(&key1, permit, LockStatus::Done);
                            break;
                        }
                        Locked::Read(r) => {
                            r.wait().await;
                        }
                    }
                }
            }));
        }

        for handle in handles {
            handle.await.unwrap();
        }
    }
}
