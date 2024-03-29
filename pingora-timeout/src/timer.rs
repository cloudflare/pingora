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

//! Lightweight timer for systems with high rate of operations with timeout
//! associated with them
//!
//! Users don't need to interact with this module.
//!
//! The idea is to bucket timers into finite time slots so that operations that
//! start and end quickly don't have to create their own timers all the time
//!
//! Benchmark:
//! - create 7.809622ms total, 78ns avg per iteration
//! - drop: 1.348552ms total, 13ns avg per iteration
//!
//! tokio timer:
//! - create 34.317439ms total, 343ns avg per iteration
//! - drop: 10.694154ms total, 106ns avg per iteration

use parking_lot::RwLock;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use thread_local::ThreadLocal;
use tokio::sync::Notify;

const RESOLUTION_MS: u64 = 10;
const RESOLUTION_DURATION: Duration = Duration::from_millis(RESOLUTION_MS);

// round to the NEXT timestamp based on the resolution
#[inline]
fn round_to(raw: u128, resolution: u128) -> u128 {
    raw - 1 + resolution - (raw - 1) % resolution
}
// millisecond resolution as most
#[derive(PartialEq, PartialOrd, Eq, Ord, Clone, Copy, Debug)]
struct Time(u128);

impl From<u128> for Time {
    fn from(raw_ms: u128) -> Self {
        Time(round_to(raw_ms, RESOLUTION_MS as u128))
    }
}

impl From<Duration> for Time {
    fn from(d: Duration) -> Self {
        Time(round_to(d.as_millis(), RESOLUTION_MS as u128))
    }
}

impl Time {
    pub fn not_after(&self, ts: u128) -> bool {
        self.0 <= ts
    }
}

/// the stub for waiting for a timer to be expired.
pub struct TimerStub(Arc<Notify>, Arc<AtomicBool>);

impl TimerStub {
    /// Wait for the timer to expire.
    pub async fn poll(self) {
        if self.1.load(Ordering::SeqCst) {
            return;
        }
        self.0.notified().await;
    }
}

struct Timer(Arc<Notify>, Arc<AtomicBool>);

impl Timer {
    pub fn new() -> Self {
        Timer(Arc::new(Notify::new()), Arc::new(AtomicBool::new(false)))
    }

    pub fn fire(&self) {
        self.1.store(true, Ordering::SeqCst);
        self.0.notify_waiters();
    }

    pub fn subscribe(&self) -> TimerStub {
        TimerStub(self.0.clone(), self.1.clone())
    }
}

/// The object that holds all the timers registered to it.
pub struct TimerManager {
    // each thread insert into its local timer tree to avoid lock contention
    timers: ThreadLocal<RwLock<BTreeMap<Time, Timer>>>,
    zero: Instant, // the reference zero point of Timestamp
    // Start a new clock thread if this is -1 or staled. The clock thread should keep updating this
    clock_watchdog: AtomicI64,
    paused: AtomicBool,
}

// Consider the clock thread is dead after it fails to update the thread in DELAYS_SEC
const DELAYS_SEC: i64 = 2; // TODO: make sure this value is larger than RESOLUTION_DURATION

impl Default for TimerManager {
    fn default() -> Self {
        TimerManager {
            timers: ThreadLocal::new(),
            zero: Instant::now(),
            clock_watchdog: AtomicI64::new(-DELAYS_SEC),
            paused: AtomicBool::new(false),
        }
    }
}

impl TimerManager {
    /// Create a new [TimerManager]
    pub fn new() -> Self {
        Self::default()
    }

    // This thread sleeps for a resolution time and then fires all the timers that are due to fire
    pub(crate) fn clock_thread(&self) {
        loop {
            std::thread::sleep(RESOLUTION_DURATION);
            let now = Instant::now() - self.zero;
            self.clock_watchdog
                .store(now.as_secs() as i64, Ordering::Relaxed);
            if self.is_paused_for_fork() {
                // just stop acquiring the locks, waiting for fork to happen
                continue;
            }
            let now = now.as_millis();
            // iterate through the timer tree for all threads
            for thread_timer in self.timers.iter() {
                let mut timers = thread_timer.write();
                // Fire all timers until now
                loop {
                    let key_to_remove = timers.iter().next().and_then(|(k, _)| {
                        if k.not_after(now) {
                            Some(*k)
                        } else {
                            None
                        }
                    });
                    if let Some(k) = key_to_remove {
                        let timer = timers.remove(&k);
                        // safe to unwrap, the key is from iter().next()
                        timer.unwrap().fire();
                    } else {
                        break;
                    }
                }
                // write lock drops here
            }
        }
    }

    // False if the clock is already started
    // If true, the caller must start the clock thread next
    pub(crate) fn should_i_start_clock(&self) -> bool {
        let Err(prev) = self.is_clock_running() else {
            return false;
        };
        let now = Instant::now().duration_since(self.zero).as_secs() as i64;
        let res =
            self.clock_watchdog
                .compare_exchange(prev, now, Ordering::SeqCst, Ordering::SeqCst);
        res.is_ok()
    }

