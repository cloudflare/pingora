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

//! Cache lock

use crate::trace::{Span, Tag};
use crate::{hashtable::ConcurrentHashTable, key::CacheHashKey, CacheKey, NoCacheReason};

use http::Extensions;
use pingora_timeout::timeout;
use std::sync::Arc;
use std::time::Duration;

pub type CacheKeyLockImpl = dyn CacheKeyLock + Send + Sync;

pub trait CacheKeyLock {
    /// Try to lock a cache fetch
    ///
    /// If `stale_writer` is true, this fetch is to revalidate an asset already in cache.
    /// Else this fetch was a cache miss (i.e. not found via lookup, or force missed).
    ///
    /// Users should call after a cache miss before fetching the asset.
    /// The returned [Locked] will tell the caller either to fetch or wait.
    fn lock(&self, key: &CacheKey, stale_writer: bool) -> Locked;

    /// Release a lock for the given key
    ///
    /// When the write lock is dropped without being released, the read lock holders will consider
    /// it to be failed so that they will compete for the write lock again.
    fn release(&self, key: &CacheKey, permit: WritePermit, reason: LockStatus);

    /// Set tags on a trace span for the cache lock wait.
    fn trace_lock_wait(&self, span: &mut Span, _read_lock: &ReadLock, lock_status: LockStatus) {
        let tag_value: &'static str = lock_status.into();
        span.set_tag(|| Tag::new("status", tag_value));
    }

    /// Set a lock status for a custom `NoCacheReason`.
    fn custom_lock_status(&self, _custom_no_cache: &'static str) -> LockStatus {
        // treat custom no cache reasons as GiveUp by default
        // (like OriginNotCache)
        LockStatus::GiveUp
    }
}

const N_SHARDS: usize = 16;

/// The global cache locking manager
#[derive(Debug)]
pub struct CacheLock {
    lock_table: ConcurrentHashTable<LockStub, N_SHARDS>,
    // fixed lock timeout values for now
    age_timeout_default: Duration,
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
    /// Age timeout refers to how long a writer has been holding onto a particular lock, and wait
    /// timeout refers to how long a reader may hold onto any number of locks before giving up.
    /// When either timeout is reached, the read locks are automatically unlocked.
    pub fn new_boxed(age_timeout: Duration) -> Box<Self> {
        Box::new(CacheLock {
            lock_table: ConcurrentHashTable::new(),
            age_timeout_default: age_timeout,
        })
    }

    /// Create a new [CacheLock] with the given lock timeout
    ///
    /// Age timeout refers to how long a writer has been holding onto a particular lock, and wait
    /// timeout refers to how long a reader may hold onto any number of locks before giving up.
    /// When either timeout is reached, the read locks are automatically unlocked.
    pub fn new(age_timeout_default: Duration) -> Self {
        CacheLock {
            lock_table: ConcurrentHashTable::new(),
            age_timeout_default,
        }
    }
}

impl CacheKeyLock for CacheLock {
    fn lock(&self, key: &CacheKey, stale_writer: bool) -> Locked {
        let hash = key.combined_bin();
        let key = u128::from_be_bytes(hash); // endianness doesn't matter
        let table = self.lock_table.get(key);
        if let Some(lock) = table.read().get(&key) {
            // already has an ongoing request
            // If the lock status is dangling or timeout, the lock will _remain_ in the table
            // and readers should attempt to replace it.
            // In the case of writer timeout, any remaining readers that were waiting on THIS
            // LockCore should have (or are about to) timed out on their own.
            // Finding a Timeout status means that THIS writer's lock already expired, so future
            // requests ought to recreate the lock.
            if !matches!(
                lock.0.lock_status(),
                LockStatus::Dangling | LockStatus::AgeTimeout
            ) {
                return Locked::Read(lock.read_lock());
            }
            // Dangling: the previous writer quit without unlocking the lock. Requests should
            // compete for the write lock again.
        }

        let mut table = table.write();
        // check again in case another request already added it
        if let Some(lock) = table.get(&key) {
            if !matches!(
                lock.0.lock_status(),
                LockStatus::Dangling | LockStatus::AgeTimeout
            ) {
                return Locked::Read(lock.read_lock());
            }
        }
        let (permit, stub) =
            WritePermit::new(self.age_timeout_default, stale_writer, Extensions::new());
        table.insert(key, stub);
        Locked::Write(permit)
    }

    fn release(&self, key: &CacheKey, mut permit: WritePermit, reason: LockStatus) {
        let hash = key.combined_bin();
        let key = u128::from_be_bytes(hash); // endianness doesn't matter
        if permit.lock.lock_status() == LockStatus::AgeTimeout {
            // if lock age timed out, then readers are capable of
            // replacing the lock associated with this permit from the lock table
            // (see lock() implementation)
            // keep the lock status as Timeout accordingly when unlocking
            // (because we aren't removing it from the lock_table)
            permit.unlock(LockStatus::AgeTimeout);
        } else if let Some(_lock) = self.lock_table.write(key).remove(&key) {
            permit.unlock(reason);
        }
        // these situations above should capture all possible options,
        // else dangling cache lock may start
    }
}

use log::warn;
use parking_lot::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::time::Instant;
use strum::{FromRepr, IntoStaticStr};
use tokio::sync::{oneshot, Semaphore};

/// Status which the read locks could possibly see.
#[derive(Debug, Copy, Clone, PartialEq, Eq, IntoStaticStr, FromRepr)]
#[repr(u8)]
pub enum LockStatus {
    /// Waiting for the writer to populate the asset
    Waiting = 0,
    /// The writer finishes, readers can start
    Done = 1,
    /// The writer encountered error, such as network issue. A new writer will be elected.
    TransientError = 2,
    /// The writer observed that no cache lock is needed (e.g., uncacheable), readers should start
    /// to fetch independently without a new writer
    GiveUp = 3,
    /// The write lock is dropped without being unlocked
    Dangling = 4,
    /// Reader has held onto cache locks for too long, give up
    WaitTimeout = 5,
    /// The lock is held for too long by the writer
    AgeTimeout = 6,
}

