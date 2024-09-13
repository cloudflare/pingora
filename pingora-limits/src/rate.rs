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
use std::time::{Duration, Instant};

/// Input struct to custom functions for calculating rate. Includes the counts
/// from the current interval, previous interval, the configured duration of an
/// interval, and the fraction into the current interval that the sample was
/// taken.
///
/// Ex. If the interval to the Rate instance is `10s`, and the rate calculation
/// is taken at 2 seconds after the start of the current interval, then the
/// fraction of the current interval returned in this struct will be `0.2`
/// meaning 20% of the current interval has elapsed
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
pub struct RateComponents {
    pub prev_samples: isize,
    pub curr_samples: isize,
    pub interval: Duration,
    pub current_interval_fraction: f64,
}

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
    interval: Duration,
}

// see inflight module for the meaning for these numbers
const HASHES: usize = 4;
const SLOTS: usize = 1024; // This value can be lower if interval is short (key cardinality is low)

impl Rate {
    /// Create a new `Rate` with the given interval.
    pub fn new(interval: std::time::Duration) -> Self {
        Rate::new_with_estimator_config(interval, HASHES, SLOTS)
    }

    /// Create a new `Rate` with the given interval and Estimator config with the given amount of hashes and columns (slots).
    #[inline]
    pub fn new_with_estimator_config(
        interval: std::time::Duration,
        hashes: usize,
        slots: usize,
    ) -> Self {
        Rate {
            red_slot: Estimator::new(hashes, slots),
            blue_slot: Estimator::new(hashes, slots),
            red_or_blue: AtomicBool::new(true),
            start: Instant::now(),
            reset_interval_ms: interval.as_millis() as u64, // should be small not to overflow
            last_reset_time: AtomicU64::new(0),
            interval,
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

    /// Get the current rate as calculated with the given closure. This closure
    /// will take an argument containing all the accessible information about
    /// the rate from this object and allow the caller to make their own
    /// estimation of rate based on:
    ///
    /// 1. The accumulated samples in the current interval (in progress)
    /// 2. The accumulated samples in the previous interval (completed)
    /// 3. The size of the interval
    /// 4. Elapsed fraction of current interval for this sample (0..1)
    ///
    pub fn rate_with<F, T, K>(&self, key: &K, mut rate_calc_fn: F) -> T
    where
        F: FnMut(RateComponents) -> T,
        K: Hash,
    {
        let past_ms = self.maybe_reset();

        let (prev_samples, curr_samples) = if past_ms >= self.reset_interval_ms * 2 {
            // already missed 2 intervals, no data, just report 0 as a short cut
            (0, 0)
        } else if past_ms >= self.reset_interval_ms {
            (self.previous(self.red_or_blue()).get(key), 0)
        } else {
            let (prev_est, curr_est) = if self.red_or_blue() {
                (&self.blue_slot, &self.red_slot)
            } else {
                (&self.red_slot, &self.blue_slot)
            };

            (prev_est.get(key), curr_est.get(key))
        };

        rate_calc_fn(RateComponents {
            interval: self.interval,
            prev_samples,
            curr_samples,
            current_interval_fraction: (past_ms % self.reset_interval_ms) as f64
                / self.reset_interval_ms as f64,
        })
    }
}

#[cfg(test)]
mod tests {
    use float_cmp::assert_approx_eq;

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

    /// Assertion that 2 numbers are close within a generous margin. These
    /// tests are doing a lot of literal sleeping, so the measured results
    /// can't be accurate or consistent. This function does an assert with a
    /// generous tolerance
    fn assert_eq_ish(left: f64, right: f64) {
        assert_approx_eq!(f64, left, right, epsilon = 0.15)
    }

    #[test]
    fn test_observe_rate_custom_90_10() {
        let r = Rate::new(Duration::from_secs(1));
        let key = 1;

        let rate_90_10_fn = |rate_info: RateComponents| {
            let prev = rate_info.prev_samples as f64;
            let curr = rate_info.curr_samples as f64;
            (prev * 0.1 + curr * 0.9) / rate_info.interval.as_secs_f64()
        };

        // second: 0
        let observed = r.observe(&key, 3);
        assert_eq!(observed, 3);
        let observed = r.observe(&key, 2);
        assert_eq!(observed, 5);
        assert_eq!(r.rate_with(&key, rate_90_10_fn), 5. * 0.9);

        // second: 1
        sleep(Duration::from_secs(1));
        let observed = r.observe(&key, 4);
        assert_eq!(observed, 4);
        assert_eq!(r.rate_with(&key, rate_90_10_fn), 5. * 0.1 + 4. * 0.9);

        // second: 2
        sleep(Duration::from_secs(1));
        assert_eq!(r.rate_with(&key, rate_90_10_fn), 4. * 0.1);

        // second: 3
        sleep(Duration::from_secs(1));
        assert_eq!(r.rate_with(&key, rate_90_10_fn), 0f64);
    }

    // this is the function described in this post
    // https://blog.cloudflare.com/counting-things-a-lot-of-different-things/
    #[test]
    fn test_observe_rate_custom_proportional() {
        let r = Rate::new(Duration::from_secs(1));
        let key = 1;

        let rate_prop_fn = |rate_info: RateComponents| {
            let prev = rate_info.prev_samples as f64;
            let curr = rate_info.curr_samples as f64;
            let interval_secs = rate_info.interval.as_secs_f64();
            let interval_fraction = rate_info.current_interval_fraction;

            let weighted_count = prev * (1. - interval_fraction) + curr * interval_fraction;
            weighted_count / interval_secs
        };

        // second: 0
        let observed = r.observe(&key, 3);
        assert_eq!(observed, 3);
        let observed = r.observe(&key, 2);
        assert_eq!(observed, 5);
        assert_eq_ish(r.rate_with(&key, rate_prop_fn), 0.);

        // second 0.5
        sleep(Duration::from_secs_f64(0.5));
        assert_eq_ish(r.rate_with(&key, rate_prop_fn), 5. * 0.5);

        // second: 1
        sleep(Duration::from_secs_f64(0.5));
        let observed = r.observe(&key, 4);
        assert_eq!(observed, 4);
        assert_eq_ish(r.rate_with(&key, rate_prop_fn), 5.);

        // second 1.75
        sleep(Duration::from_secs_f64(0.75));
        assert_eq_ish(r.rate_with(&key, rate_prop_fn), 5. * 0.25 + 4. * 0.75);

        // second: 2
        sleep(Duration::from_secs_f64(0.25));
        assert_eq_ish(r.rate_with(&key, rate_prop_fn), 4.);

        // second: 3
        sleep(Duration::from_secs(1));
        assert_eq!(r.rate_with(&key, rate_prop_fn), 0f64);
    }
}