    // Ok(()) if clock is running (watch dog is within DELAYS_SEC of now)
    // Err(time) if watch do stopped at `time`
    pub(crate) fn is_clock_running(&self) -> Result<(), i64> {
        let now = Instant::now().duration_since(self.zero).as_secs() as i64;
        let prev = self.clock_watchdog.load(Ordering::SeqCst);
        if now < prev + DELAYS_SEC {
            Ok(())
        } else {
            Err(prev)
        }
    }

    /// Register a timer.
    ///
    /// When the timer expires, the [TimerStub] will be notified.
    pub fn register_timer(&self, duration: Duration) -> TimerStub {
        if self.is_paused_for_fork() {
            // Return a dummy TimerStub that will trigger right away.
            // This is fine assuming pause_for_fork() is called right before fork().
            // The only possible register_timer() is from another thread which will
            // be entirely lost after fork()
            // TODO: buffer these register calls instead (without a lock)
            let timer = Timer::new();
            timer.fire();
            return timer.subscribe();
        }
        let now: Time = (Instant::now() + duration - self.zero).into();
        {
            let timers = self.timers.get_or(|| RwLock::new(BTreeMap::new())).read();
            if let Some(t) = timers.get(&now) {
                return t.subscribe();
            }
        } // drop read lock

        let timer = Timer::new();
        let mut timers = self.timers.get_or(|| RwLock::new(BTreeMap::new())).write();
        // Usually we check if another thread has insert the same node before we get the write lock,
        // but because only this thread will insert anything to its local timers tree, there
        // is no possible race that can happen. The only other thread is the clock thread who
        // only removes timer from the tree
        let stub = timer.subscribe();
        timers.insert(now, timer);
        stub
    }

    fn is_paused_for_fork(&self) -> bool {
        self.paused.load(Ordering::SeqCst)
    }

    /// Pause the timer for fork()
    ///
    /// Because RwLock across fork() is undefined behavior, this function makes sure that no one
    /// holds any locks.
    ///
    /// This function should be called right before fork().
    pub fn pause_for_fork(&self) {
        self.paused.store(true, Ordering::SeqCst);
        // wait for everything to get out of their locks
        std::thread::sleep(RESOLUTION_DURATION * 2);
    }

    /// Unpause the timer after fork()
    ///
    /// This function should be called right after fork().
    pub fn unpause(&self) {
        self.paused.store(false, Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_round() {
        assert_eq!(round_to(30, 10), 30);
        assert_eq!(round_to(31, 10), 40);
        assert_eq!(round_to(29, 10), 30);
    }

    #[test]
    fn test_time() {
        let t: Time = 128.into(); // t will round to 130
        assert_eq!(t, Duration::from_millis(130).into());
        assert!(!t.not_after(128));
        assert!(!t.not_after(129));
        assert!(t.not_after(130));
        assert!(t.not_after(131));
    }

    #[tokio::test]
    async fn test_timer_manager() {
        let tm_a = Arc::new(TimerManager::new());
        let tm = tm_a.clone();
        std::thread::spawn(move || tm_a.clock_thread());

        let now = Instant::now();
        let t1 = tm.register_timer(Duration::from_secs(1));
        let t2 = tm.register_timer(Duration::from_secs(1));
        t1.poll().await;
        assert_eq!(now.elapsed().as_secs(), 1);
        let now = Instant::now();
        t2.poll().await;
        // t2 fired along t1 so no extra wait time
        assert_eq!(now.elapsed().as_secs(), 0);
    }

    #[test]
    fn test_timer_manager_start_check() {
        let tm = Arc::new(TimerManager::new());
        assert!(tm.should_i_start_clock());
        assert!(!tm.should_i_start_clock());
        assert!(tm.is_clock_running().is_ok());
    }

    #[test]
    fn test_timer_manager_watchdog() {
        let tm = Arc::new(TimerManager::new());
        assert!(tm.should_i_start_clock());
        assert!(!tm.should_i_start_clock());

        // we don't actually start the clock thread, sleep for the watchdog to expire
        std::thread::sleep(Duration::from_secs(DELAYS_SEC as u64 + 1));
        assert!(tm.is_clock_running().is_err());
        assert!(tm.should_i_start_clock());
    }

    #[tokio::test]
    async fn test_timer_manager_pause() {
        let tm_a = Arc::new(TimerManager::new());
        let tm = tm_a.clone();
        std::thread::spawn(move || tm_a.clock_thread());

        let now = Instant::now();
        let t1 = tm.register_timer(Duration::from_secs(2));
        tm.pause_for_fork();
        // no actual fork happen, we just test that pause and unpause work

        // any timer in this critical section is timed out right away
        let t2 = tm.register_timer(Duration::from_secs(2));
        t2.poll().await;
        assert_eq!(now.elapsed().as_secs(), 0);

        std::thread::sleep(Duration::from_secs(1));
        tm.unpause();
        t1.poll().await;
        assert_eq!(now.elapsed().as_secs(), 2);
    }
}