impl From<LockStatus> for u8 {
    fn from(l: LockStatus) -> u8 {
        match l {
            LockStatus::Waiting => 0,
            LockStatus::Done => 1,
            LockStatus::TransientError => 2,
            LockStatus::GiveUp => 3,
            LockStatus::Dangling => 4,
            LockStatus::WaitTimeout => 5,
            LockStatus::AgeTimeout => 6,
        }
    }
}

impl From<u8> for LockStatus {
    fn from(v: u8) -> Self {
        Self::from_repr(v).unwrap_or(Self::GiveUp)
    }
}

#[derive(Debug)]
pub struct LockCore {
    pub lock_start: Instant,
    pub age_timeout: Duration,
    pub(super) lock: Semaphore,
    // use u8 for Atomic enum
    lock_status: AtomicU8,
    stale_writer: bool,
    extensions: Extensions,
    /// What the writer has said about its fill, and who is waiting to hear it. One
    /// mutex for both, so nothing publishes in the gap between a reader checking and
    /// registering.
    fill: Mutex<FillState>,
    /// Lets `publish` skip the lock when replacing nothing with nothing. Written
    /// under `fill` so it cannot disagree with `published`; a stale `false` read only
    /// means a racing publication.
    published_any: AtomicBool,
}

#[derive(Debug, Default)]
struct FillState {
    /// Replaced, not accumulated: see [`WritePermit::publish`].
    published: Vec<u64>,
    /// Readers to be told about a later publication, in no meaningful order. Nobody
    /// removes themselves: a departure is a closed sender, swept by whoever next
    /// holds this lock.
    waiters: Vec<TokenWaiter>,
    /// Where the next sweep resumes. Sweeps examine a bounded window, so the cursor
    /// advances past survivors and wraps to stop entries hiding behind live ones.
    sweep_cursor: usize,
}

/// Entries examined per registration. The vector settles near
/// `live * BUDGET / (BUDGET - 1)`.
const WAITER_SWEEP_BUDGET: usize = 8;

#[derive(Debug)]
struct TokenWaiter {
    /// Shared with the reader's [`UnusableFills`], so registering is a refcount bump.
    unusable: Arc<[UnusableFill]>,
    /// Also the liveness signal: closed once the reader drops its receiver.
    tell: oneshot::Sender<UnusableFill>,
}

impl LockCore {
    pub fn new_arc(timeout: Duration, stale_writer: bool, extensions: Extensions) -> Arc<Self> {
        Arc::new(LockCore {
            lock: Semaphore::new(0),
            age_timeout: timeout,
            lock_start: Instant::now(),
            lock_status: AtomicU8::new(LockStatus::Waiting.into()),
            stale_writer,
            extensions,
            fill: Mutex::new(FillState::default()),
            published_any: AtomicBool::new(false),
        })
    }

    pub fn locked(&self) -> bool {
        self.lock.available_permits() == 0
    }

    /// Check what is published, and if none of it matters, register for later
    /// publications. One lock for both, or a publication lands in the gap.
    fn watch_fill(&self, unusable: &Arc<[UnusableFill]>) -> Watching {
        let mut fill = self.fill.lock();
        if let Some(matched) = first_match(&fill.published, unusable) {
            return Watching::AlreadyPublished(matched);
        }

        // On the lock this registration takes anyway, and bounded so one
        // registration cannot end up scanning every reader ahead of it.
        let mut budget = WAITER_SWEEP_BUDGET;
        while budget > 0 && !fill.waiters.is_empty() {
            if fill.sweep_cursor >= fill.waiters.len() {
                fill.sweep_cursor = 0;
            }
            let at = fill.sweep_cursor;
            if fill.waiters[at].tell.is_closed() {
                // Cursor stays: `swap_remove` moves an unexamined entry here.
                fill.waiters.swap_remove(at);
            } else {
                fill.sweep_cursor += 1;
            }
            budget -= 1;
        }

        let (tell, told) = oneshot::channel();
        fill.waiters.push(TokenWaiter {
            unusable: unusable.clone(),
            tell,
        });
        Watching::Registered(told)
    }

    /// Replace what is published and wake only the readers it matters to.
    fn publish(&self, tokens: &[u64]) {
        let woken = {
            let mut fill = self.fill.lock();
            fill.published.clear();
            fill.published.extend_from_slice(tokens);
            // Same lock as `published`, so racing publications cannot disagree.
            self.published_any
                .store(!tokens.is_empty(), Ordering::Relaxed);

            // One pass in place: departed readers dropped, matched readers taken out
            // to be told once the lock is released. An empty `woken` never allocates.
            let mut woken = Vec::new();
            let mut i = 0;
            while i < fill.waiters.len() {
                if fill.waiters[i].tell.is_closed() {
                    fill.waiters.swap_remove(i);
                    continue;
                }
                match first_match(tokens, &fill.waiters[i].unusable) {
                    Some(matched) => woken.push((fill.waiters.swap_remove(i).tell, matched)),
                    None => i += 1,
                }
            }
            woken
        };

        // Sent after releasing the lock.
        for (tell, matched) in woken {
            let _ = tell.send(matched);
        }
    }

    /// Live readers only, so an assertion does not depend on when a sweep last ran.
    #[cfg(test)]
    fn registered_waiters(&self) -> usize {
        self.fill
            .lock()
            .waiters
            .iter()
            .filter(|waiter| !waiter.tell.is_closed())
            .count()
    }

