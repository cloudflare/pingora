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

use super::{Encode, COMPRESSION_ERROR};
use bytes::Bytes;
use parking_lot::Mutex;
use pingora_error::{OrErr, Result};
use std::io::Write;
use std::time::{Duration, Instant};
use zstd::stream::write::Encoder;

/// [RFC 9842](https://datatracker.ietf.org/doc/html/rfc9842) magic number for dcz.
pub const DCZ_MAGIC: [u8; 8] = [0x5e, 0x2a, 0x4d, 0x18, 0x20, 0x00, 0x00, 0x00];

/// [RFC 9842](https://datatracker.ietf.org/doc/html/rfc9842) header size: 8-byte magic + 32-byte SHA-256 hash.
pub const DCZ_HEADER_SIZE: usize = 40;

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

/// Dictionary compressor for [RFC 9842](https://datatracker.ietf.org/doc/html/rfc9842) (dcz).
/// Prepends [`DCZ_HEADER_SIZE`]-byte header to output.
pub struct DictionaryCompressor {
    compress: Mutex<Encoder<'static, Vec<u8>>>,
    dictionary_hash: [u8; 32],
    header_written: bool,
    total_in: usize,
    total_out: usize,
    duration: Duration,
}

impl DictionaryCompressor {
    pub fn new(level: u32, dictionary: &[u8], dictionary_hash: [u8; 32]) -> Result<Self> {
        let encoder = Encoder::with_dictionary(vec![], level as i32, dictionary).or_err(
            COMPRESSION_ERROR,
            "failed to create zstd encoder with dictionary",
        )?;

        Ok(DictionaryCompressor {
            compress: Mutex::new(encoder),
            dictionary_hash,
            header_written: false,
            total_in: 0,
            total_out: 0,
            duration: Duration::new(0, 0),
        })
    }

    fn build_header(&self) -> [u8; DCZ_HEADER_SIZE] {
        let mut header = [0u8; DCZ_HEADER_SIZE];
        header[..8].copy_from_slice(&DCZ_MAGIC);
        header[8..].copy_from_slice(&self.dictionary_hash);
        header
    }
}

impl Encode for DictionaryCompressor {
    fn encode(&mut self, input: &[u8], end: bool) -> Result<Bytes> {
        const MAX_INIT_COMPRESSED_BUF_SIZE: usize = 16 * 1024;
        let start = Instant::now();
        self.total_in += input.len();
        let mut compress = self.compress.lock();

        let reserve_size = if !self.header_written {
            DCZ_HEADER_SIZE + std::cmp::min(MAX_INIT_COMPRESSED_BUF_SIZE, input.len())
        } else {
            std::cmp::min(MAX_INIT_COMPRESSED_BUF_SIZE, input.len())
        };
        compress.get_mut().reserve(reserve_size);

        if !self.header_written {
            compress.get_mut().extend_from_slice(&self.build_header());
            self.header_written = true;
        }

        compress
            .write_all(input)
            .or_err(COMPRESSION_ERROR, "while compress dcz")?;
        if end {
            compress
                .do_finish()
                .or_err(COMPRESSION_ERROR, "while compress dcz")?;
        }
        self.total_out += compress.get_ref().len();
        self.duration += start.elapsed();
        Ok(std::mem::take(compress.get_mut()).into())
    }

