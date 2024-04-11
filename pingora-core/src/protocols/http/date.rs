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

use chrono::DateTime;
use http::header::HeaderValue;
use std::cell::RefCell;
use std::time::{Duration, SystemTime};

fn to_date_string(epoch_sec: i64) -> String {
    let dt = DateTime::from_timestamp(epoch_sec, 0).unwrap();
    dt.format("%a, %d %b %Y %H:%M:%S GMT").to_string()
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
            h1_date: HeaderValue::from_str(&to_date_string(d.as_secs() as i64)).unwrap(),
            epoch: d,
        }
    }

    pub fn update(&mut self, d_now: Duration) {
        if d_now.as_secs() != self.epoch.as_secs() {
            self.epoch = d_now;
            self.h1_date = HeaderValue::from_str(&to_date_string(d_now.as_secs() as i64)).unwrap();
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

    fn now_date_header() -> HeaderValue {
        HeaderValue::from_str(&to_date_string(
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_secs() as i64,
        ))
        .unwrap()
    }

    #[test]
    fn test_date_string() {
        let date_str = to_date_string(1);
        assert_eq!("Thu, 01 Jan 1970 00:00:01 GMT", date_str);
    }

    #[test]
    fn test_date_cached() {
        assert_eq!(get_cached_date(), now_date_header());
    }
}