    /// Entries held, swept or not. Only the sweep bound should assert on this.
    #[cfg(test)]
    fn retained_waiters(&self) -> usize {
        self.fill.lock().waiters.len()
    }

    pub fn unlock(&self, reason: LockStatus) {
        assert!(
            reason != LockStatus::WaitTimeout,
            "WaitTimeout is not stored in LockCore"
        );
        self.lock_status.store(reason.into(), Ordering::SeqCst);
        // Any small positive number will do, 10 is used for RwLock as well.
        // No need to wake up all at once.
        self.lock.add_permits(10);
    }

    pub fn lock_status(&self) -> LockStatus {
        self.lock_status.load(Ordering::SeqCst).into()
    }

    /// Was this lock for a stale cache fetch writer?
    pub fn stale_writer(&self) -> bool {
        self.stale_writer
    }

    pub fn extensions(&self) -> &Extensions {
        &self.extensions
    }
}

// all 3 structs below are just Arc<LockCore> with different interfaces

/// ReadLock: the requests who get it need to wait until it is released
#[derive(Debug)]
pub struct ReadLock(Arc<LockCore>);

/// One fill a reader cannot use, and why. Per token rather than per set, because a
/// key has one [`UnusableFills`] and a set-wide reason would attribute one part of
/// an application's give-up to another's cause.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnusableFill {
    /// Names a fill this reader cannot use. Opaque to the cache: whatever the
    /// writer and reader agree it means.
    pub token: u64,
    /// Why, as the [`crate::NoCacheReason::Custom`] label the cache disables with.
    /// Not a whole [`crate::NoCacheReason`]: the cause belongs to the application,
    /// and a caller could otherwise pass [`crate::NoCacheReason::NeverEnabled`],
    /// which [`crate::HttpCache::disable`] rejects.
    pub reason: &'static str,
}

/// The first published fill this reader cannot use, in the *reader's* order. Both
/// sides are a handful of tokens, so a plain scan.
fn first_match(published: &[u64], unusable: &[UnusableFill]) -> Option<UnusableFill> {
    unusable
        .iter()
        .find(|candidate| published.contains(&candidate.token))
        .copied()
}

/// What happened to *this* reader's cache-lock wait, and so what to do next.
///
/// Distinct from [`LockStatus`], the lock's *shared* state, which cannot carry a
/// payload. This is per-reader and never stored, which is what lets
/// [`Self::Abandoned`] carry its cause.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockWaitOutcome {
    /// The writer populated the asset. Look it up again.
    Done,
    /// The writer hit a transient error such as a network failure. Compete to be
    /// the new writer.
    TransientError,
    /// The write lock was dropped without being unlocked. A bug, but the request
    /// recovers by competing to be the new writer.
    Dangling,
    /// This reader has spent too long waiting on cache locks.
    WaitTimeout,
    /// The writer held the lock past its age. Compete for it rather than waiting,
    /// so a stuck writer cannot pin every reader to it.
    AgeTimeout,
    /// The writer found no lock was needed, for instance because the asset turned
    /// out to be uncacheable. Every reader proceeds uncached.
    GiveUp,
    /// This reader stopped waiting because the writer published a fill it cannot
    /// use. Local to this reader: the lock status is untouched and every other
    /// reader goes on coalescing behind the same writer.
    Abandoned {
        /// Why, from the matched [`UnusableFill`].
        reason: NoCacheReason,
        /// The published token it matched.
        token: u64,
    },
}

impl LockWaitOutcome {
    /// The nearest shared-status label, for tracing and metrics only.
    /// [`Self::Abandoned`] reports [`LockStatus::GiveUp`] because it shares that
    /// handling, but is never *stored* as such.
    pub fn lock_status(&self) -> LockStatus {
        match self {
            LockWaitOutcome::Done => LockStatus::Done,
            LockWaitOutcome::TransientError => LockStatus::TransientError,
            LockWaitOutcome::Dangling => LockStatus::Dangling,
            LockWaitOutcome::WaitTimeout => LockStatus::WaitTimeout,
            LockWaitOutcome::AgeTimeout => LockStatus::AgeTimeout,
            LockWaitOutcome::GiveUp | LockWaitOutcome::Abandoned { .. } => LockStatus::GiveUp,
        }
    }
}

/// The fills a reader cannot use. Put one in a [`crate::CacheKey`]'s extensions
/// and [`crate::HttpCache::cache_lock_wait`] honours it. Empty waits as normal.
///
/// One set and one publisher per key: extensions hold one value per type, and each
/// [`WritePermit::publish`] supersedes the last, so two features on a key must agree
/// on a combined set. Distinct token ranges stop tokens colliding but the space is
/// still flat and shared.
///
/// A match costs the reader its caching -- [`LockWaitOutcome::Abandoned`] means an
/// uncached fetch, where [`LockWaitOutcome::AgeTimeout`] recompetes and can still
/// cache. Name only fills that genuinely cannot be waited for.
#[derive(Debug, Clone)]
pub struct UnusableFills {
    pub(crate) fills: Arc<[UnusableFill]>,
}

impl UnusableFills {
    /// `fills` in precedence order: if several are published at once, the earliest
    /// here is reported back.
    pub fn new(fills: impl Into<Arc<[UnusableFill]>>) -> Self {
        UnusableFills {
            fills: fills.into(),
        }
    }

    /// The fills this reader cannot use, in precedence order.
    pub fn fills(&self) -> &[UnusableFill] {
        &self.fills
    }

    /// Which of `published` this reader cannot use, or `None` to keep waiting. The
    /// same function the wait uses, so a caller can check its token convention
    /// against the real rule rather than a copy.
    pub fn first_match(&self, published: &[u64]) -> Option<UnusableFill> {
        first_match(published, &self.fills)
    }
}

