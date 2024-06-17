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

//! HTTP/1.x and HTTP/2 implementation APIs

mod body_buffer;
pub mod client;
pub mod compression;
pub mod conditional_filter;
pub(crate) mod date;
pub mod error_resp;
pub mod server;
pub mod v1;
pub mod v2;

pub use server::Session as ServerSession;

/// The Pingora server name string
pub const SERVER_NAME: &[u8; 7] = b"Pingora";

/// An enum to hold all possible HTTP response events.
#[derive(Debug)]
pub enum HttpTask {
    /// the response header and the boolean end of response flag
    Header(Box<pingora_http::ResponseHeader>, bool),
    /// A piece of response header and the end of response boolean flag
    Body(Option<bytes::Bytes>, bool),
    /// HTTP response trailer
    Trailer(Option<Box<http::HeaderMap>>),
    /// Signal that the response is already finished
    Done,
    /// Signal that the reading of the response encountered errors.
    Failed(pingora_error::BError),
}

impl HttpTask {
    /// Whether this [`HttpTask`] means the end of the response
    pub fn is_end(&self) -> bool {
        match self {
            HttpTask::Header(_, end) => *end,
            HttpTask::Body(_, end) => *end,
            HttpTask::Trailer(_) => true,
            HttpTask::Done => true,
            HttpTask::Failed(_) => true,
        }
    }
}