    fn stat(&self) -> (&'static str, usize, usize, Duration) {
        ("dcz", self.total_in, self.total_out, self.duration)
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

#[cfg(test)]
mod tests_dictionary {
    use super::*;

    const TEST_DICTIONARY: &[u8] = b"The quick brown fox jumps over the lazy dog. \
        This is a test dictionary with common words and patterns that might appear \
        in the content being compressed. HTTP headers, JSON structures, HTML tags.";

    // This is not a real SHA-256 hash as specified in
    // [RFC 9842](https://datatracker.ietf.org/doc/html/rfc9842).
    // The compression module treats the dictionary hash as opaque bytes, so any
    // 32-byte value is sufficient to test that the hash is correctly written
    // into the DCZ header.
    fn test_dictionary_hash() -> [u8; 32] {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        let mut hasher = DefaultHasher::new();
        TEST_DICTIONARY.hash(&mut hasher);
        let hash = hasher.finish();

        let mut result = [0u8; 32];
        result[..8].copy_from_slice(&hash.to_le_bytes());
        result[8..16].copy_from_slice(&hash.to_be_bytes());
        for (i, byte) in result[16..32].iter_mut().enumerate() {
            *byte = ((i + 16) as u8).wrapping_mul(hash as u8);
        }
        result
    }

    #[test]
    fn compress_dcz_prepends_header() {
        let hash = test_dictionary_hash();
        let mut compressor = DictionaryCompressor::new(3, TEST_DICTIONARY, hash).unwrap();

        let input = b"The quick brown fox jumps over the lazy dog again.";
        let compressed = compressor.encode(input, true).unwrap();

        assert!(compressed.len() >= DCZ_HEADER_SIZE);
        // RFC 9842 magic
        assert_eq!(&compressed[..8], &DCZ_MAGIC);
        // dictionary hash
        assert_eq!(&compressed[8..40], &hash);
        // zstd magic follows
        assert_eq!(&compressed[40..44], &[0x28, 0xB5, 0x2F, 0xFD]);
    }

    #[test]
    fn compress_dcz_header_written_once() {
        let hash = test_dictionary_hash();
        let mut compressor = DictionaryCompressor::new(3, TEST_DICTIONARY, hash).unwrap();

        let chunk1 = compressor.encode(b"First chunk of data. ", false).unwrap();
        assert!(chunk1.len() >= DCZ_HEADER_SIZE);
        assert_eq!(&chunk1[..8], &DCZ_MAGIC);

        let chunk2 = compressor.encode(b"Second chunk of data.", true).unwrap();
        if chunk2.len() >= 8 {
            assert_ne!(&chunk2[..8], &DCZ_MAGIC);
        }
    }

    #[test]
    fn compress_dcz_stats() {
        let hash = test_dictionary_hash();
        let mut compressor = DictionaryCompressor::new(3, TEST_DICTIONARY, hash).unwrap();

        let input = b"Some test data to compress with the dictionary.";
        let _ = compressor.encode(input, true).unwrap();

        let (name, total_in, total_out, duration) = compressor.stat();
        assert_eq!(name, "dcz");
        assert_eq!(total_in, input.len());
        assert!(total_out >= DCZ_HEADER_SIZE);
        assert!(duration.as_nanos() > 0);
    }

    #[test]
    fn compress_dcz_empty_input() {
        let hash = test_dictionary_hash();
        let mut compressor = DictionaryCompressor::new(3, TEST_DICTIONARY, hash).unwrap();

        let compressed = compressor.encode(b"", true).unwrap();
        assert!(compressed.len() >= DCZ_HEADER_SIZE);
        assert_eq!(&compressed[..8], &DCZ_MAGIC);
    }

    #[test]
    fn compress_dcz_streaming() {
        let hash = test_dictionary_hash();
        let mut compressor = DictionaryCompressor::new(3, TEST_DICTIONARY, hash).unwrap();

        let chunks: &[&[u8]] = &[b"First part. ", b"Second part. ", b"Final part."];

        let mut all_output = Vec::new();
        for (i, chunk) in chunks.iter().enumerate() {
            let output = compressor.encode(chunk, i == chunks.len() - 1).unwrap();
            all_output.extend_from_slice(&output);
        }

        assert!(all_output.len() >= DCZ_HEADER_SIZE);
        assert_eq!(&all_output[..8], &DCZ_MAGIC);

        let (_, total_in, _, _) = compressor.stat();
        let expected_in: usize = chunks.iter().map(|c| c.len()).sum();
        assert_eq!(total_in, expected_in);
    }

    #[test]
    fn compress_dcz_achieves_compression() {
        let hash = test_dictionary_hash();
        let mut compressor = DictionaryCompressor::new(3, TEST_DICTIONARY, hash).unwrap();

        let input = b"The quick brown fox jumps over the lazy dog. \
            The quick brown fox jumps over the lazy dog. \
            The quick brown fox jumps over the lazy dog.";

        let compressed = compressor.encode(input, true).unwrap();
        let compressed_data_size = compressed.len() - DCZ_HEADER_SIZE;
        assert!(compressed_data_size < input.len());
    }

    #[test]
    fn compress_dcz_roundtrip() {
        let hash = test_dictionary_hash();
        let mut compressor = DictionaryCompressor::new(3, TEST_DICTIONARY, hash).unwrap();

        let input = b"The quick brown fox jumps over the lazy dog. \
            HTTP headers, JSON structures, HTML tags. \
            Common patterns that appear in web content.";
        let compressed = compressor.encode(input, true).unwrap();

        // Verify DCZ header is present then strip it
        assert!(compressed.len() >= DCZ_HEADER_SIZE);
        assert_eq!(&compressed[..8], &DCZ_MAGIC);
        let zstd_data = &compressed[DCZ_HEADER_SIZE..];

        // Decompress with the same dictionary and verify roundtrip
        let mut decoder =
            zstd::stream::read::Decoder::with_dictionary(zstd_data, TEST_DICTIONARY).unwrap();
        let mut decompressed = Vec::new();
        std::io::Read::read_to_end(&mut decoder, &mut decompressed).unwrap();

        assert_eq!(decompressed, input);
    }
}
