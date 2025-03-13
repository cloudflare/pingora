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

use std::cell::{RefCell, RefMut};
use thread_local::ThreadLocal;
use zstd_safe::{CCtx, CDict, DCtx, DDict};

/// Each thread will own its compression and decompression CTXes, and they share a single dict
/// https://facebook.github.io/zstd/zstd_manual.html recommends to reuse ctx per thread

// Both `Compression` and `CompressionWithDict` are just wrappers around the inner compression and
// decompression contexts, but have different APIs to access it.

#[derive(Default)]
pub struct Compression(CompressionInner);

// these codes are inspired by zstd crate

impl Compression {
    pub fn new() -> Self {
        Compression(CompressionInner::new())
    }

    pub fn compress_to_buffer<C: zstd_safe::WriteBuf + ?Sized>(
        &self,
        source: &[u8],
        destination: &mut C,
        level: i32,
    ) -> Result<usize, &'static str> {
        self.0.compress_to_buffer(source, destination, level)
    }

    pub fn compress(&self, data: &[u8], level: i32) -> Result<Vec<u8>, &'static str> {
        let mut buffer = make_compressed_data_buffer(data.len());
        self.compress_to_buffer(data, &mut buffer, level)?;
        Ok(buffer)
    }

    pub fn decompress_to_buffer<C: zstd_safe::WriteBuf + ?Sized>(
        &self,
        source: &[u8],
        destination: &mut C,
    ) -> Result<usize, &'static str> {
        self.0.decompress_to_buffer(source, destination)
    }
}

pub struct CompressionWithDict {
    inner: CompressionInner,
    // these dictionaries are owned by this struct, hence the static lifetime
    com_dict: CDict<'static>,
    de_dict: DDict<'static>,
}

impl CompressionWithDict {
    pub fn new(dict: &[u8], compression_level: i32) -> Self {
        CompressionWithDict {
            inner: CompressionInner::new(),
            // compression dictionary needs to be loaded ahead of time
            // with the compression level
            com_dict: CDict::create(dict, compression_level),
            de_dict: DDict::create(dict),
        }
    }

    pub fn compress_to_buffer<C: zstd_safe::WriteBuf + ?Sized>(
        &self,
        source: &[u8],
        destination: &mut C,
    ) -> Result<usize, &'static str> {
        self.inner
            .compress_to_buffer_using_dict(source, destination, &self.com_dict)
    }

    pub fn compress(&self, data: &[u8]) -> Result<Vec<u8>, &'static str> {
        let mut buffer = make_compressed_data_buffer(data.len());
        self.compress_to_buffer(data, &mut buffer)?;
        Ok(buffer)
    }

    pub fn decompress_to_buffer<C: zstd_safe::WriteBuf + ?Sized>(
        &self,
        source: &[u8],
        destination: &mut C,
    ) -> Result<usize, &'static str> {
        self.inner
            .decompress_to_buffer_using_dict(source, destination, &self.de_dict)
    }
}

#[derive(Default)]
struct CompressionInner {
    com_context: ThreadLocal<RefCell<zstd_safe::CCtx<'static>>>,
    de_context: ThreadLocal<RefCell<zstd_safe::DCtx<'static>>>,
}

impl CompressionInner {
    fn new() -> Self {
        CompressionInner {
            com_context: ThreadLocal::new(),
            de_context: ThreadLocal::new(),
        }
    }

    #[inline]
    fn get_com_context(&self) -> RefMut<CCtx<'static>> {
        self.com_context
            .get_or(|| RefCell::new(CCtx::create()))
            .borrow_mut()
    }

    #[inline]
    fn get_de_context(&self) -> RefMut<DCtx<'static>> {
        self.de_context
            .get_or(|| RefCell::new(DCtx::create()))
            .borrow_mut()
    }

    fn compress_to_buffer<C: zstd_safe::WriteBuf + ?Sized>(
        &self,
        source: &[u8],
        destination: &mut C,
        level: i32,
    ) -> Result<usize, &'static str> {
        self.get_com_context()
            .compress(destination, source, level)
            .map_err(zstd_safe::get_error_name)
    }

    fn decompress_to_buffer<C: zstd_safe::WriteBuf + ?Sized>(
        &self,
        source: &[u8],
        destination: &mut C,
    ) -> Result<usize, &'static str> {
        self.get_de_context()
            .decompress(destination, source)
            .map_err(zstd_safe::get_error_name)
    }

    fn compress_to_buffer_using_dict<C: zstd_safe::WriteBuf + ?Sized>(
        &self,
        source: &[u8],
        destination: &mut C,
        dict: &CDict,
    ) -> Result<usize, &'static str> {
        self.get_com_context()
            .compress_using_cdict(destination, source, dict)
            .map_err(zstd_safe::get_error_name)
    }

    pub fn decompress_to_buffer_using_dict<C: zstd_safe::WriteBuf + ?Sized>(
        &self,
        source: &[u8],
        destination: &mut C,
        dict: &DDict,
    ) -> Result<usize, &'static str> {
        self.get_de_context()
            .decompress_using_ddict(destination, source, dict)
            .map_err(zstd_safe::get_error_name)
    }
}

// Helper to create a buffer for the compressed data, preallocating enough
// for the compressed size (given the size of the uncompressed data).
#[inline]
fn make_compressed_data_buffer(uncompressed_len: usize) -> Vec<u8> {
    let buffer_len = zstd_safe::compress_bound(uncompressed_len);
    Vec::with_capacity(buffer_len)
}