/// How a wait behind a writer ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitOutcome {
    /// The writer released the lock. Read [`ReadLock::lock_status`] for what it left
    /// behind.
    Released,
    /// The writer held the lock for longer than its age allows.
    AgeTimeout,
    /// The writer published a fill this reader cannot use.
    Abandoned(UnusableFill),
}

enum Watching {
    Registered(oneshot::Receiver<UnusableFill>),
    AlreadyPublished(UnusableFill),
}

impl ReadLock {
    /// Wait for the writer to release the lock
    pub async fn wait(&self) {
        self.wait_inner(None).await;
    }

    /// Wait for the writer to release the lock, unless it publishes a fill in
    /// `unusable` first — which lets a reader act on what the writer only learns
    /// after the reader committed to waiting.
    ///
    /// Giving up is local to this reader: the lock status is untouched and every
    /// other reader goes on coalescing. Tokens never published never match.
    pub async fn wait_unless_published(&self, unusable: &UnusableFills) -> WaitOutcome {
        self.wait_inner(Some(&unusable.fills)).await
    }

    async fn wait_inner(&self, unusable: Option<&Arc<[UnusableFill]>>) -> WaitOutcome {
        if !self.locked() {
            return WaitOutcome::Released;
        }

        // FIXME: for now it is the awkward responsibility of the ReadLock to set the
        // timeout status on the lock itself because the write permit cannot lock age
        // timeout on its own
        // TODO: need to be careful not to wake everyone up at the same time
        // (maybe not an issue because regular cache lock release behaves that way)
        //
        // Checked before any published token: an expired lock has to be replaced
        // whatever the writer said, and the caller can then recompete and still
        // cache. Also the only path that stores `AgeTimeout`.
        let Some(duration) = self.0.age_timeout.checked_sub(self.0.lock_start.elapsed()) else {
            // expiration has already occurred, store timeout status
            self.0
                .lock_status
                .store(LockStatus::AgeTimeout.into(), Ordering::SeqCst);
            return WaitOutcome::AgeTimeout;
        };

        // Naming nothing is the same as no interest: an empty set matches no token,
        // so registering would buy a wakeup nothing could deliver.
        let unusable = unusable.filter(|unusable| !unusable.is_empty());

        let told = match unusable {
            // The ordinary case: nothing named, so nothing to register or select on.
            None => {
                return Self::writer_done(&self.0, duration).await;
            }
            Some(unusable) => match self.0.watch_fill(unusable) {
                // Already true on arrival; no later publication would repeat it.
                Watching::AlreadyPublished(matched) => {
                    // The writer can release between the check above and here, and
                    // releasing does not clear what it published. Let the release
                    // win, as the `biased` select below does: the asset is in cache,
                    // so abandoning would fetch it uncached for nothing.
                    if !self.locked() {
                        return WaitOutcome::Released;
                    }
                    return WaitOutcome::Abandoned(matched);
                }
                Watching::Registered(told) => told,
            },
        };
        let told_to_stop = async {
            match told.await {
                Ok(matched) => WaitOutcome::Abandoned(matched),
                // Unreachable: the registry parts with a sender only by sending to
                // it, or by sweeping one already closed, which this cannot be while
                // this await holds its receiver. So this is a bug in this file
                // rather than a race a caller can provoke.
                //
                // `Released` rather than a panic in release builds: the caller then
                // takes the dangling-lock path, recompetes, and can still cache.
                Err(_) => {
                    debug_assert!(false, "fill waiter sender dropped while registered");
                    WaitOutcome::Released
                }
            }
        };

        // Biased so a writer that finishes at the same moment wins: completing the
        // wait normally is always at least as good as giving up on it.
        tokio::select! {
            biased;
            outcome = Self::writer_done(&self.0, duration) => outcome,
            outcome = told_to_stop => outcome,
        }
    }

