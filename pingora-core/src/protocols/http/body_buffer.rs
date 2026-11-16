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

use bytes::{Bytes, BytesMut};

/// Chunks smaller than this are copied into `tail`, so a small slice does not keep a larger
/// backing allocation (such as an HTTP/2 connection's read buffer) alive.
const COALESCE_THRESHOLD: usize = 4 * 1024;

/// `tail` moves to `chunks` rather than growing past this size.
const TAIL_SEGMENT_LIMIT: usize = 2 * COALESCE_THRESHOLD;

/// A buffer with size limit. When the total amount of data written to the buffer is below the limit
/// all the data will be held in the buffer. Otherwise, the buffer will report to be truncated.
///
/// Large chunks are kept by reference count; small ones are copied together.
pub struct FixedBuffer {
    chunks: Vec<Bytes>,
    /// Small chunks copied together, following `chunks`.
    tail: BytesMut,
    len: usize,
    capacity: usize,
    truncated: bool,
}

impl FixedBuffer {
    pub fn new(capacity: usize) -> Self {
        FixedBuffer {
            chunks: Vec::new(),
            tail: BytesMut::new(),
            len: 0,
            capacity,
            truncated: false,
        }
    }

    pub fn write_to_buffer(&mut self, data: &Bytes) {
        if self.truncated {
            return;
        }

        if self.len + data.len() > self.capacity {
            // Buffered data is no longer usable for a retry, so release it.
            *self = FixedBuffer {
                truncated: true,
                ..FixedBuffer::new(self.capacity)
            };
            return;
        }

        if data.is_empty() {
            return;
        }

        self.len += data.len();
        if data.len() < COALESCE_THRESHOLD {
            let needed = self.tail.len() + data.len();
            if needed > TAIL_SEGMENT_LIMIT {
                self.seal_tail();
            } else if needed > self.tail.capacity() && !self.tail.is_empty() {
                // A lone small chunk stays exact-size; the second grows straight to the limit.
                self.tail.reserve(TAIL_SEGMENT_LIMIT - self.tail.len());
            }
            self.tail.extend_from_slice(data);
        } else {
            self.seal_tail();
            self.chunks.push(data.clone());
        }
    }

    fn seal_tail(&mut self) {
        if !self.tail.is_empty() {
            self.chunks.push(self.tail.split().freeze());
        }
    }

    pub fn clear(&mut self) {
        self.truncated = false;
        self.chunks.clear();
        self.tail.clear();
        self.len = 0;
    }
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
    pub fn is_truncated(&self) -> bool {
        self.truncated
    }

