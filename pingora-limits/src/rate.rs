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

//! The rate module defines the [Rate] type that helps estimate the occurrence of events over a
//! period of time.

use crate::estimator::Estimator;
use std::hash::Hash;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;

/// A stable rate estimator that reports the rate of events in the past `interval` time.
/// It returns the average rate between `interval` * 2 and `interval` while collecting the events
/// happening between `interval` and now.
///
/// This estimator ignores events that happen less than once per `interval` time.
pub struct Rate {
    // 2 slots so that we use one to collect the current events and the other to report rate
    red_slot: Estimator,
    blue_slot: Estimator,
    red_or_blue: AtomicBool, // true: the current slot is red, otherwise blue
    start: Instant,
    // Use u64 below instead of Instant because we want atomic operation
    reset_interval_ms: u64, // the time interval to reset `current` and move it to `previous`
    last_reset_time: AtomicU64, // the timestamp in ms since `start`
}

// see inflight module for the meaning for these numbers
const HASHES: usize = 4;
const SLOTS: usize = 1024; // This value can be lower if interval is short (key cardinality is low)

impl Rate {
    /// Create a new `Rate` with the given interval.
    pub fn new(interval: std::time::Duration) -> Self {
        Rate {
            red_slot: Estimator::new(HASHES, SLOTS),
            blue_slot: Estimator::new(HASHES, SLOTS),
            red_or_blue: AtomicBool::new(true),
            start: Instant::now(),
            reset_interval_ms: interval.as_millis() as u64, // should be small not to overflow
            last_reset_time: AtomicU64::new(0),
        }
    }

    fn current(&self, red_or_blue: bool) -> &Estimator {
        if red_or_blue {
            &self.red_slot
        } else {
            &self.blue_slot
        }
    }

    fn previous(&self, red_or_blue: bool) -> &Estimator {
        if red_or_blue {
            &self.blue_slot
        } else {
            &self.red_slot
        }
    }

    fn red_or_blue(&self) -> bool {
        self.red_or_blue.load(Ordering::SeqCst)
    }

    /// Return the per second rate estimation.
    pub fn rate<T: Hash>(&self, key: &T) -> f64 {
        let past_ms = self.maybe_reset();
        if past_ms >= self.reset_interval_ms * 2 {
            // already missed 2 intervals, no data, just report 0 as a short cut
            return 0f64;
        }

        self.previous(self.red_or_blue()).get(key) as f64 / self.reset_interval_ms as f64 * 1000.0
    }

    /// Report new events and return number of events seen so far in the current interval.
    pub fn observe<T: Hash>(&self, key: &T, events: isize) -> isize {
        self.maybe_reset();
        self.current(self.red_or_blue()).incr(key, events)
    }

    // reset if needed, return the time since last reset for other fn to use
    fn maybe_reset(&self) -> u64 {
        // should be short enough not to overflow
        let now = Instant::now().duration_since(self.start).as_millis() as u64;
        let last_reset = self.last_reset_time.load(Ordering::SeqCst);
        let past_ms = now - last_reset;

        if past_ms < self.reset_interval_ms {
            // no need to reset
            return past_ms;
        }
        let red_or_blue = self.red_or_blue();
        match self.last_reset_time.compare_exchange(
            last_reset,
            now,
            Ordering::SeqCst,
            Ordering::Acquire,
        ) {
            Ok(_) => {
                // first clear the previous slot
                self.previous(red_or_blue).reset();
                // then flip the flag to tell others to use the reset slot
                self.red_or_blue.store(!red_or_blue, Ordering::SeqCst);
                // if current time is beyond 2 intervals, the data stored in the previous slot
                // is also stale, we should clear that too
                if now - last_reset >= self.reset_interval_ms * 2 {
                    // Note that this is the previous one now because we just flipped self.red_or_blue
                    self.current(red_or_blue).reset();
                }
            }
            Err(new) => {
                // another thread beats us to it
                assert!(new >= now - 1000); // double check that the new timestamp looks right
            }
        }

        past_ms
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread::sleep;
    use std::time::Duration;

    #[test]
    fn test_observe_rate() {
        let r = Rate::new(Duration::from_secs(1));
        let key = 1;

        // second: 0
        let observed = r.observe(&key, 3);
        assert_eq!(observed, 3);
        let observed = r.observe(&key, 2);
        assert_eq!(observed, 5);
        assert_eq!(r.rate(&key), 0f64); // no estimation yet because the interval has not passed

        // second: 1
        sleep(Duration::from_secs(1));
        let observed = r.observe(&key, 4);
        assert_eq!(observed, 4);
        assert_eq!(r.rate(&key), 5f64); // 5 rps

        // second: 2
        sleep(Duration::from_secs(1));
        assert_eq!(r.rate(&key), 4f64);

        // second: 3
        sleep(Duration::from_secs(1));
        assert_eq!(r.rate(&key), 0f64); // no event observed in the past 2 seconds
    }
}