    /// Wait out the writer, or the lock's remaining age.
    async fn writer_done(core: &LockCore, duration: Duration) -> WaitOutcome {
        match timeout(duration, core.lock.acquire()).await {
            Ok(Ok(_)) => {
                // permit is returned to Semaphore right away
                WaitOutcome::Released
            }
            Ok(Err(e)) => {
                warn!("error acquiring semaphore {e:?}");
                WaitOutcome::Released
            }
            Err(_) => {
                core.lock_status
                    .store(LockStatus::AgeTimeout.into(), Ordering::SeqCst);
                WaitOutcome::AgeTimeout
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
        self.0.lock_start.elapsed() >= self.0.age_timeout
    }

    /// The current status of the lock
    pub fn lock_status(&self) -> LockStatus {
        let status = self.0.lock_status();
        if matches!(status, LockStatus::Waiting) && self.expired() {
            LockStatus::AgeTimeout
        } else {
            status
        }
    }

    pub fn extensions(&self) -> &Extensions {
        self.0.extensions()
    }
}

/// WritePermit: requires who get it need to populate the cache and then release it
#[derive(Debug)]
pub struct WritePermit {
    lock: Arc<LockCore>,
    finished: bool,
}

impl WritePermit {
    /// Create a new lock, with a permit to be given to the associated writer.
    pub fn new(
        timeout: Duration,
        stale_writer: bool,
        extensions: Extensions,
    ) -> (WritePermit, LockStub) {
        let lock = LockCore::new_arc(timeout, stale_writer, extensions);
        let stub = LockStub(lock.clone());
        (
            WritePermit {
                lock,
                finished: false,
            },
            stub,
        )
    }

    /// Was this lock for a stale cache fetch writer?
    pub fn stale_writer(&self) -> bool {
        self.lock.stale_writer()
    }

    pub fn unlock(&mut self, reason: LockStatus) {
        self.finished = true;
        self.lock.unlock(reason);
    }

    pub fn lock_status(&self) -> LockStatus {
        self.lock.lock_status()
    }

    pub fn extensions(&self) -> &Extensions {
        self.lock.extensions()
    }

    /// Describe what this fill involves, so readers that cannot use it stop
    /// waiting. Tokens are opaque: writer and readers need only agree on meaning.
    ///
    /// Each call **replaces** the previous description, since a writer that moved
    /// on is no longer doing what it said and a reader giving up over an
    /// abandoned attempt would lose its caching for nothing. Empty describes
    /// nothing. Only matching readers are woken.
    pub fn publish(&self, tokens: &[u64]) {
        // Most writers never publish anything, and this is called on every
        // upstream request. Replacing nothing with nothing cannot wake a reader,
        // since an empty set matches no token, so it does not need the lock.
        // Racing readers are unaffected either way: one that registers around
        // this point takes the lock, sees nothing published, and waits.
        if tokens.is_empty() && !self.lock.published_any.load(Ordering::Relaxed) {
            return;
        }
        self.lock.publish(tokens);
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

#[derive(Debug)]
pub struct LockStub(pub Arc<LockCore>);
impl LockStub {
    pub fn read_lock(&self) -> ReadLock {
        ReadLock(self.0.clone())
    }

    pub fn extensions(&self) -> &Extensions {
        &self.0.extensions
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::CacheKey;

    const WRONG_PLACE: u64 = 7;
    const SOMEWHERE_ELSE: u64 = 9;

    /// Reasons are the application's; the cache never interprets them.
    const NO_GOOD: &str = "NoGoodToThisReader";

    /// A reader that cannot use `token`, for the usual single-reason case.
    fn cannot_use(token: u64) -> [UnusableFill; 1] {
        [UnusableFill {
            token,
            reason: NO_GOOD,
        }]
    }

    fn new_lock(age: Duration) -> (WritePermit, LockStub) {
        WritePermit::new(age, false, Extensions::new())
    }

    fn reader(stub: &LockStub) -> ReadLock {
        ReadLock(stub.0.clone())
    }

    /// Wait for readers to register, giving up with a useful message rather than
    /// spinning forever. An unbounded spin turns a registration regression into a
    /// CI job timeout that says nothing about what broke.
    async fn registered(stub: &LockStub, want: usize) {
        for _ in 0..1_000 {
            if stub.0.registered_waiters() >= want {
                return;
            }
            tokio::task::yield_now().await;
        }
        panic!(
            "expected {want} registered waiter(s), found {}",
            stub.0.registered_waiters()
        );
    }

    /// Wraps a future to count how many times it is polled, which is how a test
    /// can tell a reader was never woken rather than merely still waiting.
    struct CountPolls {
        inner: std::pin::Pin<Box<dyn std::future::Future<Output = WaitOutcome> + Send>>,
        polls: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl std::future::Future for CountPolls {
        type Output = WaitOutcome;

        fn poll(
            self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<WaitOutcome> {
            let this = self.get_mut();
            this.polls.fetch_add(1, Ordering::Relaxed);
            this.inner.as_mut().poll(cx)
        }
    }

    /// Only the matching reader is woken -- the other is not even polled. Waking
    /// everyone to self-test would look the same from the outside.
    #[tokio::test]
    async fn only_the_reader_whose_tokens_match_is_woken() {
        let (permit, stub) = new_lock(Duration::from_secs(30));
        let polls = Arc::new(std::sync::atomic::AtomicUsize::new(0));

        let leaves = reader(&stub);
        let left = tokio::spawn(async move {
            leaves
                .wait_unless_published(&UnusableFills::new(cannot_use(WRONG_PLACE)))
                .await
        });
        let stays = reader(&stub);
        let stayed = tokio::spawn(CountPolls {
            inner: Box::pin(async move {
                stays
                    .wait_unless_published(&UnusableFills::new(cannot_use(SOMEWHERE_ELSE)))
                    .await
            }),
            polls: polls.clone(),
        });
        registered(&stub, 2).await;
        let polled_before = polls.load(Ordering::Relaxed);

        permit.publish(&[WRONG_PLACE]);

        assert_eq!(
            left.await.unwrap(),
            WaitOutcome::Abandoned(UnusableFill {
                token: WRONG_PLACE,
                reason: NO_GOOD,
            })
        );
        assert!(!stayed.is_finished(), "the other reader still coalesces");
        assert_eq!(
            polls.load(Ordering::Relaxed),
            polled_before,
            "and was never woken by a publication that does not concern it"
        );
        assert_eq!(
            stub.0.registered_waiters(),
            1,
            "the reader that left is no longer registered"
        );
        assert_eq!(
            stub.0.lock_status(),
            LockStatus::Waiting,
            "giving up does not touch the shared status"
        );

        let mut permit = permit;
        permit.unlock(LockStatus::Done);
        assert_eq!(stayed.await.unwrap(), WaitOutcome::Released);
        assert!(
            polls.load(Ordering::Relaxed) > polled_before,
            "it is woken when the writer actually releases"
        );
    }

    /// A reader that names no tokens never registers, so no amount of publishing
    /// can wake it.
    #[tokio::test]
    async fn a_reader_that_names_no_tokens_is_never_registered() {
        let (permit, stub) = new_lock(Duration::from_secs(30));
        let core = reader(&stub);
        let waiting = tokio::spawn(async move { core.wait().await });
        tokio::task::yield_now().await;

        assert_eq!(stub.0.registered_waiters(), 0);
        permit.publish(&[WRONG_PLACE, SOMEWHERE_ELSE]);
        tokio::task::yield_now().await;
        assert!(!waiting.is_finished());
        assert_eq!(stub.0.registered_waiters(), 0);

        let mut permit = permit;
        permit.unlock(LockStatus::Done);
        waiting.await.unwrap();
    }

    /// What was published before a reader arrived still reaches it. No later
    /// publication is coming to tell it.
    #[tokio::test]
    async fn tokens_published_before_the_reader_arrives_are_not_missed() {
        let (permit, stub) = new_lock(Duration::from_secs(30));
        permit.publish(&[WRONG_PLACE]); // nobody is waiting yet

        let outcome = reader(&stub)
            .wait_unless_published(&UnusableFills::new(cannot_use(WRONG_PLACE)))
            .await;

        assert_eq!(
            outcome,
            WaitOutcome::Abandoned(UnusableFill {
                token: WRONG_PLACE,
                reason: NO_GOOD,
            })
        );
        assert_eq!(stub.0.registered_waiters(), 0, "and it did not register");

        let mut permit = permit;
        permit.unlock(LockStatus::Done);
    }

    /// Publishing replaces, so a reader is not stopped over an attempt the writer
    /// has already moved on from, which would cost its caching for nothing.
    #[tokio::test]
    async fn a_reader_is_not_stopped_by_a_superseded_publication() {
        let (permit, stub) = new_lock(Duration::from_secs(30));
        permit.publish(&[WRONG_PLACE]);
        permit.publish(&[SOMEWHERE_ELSE]); // the first attempt is done with

        let stays = reader(&stub);
        let stayed = tokio::spawn(async move {
            stays
                .wait_unless_published(&UnusableFills::new(cannot_use(WRONG_PLACE)))
                .await
        });
        registered(&stub, 1).await;
        assert!(!stayed.is_finished(), "the old attempt no longer matters");

        // And a reader already waiting is stopped if the writer moves back.
        permit.publish(&[WRONG_PLACE]);
        assert_eq!(
            stayed.await.unwrap(),
            WaitOutcome::Abandoned(UnusableFill {
                token: WRONG_PLACE,
                reason: NO_GOOD,
            })
        );

        let mut permit = permit;
        permit.unlock(LockStatus::Done);
    }

    /// Cancelling the wait leaves nothing for a publication to match. Cancellation,
    /// not an ordinary return -- what `HttpCache::cache_lock_wait` does whenever its
    /// own `wait_timeout` fires.
    ///
    /// The entry itself outlives the cancellation, since nobody deregisters, so the
    /// reclaiming is asserted through a publication that does *not* name this
    /// reader's token: matching would remove the entry too, and then this would pass
    /// without the closed-sender sweep it is here to cover.
    #[tokio::test]
    async fn cancelling_the_wait_leaves_nothing_to_match() {
        let (mut permit, stub) = new_lock(Duration::from_secs(30));

        let waits = reader(&stub);
        let interest = UnusableFills::new(cannot_use(WRONG_PLACE));
        // Boxed rather than `tokio::pin!`, which shadows the future with a
        // `Pin<&mut _>`; dropping that drops a reference and the future would
        // outlive the assertion below, passing for the wrong reason.
        let mut wait = Box::pin(waits.wait_unless_published(&interest));

        assert!(
            futures::poll!(wait.as_mut()).is_pending(),
            "the writer still holds the lock"
        );
        assert_eq!(stub.0.registered_waiters(), 1);
        assert_eq!(stub.0.retained_waiters(), 1);

        drop(wait);
        assert_eq!(
            stub.0.registered_waiters(),
            0,
            "a cancelled reader must not be left for the writer to test"
        );
        assert_eq!(
            stub.0.retained_waiters(),
            1,
            "though the entry is still there until something sweeps it"
        );

        permit.publish(&[SOMEWHERE_ELSE]);
        assert_eq!(
            stub.0.retained_waiters(),
            0,
            "and a publication sweeps it even without matching it"
        );

        permit.unlock(LockStatus::Done);
    }

    /// Nobody deregisters, so later registrations sweep what earlier readers left.
    /// Without that a long fill accumulates an entry per reader that ever waited.
    #[tokio::test]
    async fn departed_readers_are_swept_by_later_registrations() {
        let (mut permit, stub) = new_lock(Duration::from_secs(30));
        let interest = UnusableFills::new(cannot_use(WRONG_PLACE));

        for _ in 0..200 {
            let waits = reader(&stub);
            let mut wait = Box::pin(waits.wait_unless_published(&interest));
            assert!(
                futures::poll!(wait.as_mut()).is_pending(),
                "the writer still holds the lock"
            );
            drop(wait);
        }

        assert_eq!(
            stub.0.registered_waiters(),
            0,
            "every one of those readers has gone"
        );
        assert!(
            stub.0.retained_waiters() <= WAITER_SWEEP_BUDGET,
            "swept back down, found {}",
            stub.0.retained_waiters()
        );

        permit.unlock(LockStatus::Done);
    }

    /// A sweep is bounded, so the cursor is what stops departed readers hiding
    /// behind live ones: restarting at the front would examine a window of live
    /// readers and reclaim nothing, forever.
    #[tokio::test]
    async fn departed_readers_behind_live_ones_are_still_reclaimed() {
        let (mut permit, stub) = new_lock(Duration::from_secs(30));
        let interest = UnusableFills::new(cannot_use(WRONG_PLACE));

        // One `ReadLock` shared by every wait below, so the futures that stay
        // registered can outlive the loop that made them. Each call registers
        // separately regardless of which handle it came from.
        let waits = reader(&stub);

        // Enough to fill a sweep window and then some, all of them staying.
        let mut live = Vec::new();
        for _ in 0..(WAITER_SWEEP_BUDGET * 2) {
            let mut wait = Box::pin(waits.wait_unless_published(&interest));
            assert!(futures::poll!(wait.as_mut()).is_pending());
            live.push(wait);
        }
        assert_eq!(stub.0.registered_waiters(), live.len());

        // Then readers that come and go behind them.
        let departed = WAITER_SWEEP_BUDGET * 4;
        for _ in 0..departed {
            let mut wait = Box::pin(waits.wait_unless_published(&interest));
            assert!(futures::poll!(wait.as_mut()).is_pending());
            drop(wait);
        }

        assert_eq!(
            stub.0.registered_waiters(),
            live.len(),
            "the readers that stayed are all still registered"
        );
        assert!(
            stub.0.retained_waiters() <= live.len() + WAITER_SWEEP_BUDGET,
            "departed readers must be reclaimed despite never being at the front, \
             leaving an overhead set by the sweep budget rather than by how many \
             readers have come and gone: found {} entries for {} live readers",
            stub.0.retained_waiters(),
            live.len()
        );

        drop(live);
        permit.unlock(LockStatus::Done);
    }

    /// The same through an outer timeout, which is the shape
    /// `HttpCache::cache_lock_wait` actually uses: a `wait_timeout` shorter than
    /// the lock age, cutting the wait short while it is still registered.
    #[tokio::test]
    async fn a_wait_cut_short_by_an_outer_timeout_leaves_nothing_to_match() {
        let (mut permit, stub) = new_lock(Duration::from_secs(30));

        let waits = reader(&stub);
        let cut_short = tokio::time::timeout(
            Duration::from_millis(10),
            waits.wait_unless_published(&UnusableFills::new(cannot_use(WRONG_PLACE))),
        )
        .await;

        assert!(cut_short.is_err(), "the writer never released");
        assert_eq!(
            stub.0.registered_waiters(),
            0,
            "nothing is left for a publication to match"
        );
        assert_eq!(
            stub.0.retained_waiters(),
            1,
            "the entry outlives the wait, to be swept later"
        );

        permit.unlock(LockStatus::Done);
    }

    /// Naming nothing cannot be matched, so it should cost no registration.
    /// Asserted on the registry, since never-registering and never-matching look the
    /// same from outside.
    #[tokio::test]
    async fn an_empty_interest_registers_no_waiter() {
        let (mut permit, stub) = new_lock(Duration::from_secs(30));

        let waits = reader(&stub);
        let waited =
            tokio::spawn(async move { waits.wait_unless_published(&UnusableFills::new([])).await });

        // A second reader that does name something gives a deterministic point to
        // wait for. Once it has registered the runtime has run both, so an absent
        // registration is absence rather than lateness -- which a fixed number of
        // yields cannot distinguish.
        let names_something = reader(&stub);
        let named = tokio::spawn(async move {
            names_something
                .wait_unless_published(&UnusableFills::new(cannot_use(WRONG_PLACE)))
                .await
        });
        registered(&stub, 1).await;

        assert_eq!(
            stub.0.registered_waiters(),
            1,
            "an interest that names nothing has nothing to be told about"
        );
        drop(named);

        // Still an ordinary wait: released by the writer, like a reader with no
        // interest at all.
        permit.unlock(LockStatus::Done);
        assert_eq!(waited.await.unwrap(), WaitOutcome::Released);
    }

    /// Publishing nothing clears what came before, so a later reader waits rather
    /// than giving up over a fill that is no longer happening. Skipping empty
    /// publications outright would strand the stale token.
    #[tokio::test]
    async fn publishing_nothing_clears_a_previous_publication() {
        let (permit, stub) = new_lock(Duration::from_secs(30));
        permit.publish(&[WRONG_PLACE]);
        permit.publish(&[]); // retried onto the origin; no longer in any cycle

        // Asserted directly rather than by spinning on a registration that a
        // stranded token would prevent, so this fails instead of hanging.
        assert_eq!(
            first_match(&stub.0.fill.lock().published, &cannot_use(WRONG_PLACE)),
            None,
            "the abandoned attempt no longer matters"
        );

        let stays = reader(&stub);
        let stayed = tokio::spawn(async move {
            stays
                .wait_unless_published(&UnusableFills::new(cannot_use(WRONG_PLACE)))
                .await
        });

        let mut permit = permit;
        permit.unlock(LockStatus::Done);
        assert_eq!(stayed.await.unwrap(), WaitOutcome::Released);
    }

    /// A reader whose tokens are never published waits for the writer as usual,
    /// and leaves no registration behind for the writer to keep testing.
    #[tokio::test]
    async fn a_reader_that_times_out_reports_the_age_timeout() {
        let (permit, stub) = new_lock(Duration::from_millis(50));

        let outcome = reader(&stub)
            .wait_unless_published(&UnusableFills::new(cannot_use(WRONG_PLACE)))
            .await;

        assert_eq!(outcome, WaitOutcome::AgeTimeout);

        let mut permit = permit;
        permit.unlock(LockStatus::Done);
    }

    #[test]
    fn test_get_release() {
        let cache_lock = CacheLock::new_boxed(Duration::from_secs(1000));
        let key1 = CacheKey::new("a", "1");
        let locked1 = cache_lock.lock(&key1, false);
        assert!(locked1.is_write()); // write permit
        let locked2 = cache_lock.lock(&key1, false);
        assert!(!locked2.is_write()); // read lock
        if let Locked::Write(permit) = locked1 {
            cache_lock.release(&key1, permit, LockStatus::Done);
        }
        let locked3 = cache_lock.lock(&key1, false);
        assert!(locked3.is_write()); // write permit again
        if let Locked::Write(permit) = locked3 {
            cache_lock.release(&key1, permit, LockStatus::Done);
        }
    }

    #[tokio::test]
    async fn test_lock() {
        let cache_lock = CacheLock::new_boxed(Duration::from_secs(1000));
        let key1 = CacheKey::new("a", "1");
        let mut permit = match cache_lock.lock(&key1, false) {
            Locked::Write(w) => w,
            _ => panic!(),
        };
        let lock = match cache_lock.lock(&key1, false) {
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
        let key1 = CacheKey::new("a", "1");
        let mut permit = match cache_lock.lock(&key1, false) {
            Locked::Write(w) => w,
            _ => panic!(),
        };
        let lock = match cache_lock.lock(&key1, false) {
            Locked::Read(r) => r,
            _ => panic!(),
        };
        assert!(lock.locked());

        let handle = tokio::spawn(async move {
            // timed out
            lock.wait().await;
            assert_eq!(lock.lock_status(), LockStatus::AgeTimeout);
        });

        tokio::time::sleep(Duration::from_millis(2100)).await;

        handle.await.unwrap(); // check lock is timed out

        // expired lock - we will be able to install a new lock instead
        let mut permit2 = match cache_lock.lock(&key1, false) {
            Locked::Write(w) => w,
            _ => panic!(),
        };
        let lock2 = match cache_lock.lock(&key1, false) {
            Locked::Read(r) => r,
            _ => panic!(),
        };
        assert!(lock2.locked());
        let handle = tokio::spawn(async move {
            // timed out
            lock2.wait().await;
            assert_eq!(lock2.lock_status(), LockStatus::Done);
        });

        permit.unlock(LockStatus::Done);
        permit2.unlock(LockStatus::Done);
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn test_lock_expired_release() {
        let cache_lock = CacheLock::new_boxed(Duration::from_secs(1));
        let key1 = CacheKey::new("a", "1");
        let permit = match cache_lock.lock(&key1, false) {
            Locked::Write(w) => w,
            _ => panic!(),
        };

        let lock = match cache_lock.lock(&key1, false) {
            Locked::Read(r) => r,
            _ => panic!(),
        };
        assert!(lock.locked());
        let handle = tokio::spawn(async move {
            // timed out
            lock.wait().await;
            assert_eq!(lock.lock_status(), LockStatus::AgeTimeout);
        });

        tokio::time::sleep(Duration::from_millis(1100)).await; // let lock age time out
        handle.await.unwrap(); // check lock is timed out

        // writer finally finishes
        cache_lock.release(&key1, permit, LockStatus::Done);

        // can reacquire after release
        let mut permit = match cache_lock.lock(&key1, false) {
            Locked::Write(w) => w,
            _ => panic!(),
        };
        assert_eq!(permit.lock.lock_status(), LockStatus::Waiting);

        let lock2 = match cache_lock.lock(&key1, false) {
            Locked::Read(r) => r,
            _ => panic!(),
        };
        assert!(lock2.locked());
        let handle = tokio::spawn(async move {
            // timed out
            lock2.wait().await;
            assert_eq!(lock2.lock_status(), LockStatus::Done);
        });

        permit.unlock(LockStatus::Done);
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn test_lock_expired_no_reader() {
        let cache_lock = CacheLock::new_boxed(Duration::from_secs(1));
        let key1 = CacheKey::new("a", "1");
        let mut permit = match cache_lock.lock(&key1, false) {
            Locked::Write(w) => w,
            _ => panic!(),
        };
        tokio::time::sleep(Duration::from_millis(1100)).await; // let lock age time out

        // lock expired without reader, but status is not yet set
        assert_eq!(permit.lock.lock_status(), LockStatus::Waiting);

        let lock = match cache_lock.lock(&key1, false) {
            Locked::Read(r) => r,
            _ => panic!(),
        };
        // reader expires write permit
        lock.wait().await;
        assert_eq!(lock.lock_status(), LockStatus::AgeTimeout);
        assert_eq!(permit.lock.lock_status(), LockStatus::AgeTimeout);
        permit.unlock(LockStatus::AgeTimeout);
    }

    #[tokio::test]
    async fn test_lock_concurrent() {
        let _ = env_logger::builder().is_test(true).try_init();
        // Test that concurrent attempts to compete for a lock run without issues
        let cache_lock = Arc::new(CacheLock::new_boxed(Duration::from_secs(1)));
        let key1 = CacheKey::new("a", "1");

        let mut handles = vec![];

        const READERS: usize = 30;
        for _ in 0..READERS {
            let key1 = key1.clone();
            let cache_lock = cache_lock.clone();
            // simulate a cache lookup / lock attempt loop
            handles.push(tokio::spawn(async move {
                // timed out
                loop {
                    match cache_lock.lock(&key1, false) {
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

    /// An expired lock is replaced rather than abandoned, even with an unusable
    /// token already published -- both are true at once for the case this feature
    /// exists for, since a stuck owner holds its lock for the full age. Recompeting
    /// can still cache, where abandoning could not.
    #[tokio::test]
    async fn an_expired_lock_times_out_rather_than_abandoning() {
        let (mut permit, stub) = new_lock(Duration::from_millis(10));
        stub.0.publish(&[WRONG_PLACE]);
        tokio::time::sleep(Duration::from_millis(30)).await;

        let outcome = reader(&stub)
            .wait_unless_published(&UnusableFills::new(cannot_use(WRONG_PLACE)))
            .await;

        assert_eq!(
            outcome,
            WaitOutcome::AgeTimeout,
            "expiry is decided before any published token is consulted"
        );
        assert_eq!(
            stub.0.lock_status(),
            LockStatus::AgeTimeout,
            "the dead lock must not be left reading Waiting"
        );

        permit.unlock(LockStatus::Done);
    }
}