    /// The buffered data, or `None` if empty or truncated. Copies unless it holds a single chunk.
    pub fn get_buffer(&self) -> Option<Bytes> {
        if self.truncated || self.is_empty() {
            return None;
        }
        if self.tail.is_empty() {
            if let [chunk] = self.chunks.as_slice() {
                return Some(chunk.clone());
            }
        }
        // Not cached, so a retry does not hold two copies.
        let mut joined = BytesMut::with_capacity(self.len);
        for chunk in &self.chunks {
            joined.extend_from_slice(chunk);
        }
        joined.extend_from_slice(&self.tail);
        Some(joined.freeze())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIMIT: usize = 64 * 1024;

    fn pattern(len: usize, seed: u8) -> Bytes {
        (0..len)
            .map(|i| seed.wrapping_add(i as u8))
            .collect::<Vec<u8>>()
            .into()
    }

    fn assert_released(buffer: &FixedBuffer) {
        assert!(buffer.is_truncated());
        assert!(buffer.is_empty());
        assert_eq!(buffer.chunks.capacity(), 0);
        assert_eq!(buffer.tail.capacity(), 0);
        assert!(buffer.get_buffer().is_none());
    }

    #[test]
    fn mixed_chunk_sizes_round_trip() {
        let sizes = [
            10,
            COALESCE_THRESHOLD - 1,
            COALESCE_THRESHOLD,
            3,
            9000,
            100,
            TAIL_SEGMENT_LIMIT,
            1,
            32 * 1024,
            50,
        ];
        let mut buffer = FixedBuffer::new(LIMIT);
        let mut expected = Vec::new();
        for (i, size) in sizes.into_iter().enumerate() {
            let chunk = pattern(size, i as u8);
            expected.extend_from_slice(&chunk);
            buffer.write_to_buffer(&chunk);
        }

        assert!(!buffer.is_truncated());
        assert_eq!(buffer.len, expected.len());
        assert_eq!(buffer.get_buffer().unwrap(), expected);
    }

    #[test]
    fn exactly_at_limit_is_kept() {
        let mut buffer = FixedBuffer::new(LIMIT);
        let first = pattern(LIMIT - 100, 1);
        let last = pattern(100, 2);
        buffer.write_to_buffer(&first);
        buffer.write_to_buffer(&last);

        assert!(!buffer.is_truncated());
        assert_eq!(buffer.get_buffer().unwrap(), [first, last].concat());

        // An empty chunk at the limit does not truncate either.
        buffer.write_to_buffer(&Bytes::new());
        assert!(!buffer.is_truncated());
    }

    #[test]
    fn below_limit_is_kept() {
        let mut buffer = FixedBuffer::new(LIMIT);
        let chunk = pattern(LIMIT - 1, 3);
        buffer.write_to_buffer(&chunk);

        assert!(!buffer.is_truncated());
        assert_eq!(buffer.get_buffer().unwrap(), chunk);
    }

    #[test]
    fn exceeding_limit_releases_everything() {
        let mut buffer = FixedBuffer::new(LIMIT);
        buffer.write_to_buffer(&pattern(COALESCE_THRESHOLD, 1));
        buffer.write_to_buffer(&pattern(10, 2));
        assert_eq!(buffer.chunks.len(), 1);
        assert!(!buffer.tail.is_empty());

        buffer.write_to_buffer(&pattern(LIMIT, 3));
        assert_released(&buffer);

        // A partial body is never replayed, even if later chunks would fit.
        buffer.write_to_buffer(&pattern(10, 4));
        assert_released(&buffer);

        buffer.clear();
        assert!(!buffer.is_truncated());
        let chunk = pattern(10, 5);
        buffer.write_to_buffer(&chunk);
        assert_eq!(buffer.get_buffer().unwrap(), chunk);
    }

    #[test]
    fn large_chunk_is_not_copied() {
        let mut buffer = FixedBuffer::new(LIMIT);
        let small = pattern(10, 1);
        let large = pattern(COALESCE_THRESHOLD, 2);
        buffer.write_to_buffer(&small);
        buffer.write_to_buffer(&large);

        assert_eq!(buffer.chunks.len(), 2);
        assert!(buffer.tail.is_empty());
        assert_eq!(buffer.chunks[1].as_ptr(), large.as_ptr());
        assert_eq!(buffer.get_buffer().unwrap(), [small, large].concat());
    }

    #[test]
    fn single_chunk_get_buffer_is_not_copied() {
        let mut buffer = FixedBuffer::new(LIMIT);
        let chunk = pattern(32 * 1024, 1);
        buffer.write_to_buffer(&chunk);

        let retry = buffer.get_buffer().unwrap();
        assert_eq!(retry.as_ptr(), chunk.as_ptr());
        assert_eq!(retry, chunk);
    }

    #[test]
    fn small_chunks_are_coalesced() {
        let mut buffer = FixedBuffer::new(LIMIT);
        // A small slice must not keep its much larger backing allocation alive.
        let backing = pattern(32 * 1024, 1);
        let mut expected = Vec::new();
        for i in 0..100 {
            let chunk = backing.slice(i * 100..(i + 1) * 100);
            expected.extend_from_slice(&chunk);
            buffer.write_to_buffer(&chunk);
            if i == 80 {
                assert!(buffer.chunks.is_empty());
                assert_eq!(buffer.tail.len(), 8100);
                assert!(buffer.tail.capacity() <= TAIL_SEGMENT_LIMIT);
            }
        }

        assert_eq!(buffer.chunks.len(), 1);
        assert_eq!(buffer.chunks[0].len(), 8100);
        assert_eq!(buffer.tail.len(), 1900);
        assert!(buffer.tail.capacity() <= TAIL_SEGMENT_LIMIT);
        let backing_range = backing.as_ptr_range();
        assert!(!backing_range.contains(&buffer.chunks[0].as_ptr()));
        assert!(!backing_range.contains(&buffer.tail.as_ptr()));
        assert_eq!(buffer.get_buffer().unwrap(), expected);
    }

    #[test]
    fn sealed_tail_segments_share_one_allocation() {
        let mut buffer = FixedBuffer::new(LIMIT);
        let small = pattern(100, 1);
        let large = pattern(COALESCE_THRESHOLD, 2);
        let mut expected = Vec::new();
        for _ in 0..8 {
            for chunk in [&small, &small, &large] {
                expected.extend_from_slice(chunk);
                buffer.write_to_buffer(chunk);
            }
        }

        // Sealing keeps the tail's spare capacity, so each segment follows the previous one.
        let segments: Vec<_> = buffer.chunks.iter().filter(|c| c.len() == 200).collect();
        assert_eq!(segments.len(), 8);
        for pair in segments.windows(2) {
            assert_eq!(pair[0].as_ptr_range().end, pair[1].as_ptr());
        }
        assert_eq!(buffer.get_buffer().unwrap(), expected);
    }

    #[test]
    fn lone_small_chunk_keeps_exact_allocation() {
        let mut buffer = FixedBuffer::new(LIMIT);
        let chunk = pattern(100, 1);
        buffer.write_to_buffer(&chunk);

        assert!(buffer.tail.capacity() < TAIL_SEGMENT_LIMIT);
        assert_eq!(buffer.get_buffer().unwrap(), chunk);
    }

    #[test]
    fn empty_chunks_are_ignored() {
        let mut buffer = FixedBuffer::new(LIMIT);
        buffer.write_to_buffer(&Bytes::new());
        assert!(buffer.is_empty());
        assert!(buffer.get_buffer().is_none());
        assert!(buffer.chunks.is_empty());
        assert_eq!(buffer.tail.capacity(), 0);

        let chunk = pattern(COALESCE_THRESHOLD, 1);
        buffer.write_to_buffer(&chunk);
        buffer.write_to_buffer(&Bytes::new());
        assert_eq!(buffer.chunks.len(), 1);
        assert_eq!(buffer.get_buffer().unwrap().as_ptr(), chunk.as_ptr());
    }

    #[test]
    fn get_buffer_does_not_retain_joined_copy() {
        let mut buffer = FixedBuffer::new(LIMIT);
        let first = pattern(COALESCE_THRESHOLD, 1);
        let second = pattern(10, 2);
        buffer.write_to_buffer(&first);
        buffer.write_to_buffer(&second);
        let chunk_ptrs: Vec<_> = buffer.chunks.iter().map(|c| c.as_ptr()).collect();
        let tail = (
            buffer.tail.as_ptr(),
            buffer.tail.len(),
            buffer.tail.capacity(),
        );

        let expected = [first, second].concat();
        let retry = buffer.get_buffer().unwrap();
        let again = buffer.get_buffer().unwrap();
        assert_eq!(retry, expected);
        assert_eq!(again, expected);
        assert_ne!(retry.as_ptr(), again.as_ptr());

        let after: Vec<_> = buffer.chunks.iter().map(|c| c.as_ptr()).collect();
        assert_eq!(after, chunk_ptrs);
        assert_eq!(
            (
                buffer.tail.as_ptr(),
                buffer.tail.len(),
                buffer.tail.capacity()
            ),
            tail
        );
        assert_eq!(buffer.len, expected.len());
    }

    #[test]
    fn clear_resets_contents() {
        let mut buffer = FixedBuffer::new(LIMIT);
        buffer.write_to_buffer(&pattern(COALESCE_THRESHOLD, 1));
        buffer.write_to_buffer(&pattern(10, 2));
        assert!(buffer.get_buffer().is_some());

        buffer.clear();
        assert!(buffer.is_empty());
        assert!(buffer.get_buffer().is_none());

        let chunk = pattern(10, 3);
        buffer.write_to_buffer(&chunk);
        assert_eq!(buffer.get_buffer().unwrap(), chunk);
    }
}
