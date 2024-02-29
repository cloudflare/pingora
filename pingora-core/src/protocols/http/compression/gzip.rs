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

use super::Encode;

use bytes::Bytes;
use flate2::write::GzEncoder;
use pingora_error::Result;
use std::io::Write;
use std::time::{Duration, Instant};

// TODO: unzip

pub struct Compressor {
    // TODO: enum for other compression algorithms
    compress: GzEncoder<Vec<u8>>,
    total_in: usize,
    total_out: usize,
    duration: Duration,
}

impl Compressor {
    pub fn new(level: u32) -> Compressor {
        Compressor {
            compress: GzEncoder::new(vec![], flate2::Compression::new(level)),
            total_in: 0,
            total_out: 0,
            duration: Duration::new(0, 0),
        }
    }
}

impl Encode for Compressor {
    // infallible because compression can take any data
    fn encode(&mut self, input: &[u8], end: bool) -> Result<Bytes> {
        // reserve at most 16k
        const MAX_INIT_COMPRESSED_BUF_SIZE: usize = 16 * 1024;
        let start = Instant::now();
        self.total_in += input.len();
        self.compress
            .get_mut()
            .reserve(std::cmp::min(MAX_INIT_COMPRESSED_BUF_SIZE, input.len()));
        self.write_all(input).unwrap(); // write to vec, should never fail
        if end {
            self.try_finish().unwrap(); // write to vec, should never fail
        }
        self.total_out += self.compress.get_ref().len();
        self.duration += start.elapsed();
        Ok(std::mem::take(self.compress.get_mut()).into()) // into() Bytes will drop excess capacity
    }

    fn stat(&self) -> (&'static str, usize, usize, Duration) {
        ("gzip", self.total_in, self.total_out, self.duration)
    }
}

use std::ops::{Deref, DerefMut};
impl Deref for Compressor {
    type Target = GzEncoder<Vec<u8>>;

    fn deref(&self) -> &Self::Target {
        &self.compress
    }
}

impl DerefMut for Compressor {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.compress
    }
}

#[cfg(test)]
mod tests_stream {
    use super::*;

    #[test]
    fn gzip_data() {
        let mut compressor = Compressor::new(6);
        let compressed = compressor.encode(b"abcdefg", true).unwrap();
        // gzip magic headers
        assert_eq!(&compressed[..3], &[0x1f, 0x8b, 0x08]);
        // check the crc32 footer
        assert_eq!(
            &compressed[compressed.len() - 9..],
            &[0, 166, 106, 42, 49, 7, 0, 0, 0]
        );
        assert_eq!(compressor.total_in, 7);
        assert_eq!(compressor.total_out, compressed.len());

        assert!(compressor.get_ref().is_empty());
    }
}
