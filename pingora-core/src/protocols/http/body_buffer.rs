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

use bytes::{Bytes, BytesMut};

/// A buffer with size limit. When the total amount of data written to the buffer is below the limit
/// all the data will be held in the buffer. Otherwise, the buffer will report to be truncated.
pub(crate) struct FixedBuffer {
    buffer: BytesMut,
    capacity: usize,
    truncated: bool,
}

impl FixedBuffer {
    pub fn new(capacity: usize) -> Self {
        FixedBuffer {
            buffer: BytesMut::new(),
            capacity,
            truncated: false,
        }
    }

    // TODO: maybe store a Vec of Bytes for zero-copy
    pub fn write_to_buffer(&mut self, data: &Bytes) {
        if !self.truncated && (self.buffer.len() + data.len() <= self.capacity) {
            self.buffer.extend_from_slice(data);
        } else {
            // TODO: clear data because the data held here is useless anyway?
            self.truncated = true;
        }
    }
    pub fn clear(&mut self) {
        self.truncated = false;
        self.buffer.clear();
    }
    pub fn is_empty(&self) -> bool {
        self.buffer.len() == 0
    }
    pub fn is_truncated(&self) -> bool {
        self.truncated
    }
    pub fn get_buffer(&self) -> Option<Bytes> {
        // TODO: return None if truncated?
        if !self.is_empty() {
            Some(self.buffer.clone().freeze())
        } else {
            None
        }
    }
}
