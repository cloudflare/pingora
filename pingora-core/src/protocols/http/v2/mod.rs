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

//! HTTP/2 implementation

use crate::{Error, ErrorType::*, OrErr, Result};
use bytes::Bytes;
use h2::SendStream;

pub mod client;
pub mod server;

/// A helper function to write the body of h2 streams.
pub async fn write_body(writer: &mut SendStream<Bytes>, data: Bytes, end: bool) -> Result<()> {
    let mut remaining = data;

    // Cannot poll 0 capacity, so send it directly.
    if remaining.is_empty() {
        writer
            .send_data(remaining, end)
            .or_err(WriteError, "while writing h2 request body")?;
        return Ok(());
    }

    loop {
        writer.reserve_capacity(remaining.len());
        match std::future::poll_fn(|cx| writer.poll_capacity(cx)).await {
            None => return Error::e_explain(H2Error, "cannot reserve capacity"),
            Some(ready) => {
                let n = ready.or_err(H2Error, "while waiting for capacity")?;
                let remaining_size = remaining.len();
                let data_to_send = remaining.split_to(std::cmp::min(remaining_size, n));
                writer
                    .send_data(data_to_send, remaining.is_empty() && end)
                    .or_err(WriteError, "while writing h2 request body")?;
                if remaining.is_empty() {
                    return Ok(());
                }
            }
        }
    }
}
