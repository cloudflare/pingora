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

use std::cell::RefCell;
use thread_local::ThreadLocal;
use zstd_safe::{CCtx, DCtx};

/// Each thread will own its compression and decompression CTXes, and they share a single dict
/// https://facebook.github.io/zstd/zstd_manual.html recommends to reuse ctx per thread

#[derive(Default)]
pub struct Compression {
    com_context: ThreadLocal<RefCell<zstd_safe::CCtx<'static>>>,
    de_context: ThreadLocal<RefCell<zstd_safe::DCtx<'static>>>,
    dict: Vec<u8>,
}

// these codes are inspired by zstd crate

impl Compression {
    pub fn new() -> Self {
        Compression {
            com_context: ThreadLocal::new(),
            de_context: ThreadLocal::new(),
            dict: vec![],
        }
    }
    pub fn with_dict(dict: Vec<u8>) -> Self {
        Compression {
            com_context: ThreadLocal::new(),
            de_context: ThreadLocal::new(),
            dict,
        }
    }

    pub fn compress_to_buffer<C: zstd_safe::WriteBuf + ?Sized>(
        &self,
        source: &[u8],
        destination: &mut C,
        level: i32,
    ) -> Result<usize, &'static str> {
        self.com_context
            .get_or(|| RefCell::new(CCtx::create()))
            .borrow_mut()
            .compress_using_dict(destination, source, &self.dict[..], level)
            .map_err(zstd_safe::get_error_name)
    }

    pub fn compress(&self, data: &[u8], level: i32) -> Result<Vec<u8>, &'static str> {
        let buffer_len = zstd_safe::compress_bound(data.len());
        let mut buffer = Vec::with_capacity(buffer_len);

        self.compress_to_buffer(data, &mut buffer, level)?;

        Ok(buffer)
    }

    pub fn decompress_to_buffer<C: zstd_safe::WriteBuf + ?Sized>(
        &self,
        source: &[u8],
        destination: &mut C,
    ) -> Result<usize, &'static str> {
        self.de_context
            .get_or(|| RefCell::new(DCtx::create()))
            .borrow_mut()
            .decompress_using_dict(destination, source, &self.dict)
            .map_err(zstd_safe::get_error_name)
    }
}
