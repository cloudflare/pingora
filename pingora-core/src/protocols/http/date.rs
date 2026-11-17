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

use http::header::HeaderValue;
use httpdate::fmt_http_date;
use std::cell::RefCell;
use std::time::{Duration, SystemTime};

fn to_date_string(epoch_sec: u64) -> String {
    // HTTP dates have four-digit years. Clamp to the last representable second
    // so an out-of-range system clock cannot make httpdate panic.
    let epoch_sec = epoch_sec.min(253_402_300_799);
    fmt_http_date(SystemTime::UNIX_EPOCH + Duration::from_secs(epoch_sec))
}

struct CacheableDate {
    h1_date: HeaderValue,
    epoch: Duration,
}

impl CacheableDate {
    pub fn new() -> Self {
        let d = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap();
        CacheableDate {
            h1_date: HeaderValue::from_str(&to_date_string(d.as_secs())).unwrap(),
            epoch: d,
        }
    }

    pub fn update(&mut self, d_now: Duration) {
        if d_now.as_secs() != self.epoch.as_secs() {
            self.epoch = d_now;
            self.h1_date = HeaderValue::from_str(&to_date_string(d_now.as_secs())).unwrap();
        }
    }

    pub fn get_date(&mut self) -> HeaderValue {
        let d = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap();
        self.update(d);
        self.h1_date.clone()
    }
}

thread_local! {
    static CACHED_DATE: RefCell<CacheableDate>
        = RefCell::new(CacheableDate::new());
}

pub fn get_cached_date() -> HeaderValue {
    CACHED_DATE.with(|cache_date| (*cache_date.borrow_mut()).get_date())
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_date_string() {
        for (seconds, expected) in [
            (0, "Thu, 01 Jan 1970 00:00:00 GMT"),
            (1, "Thu, 01 Jan 1970 00:00:01 GMT"),
            (784_111_777, "Sun, 06 Nov 1994 08:49:37 GMT"),
            (951_782_399, "Mon, 28 Feb 2000 23:59:59 GMT"),
            (951_782_400, "Tue, 29 Feb 2000 00:00:00 GMT"),
            (951_868_800, "Wed, 01 Mar 2000 00:00:00 GMT"),
            (4_107_542_400, "Mon, 01 Mar 2100 00:00:00 GMT"),
            (253_402_300_799, "Fri, 31 Dec 9999 23:59:59 GMT"),
        ] {
            let actual = to_date_string(seconds);
            assert_eq!(actual, expected);
            assert_eq!(actual.len(), 29);
            assert_eq!(
                httpdate::parse_http_date(&actual).unwrap(),
                SystemTime::UNIX_EPOCH + Duration::from_secs(seconds)
            );
        }
    }

    #[test]
    fn test_date_string_out_of_range() {
        for seconds in [253_402_300_800, i64::MAX as u64, u64::MAX] {
            assert_eq!(to_date_string(seconds), "Fri, 31 Dec 9999 23:59:59 GMT");
        }
    }

    #[test]
    fn test_date_cached() {
        let mut cache = CacheableDate {
            h1_date: HeaderValue::from_str(&to_date_string(1)).unwrap(),
            epoch: Duration::from_secs(1),
        };
        let original = cache.h1_date.clone();
        cache.update(Duration::new(1, 999_999_999));
        assert_eq!(cache.h1_date, original);
        // The cached allocation is reused until the second changes.
        assert_eq!(
            cache.h1_date.as_bytes().as_ptr(),
            original.as_bytes().as_ptr()
        );
        assert_eq!(cache.epoch, Duration::from_secs(1));

        cache.update(Duration::from_secs(2));
        assert_eq!(cache.h1_date, "Thu, 01 Jan 1970 00:00:02 GMT");
        assert_eq!(cache.epoch, Duration::from_secs(2));

        // A clock adjustment backwards also refreshes the header.
        cache.update(Duration::from_secs(0));
        assert_eq!(cache.h1_date, "Thu, 01 Jan 1970 00:00:00 GMT");
        assert_eq!(cache.epoch, Duration::from_secs(0));
    }
}
