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

use super::Encode;
use super::COMPRESSION_ERROR;

use brotli::{CompressorWriter, DecompressorWriter};
use bytes::Bytes;
use pingora_error::{OrErr, Result};
use std::io::Write;
use std::time::{Duration, Instant};

pub struct Decompressor {
    decompress: DecompressorWriter<Vec<u8>>,
    total_in: usize,
    total_out: usize,
    duration: Duration,
}

impl Decompressor {
    pub fn new() -> Self {
        Decompressor {
            // default buf is 4096 if 0 is used, TODO: figure out the significance of this value
            decompress: DecompressorWriter::new(vec![], 0),
            total_in: 0,
            total_out: 0,
            duration: Duration::new(0, 0),
        }
    }
}

impl Encode for Decompressor {
    fn encode(&mut self, input: &[u8], end: bool) -> Result<Bytes> {
        const MAX_INIT_COMPRESSED_SIZE_CAP: usize = 4 * 1024;
        // Brotli compress ratio can be 3.5 to 4.5
        const ESTIMATED_COMPRESSION_RATIO: usize = 4;
        let start = Instant::now();
        self.total_in += input.len();
        // cap the buf size amplification, there is a DoS risk of always allocate
        // 4x the memory of the input buffer
        let reserve_size = if input.len() < MAX_INIT_COMPRESSED_SIZE_CAP {
            input.len() * ESTIMATED_COMPRESSION_RATIO
        } else {
            input.len()
        };
        self.decompress.get_mut().reserve(reserve_size);
        self.decompress
            .write_all(input)
            .or_err(COMPRESSION_ERROR, "while decompress Brotli")?;
        // write to vec will never fail. The only possible error is that the input data
        // is invalid (not brotli compressed)
        if end {
            self.decompress
                .flush()
                .or_err(COMPRESSION_ERROR, "while decompress Brotli")?;
        }
        self.total_out += self.decompress.get_ref().len();
        self.duration += start.elapsed();
        Ok(std::mem::take(self.decompress.get_mut()).into()) // into() Bytes will drop excess capacity
    }

    fn stat(&self) -> (&'static str, usize, usize, Duration) {
        ("de-brotli", self.total_in, self.total_out, self.duration)
    }
}

pub struct Compressor {
    compress: Option<CompressorWriter<Vec<u8>>>,
    total_in: usize,
    total_out: usize,
    duration: Duration,
}

impl Compressor {
    pub fn new(level: u32) -> Self {
        Compressor {
            // buf_size:4096 , lgwin:19 TODO: fine tune these
            compress: Some(CompressorWriter::new(vec![], 4096, level, 19)),
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

        // reserve at most input size, cap at 16k, compressed output should be smaller
        let Some(compress) = self.compress.as_mut() else {
            // The stream was already finished; nothing more to emit.
            return Ok(Bytes::new());
        };
        compress
            .get_mut()
            .reserve(std::cmp::min(MAX_INIT_COMPRESSED_BUF_SIZE, input.len()));
        compress
            .write_all(input)
            .or_err(COMPRESSION_ERROR, "while compress Brotli")?;
        // write to vec will never fail.
        if !end {
            self.total_out += compress.get_ref().len();
            self.duration += start.elapsed();
            return Ok(std::mem::take(compress.get_mut()).into()); // into() Bytes will drop excess capacity
        }

        // Flushing only drains pending output; it does not emit the final
        // ISLAST meta-block, which would be left in the writer and lost when
        // it is dropped. Finish the stream via into_inner() (which runs
        // BROTLI_OPERATION_FINISH) so the returned buffer is a complete
        // brotli stream that strict decoders accept.
        let writer = self.compress.take().unwrap();
        let finished = writer.into_inner();
        self.total_out += finished.len();
        self.duration += start.elapsed();
        Ok(finished.into()) // into() Bytes will drop excess capacity
    }

    fn stat(&self) -> (&'static str, usize, usize, Duration) {
        ("brotli", self.total_in, self.total_out, self.duration)
    }
}

#[cfg(test)]
mod tests_stream {
    use super::*;

    #[test]
    fn decompress_brotli_data() {
        let mut compressor = Decompressor::new();
        let decompressed = compressor
            .encode(
                &[
                    0x1f, 0x0f, 0x00, 0xf8, 0x45, 0x07, 0x87, 0x3e, 0x10, 0xfb, 0x55, 0x92, 0xec,
                    0x12, 0x09, 0xcc, 0x38, 0xdd, 0x51, 0x1e,
                ],
                true,
            )
            .unwrap();

        assert_eq!(&decompressed[..], &b"adcdefgabcdefgh\n"[..]);
    }

    #[test]
    fn compress_brotli_data() {
        let mut compressor = Compressor::new(11);
        let compressed = compressor.encode(&b"adcdefgabcdefgh\n"[..], true).unwrap();

        // The finished stream now carries the final ISLAST meta-block, so the
        // pinned bytes differ from the pre-fix truncated prefix (0x85, 0x07,
        // ...): rust-brotli packs the flush and finish paths differently.
        assert_eq!(
            &compressed[..],
            &[
                0x15, 0x0f, 0x00, 0xf8, 0x45, 0x07, 0x87, 0x3e, 0x10, 0xfb, 0x55, 0x92, 0xec, 0x12,
                0x09, 0xcc, 0x38, 0xdd, 0x51, 0x1e,
            ],
        );
        // And the pinned stream must be complete for strict decoders.
        assert_strict_roundtrip(&compressed, &b"adcdefgabcdefgh\n"[..]);
    }

    // Regression tests: the compressor must emit a COMPLETE brotli stream —
    // including the final ISLAST meta-block — that a strict one-shot decoder
    // accepts. A stream whose pending output was only flushed is an
    // incomplete prefix and is rejected by strict decoders with an
    // unexpected-EOF error.
    fn assert_strict_roundtrip(encoded: &[u8], expected: &[u8]) {
        let mut decoded = Vec::new();
        brotli::BrotliDecompress(&mut &encoded[..], &mut decoded)
            .expect("strict decoder must accept the stream as complete");
        assert_eq!(&decoded[..], expected);
    }

    #[test]
    fn compress_brotli_single_chunk_strict_roundtrip() {
        let mut compressor = Compressor::new(5);
        let compressed = compressor.encode(b"hello world", true).unwrap();
        assert_strict_roundtrip(&compressed, b"hello world");
    }

    #[test]
    fn compress_brotli_multi_chunk_strict_roundtrip() {
        let mut compressor = Compressor::new(5);
        let mut encoded = Vec::new();
        encoded.extend_from_slice(compressor.encode(b"hello ", false).unwrap().as_ref());
        encoded.extend_from_slice(compressor.encode(b"world", false).unwrap().as_ref());
        encoded.extend_from_slice(compressor.encode(b"", true).unwrap().as_ref());
        assert_strict_roundtrip(&encoded, b"hello world");
    }

    #[test]
    fn compress_brotli_empty_body_strict_roundtrip() {
        let mut compressor = Compressor::new(5);
        let compressed = compressor.encode(b"", true).unwrap();
        assert_strict_roundtrip(&compressed, b"");
    }
}
