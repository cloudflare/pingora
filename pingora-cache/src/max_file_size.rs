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

use crate::storage::{HandleMiss, MissFinishType};
use crate::MissHandler;
use async_trait::async_trait;
use bytes::Bytes;
use pingora_error::{Error, ErrorType};

/// [MaxFileSizeMissHandler] wraps a MissHandler to enforce a maximum asset size that should be
/// written to the MissHandler.
///
/// This is used to enforce a maximum cache size for a request when the
/// response size is not known ahead of time (no Content-Length header). When the response size _is_
/// known ahead of time, it should be checked up front (when calculating cacheability) for efficiency.
/// Note: for requests with partial read support (where downstream reads the response from cache as
/// it is filled), this will cause the request as a whole to fail. The response will be remembered
/// as uncacheable, though, so downstream will be able to retry the request, since the cache will be
/// disabled for the retried request.
pub struct MaxFileSizeMissHandler {
    inner: MissHandler,
    max_file_size_bytes: usize,
    bytes_written: usize,
}

impl MaxFileSizeMissHandler {
    /// Create a new [MaxFileSizeMissHandler] wrapping the given [MissHandler]
    pub fn new(inner: MissHandler, max_file_size_bytes: usize) -> MaxFileSizeMissHandler {
        MaxFileSizeMissHandler {
            inner,
            max_file_size_bytes,
            bytes_written: 0,
        }
    }
}

/// Error type returned when the limit is reached.
pub const ERR_RESPONSE_TOO_LARGE: ErrorType = ErrorType::Custom("response too large");

#[async_trait]
impl HandleMiss for MaxFileSizeMissHandler {
    async fn write_body(&mut self, data: Bytes, eof: bool) -> pingora_error::Result<()> {
        // fail if writing the body would exceed the max_file_size_bytes
        if self.bytes_written + data.len() > self.max_file_size_bytes {
            return Error::e_explain(
                ERR_RESPONSE_TOO_LARGE,
                format!(
                    "writing data of size {} bytes would exceed max file size of {} bytes",
                    data.len(),
                    self.max_file_size_bytes
                ),
            );
        }

        self.bytes_written += data.len();
        self.inner.write_body(data, eof).await
    }

    async fn finish(self: Box<Self>) -> pingora_error::Result<MissFinishType> {
        self.inner.finish().await
    }

    fn streaming_write_tag(&self) -> Option<&[u8]> {
        self.inner.streaming_write_tag()
    }
}
