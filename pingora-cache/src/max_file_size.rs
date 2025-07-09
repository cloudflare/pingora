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

//! Set limit on the largest size to cache

use pingora_error::ErrorType;

/// Error type returned when the limit is reached.
pub const ERR_RESPONSE_TOO_LARGE: ErrorType = ErrorType::Custom("response too large");

// Body bytes tracker to adjust (predicted) cacheability,
// even if cache has been disabled.
#[derive(Debug)]
pub(crate) struct MaxFileSizeTracker {
    body_bytes: usize,
    max_size: usize,
}

impl MaxFileSizeTracker {
    // Create a new Tracker object.
    pub fn new(max_size: usize) -> MaxFileSizeTracker {
        MaxFileSizeTracker {
            body_bytes: 0,
            max_size,
        }
    }

    // Add bytes to the tracker.
    // If return value is true, the tracker bytes are under the max size allowed.
    pub fn add_body_bytes(&mut self, bytes: usize) -> bool {
        self.body_bytes += bytes;
        self.allow_caching()
    }

    pub fn max_file_size_bytes(&self) -> usize {
        self.max_size
    }

    pub fn allow_caching(&self) -> bool {
        self.body_bytes <= self.max_size
    }
}
