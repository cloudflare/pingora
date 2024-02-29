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

use super::{Encode, COMPRESSION_ERROR};
use bytes::Bytes;
use parking_lot::Mutex;
use pingora_error::{OrErr, Result};
use std::io::Write;
use std::time::{Duration, Instant};
use zstd::stream::write::Encoder;

pub struct Compressor {
    compress: Mutex<Encoder<'static, Vec<u8>>>,
    total_in: usize,
    total_out: usize,
    duration: Duration,
}

impl Compressor {
    pub fn new(level: u32) -> Self {
        Compressor {
            // Mutex because Encoder is not Sync
            // https://github.com/gyscos/zstd-rs/issues/186
            compress: Mutex::new(Encoder::new(vec![], level as i32).unwrap()),
            total_in: 0,
            total_out: 0,
            duration: Duration::new(0, 0),
        }
    }
}

impl Encode for Compressor {
    fn encode(&mut self, input: &[u8], end: bool) -> Result<Bytes> {
        // reserve at most 16k
        const MAX_INIT_COMPRESSED_BUF_SIZE: usize = 16 * 1024;
        let start = Instant::now();
        self.total_in += input.len();
        let mut compress = self.compress.lock();
        // reserve at most input size, cap at 16k, compressed output should be smaller
        compress
            .get_mut()
            .reserve(std::cmp::min(MAX_INIT_COMPRESSED_BUF_SIZE, input.len()));
        compress
            .write_all(input)
            .or_err(COMPRESSION_ERROR, "while compress zstd")?;
        // write to vec will never fail.
        if end {
            compress
                .do_finish()
                .or_err(COMPRESSION_ERROR, "while compress zstd")?;
        }
        self.total_out += compress.get_ref().len();
        self.duration += start.elapsed();
        Ok(std::mem::take(compress.get_mut()).into()) // into() Bytes will drop excess capacity
    }

    fn stat(&self) -> (&'static str, usize, usize, Duration) {
        ("zstd", self.total_in, self.total_out, self.duration)
    }
}

#[cfg(test)]
mod tests_stream {
    use super::*;

    #[test]
    fn compress_zstd_data() {
        let mut compressor = Compressor::new(11);
        let input = b"adcdefgabcdefghadcdefgabcdefghadcdefgabcdefghadcdefgabcdefgh\n";
        let compressed = compressor.encode(&input[..], false).unwrap();
        // waiting for more data
        assert!(compressed.is_empty());

        let compressed = compressor.encode(&input[..], true).unwrap();

        // the zstd Magic_Number
        assert_eq!(&compressed[..4], &[0x28, 0xB5, 0x2F, 0xFD]);
        assert!(compressed.len() < input.len());
    }
}
