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

use bytes::Bytes;
use pingora_error::Result;
use pingora_http::{RequestHeader, ResponseHeader};
use std::time::Duration;

use super::v1::client::HttpSession as Http1Session;
use super::v2::client::Http2Session;
use crate::protocols::{Digest, SocketAddr, Stream};

/// A type for Http client session. It can be either an Http1 connection or an Http2 stream.
pub enum HttpSession {
    H1(Http1Session),
    H2(Http2Session),
}

impl HttpSession {
    pub fn as_http1(&self) -> Option<&Http1Session> {
        match self {
            Self::H1(s) => Some(s),
            Self::H2(_) => None,
        }
    }

    pub fn as_http2(&self) -> Option<&Http2Session> {
        match self {
            Self::H1(_) => None,
            Self::H2(s) => Some(s),
        }
    }
    /// Write the request header to the server
    /// After the request header is sent. The caller can either start reading the response or
    /// sending request body if any.
    pub async fn write_request_header(&mut self, req: Box<RequestHeader>) -> Result<()> {
        match self {
            HttpSession::H1(h1) => {
                h1.write_request_header(req).await?;
                Ok(())
            }
            HttpSession::H2(h2) => h2.write_request_header(req, false),
        }
    }

    /// Write a chunk of the request body.
    pub async fn write_request_body(&mut self, data: Bytes, end: bool) -> Result<()> {
        match self {
            HttpSession::H1(h1) => {
                // TODO: maybe h1 should also have the concept of `end`
                h1.write_body(&data).await?;
                Ok(())
            }
            HttpSession::H2(h2) => h2.write_request_body(data, end),
        }
    }

    /// Signal that the request body has ended
    pub async fn finish_request_body(&mut self) -> Result<()> {
        match self {
            HttpSession::H1(h1) => {
                h1.finish_body().await?;
                Ok(())
            }
            HttpSession::H2(h2) => h2.finish_request_body(),
        }
    }

    /// Set the read timeout for reading header and body.
    ///
    /// The timeout is per read operation, not on the overall time reading the entire response
    pub fn set_read_timeout(&mut self, timeout: Duration) {
        match self {
            HttpSession::H1(h1) => h1.read_timeout = Some(timeout),
            HttpSession::H2(h2) => h2.read_timeout = Some(timeout),
        }
    }

    /// Set the write timeout for writing header and body.
    ///
    /// The timeout is per write operation, not on the overall time writing the entire request.
    ///
    /// This is a noop for h2.
    pub fn set_write_timeout(&mut self, timeout: Duration) {
        match self {
            HttpSession::H1(h1) => h1.write_timeout = Some(timeout),
            HttpSession::H2(_) => { /* no write timeout because the actual write happens async*/ }
        }
    }

    /// Read the response header from the server
    /// For http1, this function can be called multiple times, if the headers received are just
    /// informational headers.
    pub async fn read_response_header(&mut self) -> Result<()> {
        match self {
            HttpSession::H1(h1) => {
                h1.read_response().await?;
                Ok(())
            }
            HttpSession::H2(h2) => h2.read_response_header().await,
        }
    }

    /// Read response body
    ///
    /// `None` when no more body to read.
    pub async fn read_response_body(&mut self) -> Result<Option<Bytes>> {
        match self {
            HttpSession::H1(h1) => h1.read_body_bytes().await,
            HttpSession::H2(h2) => h2.read_response_body().await,
        }
    }

    /// No (more) body to read
    pub fn response_done(&mut self) -> bool {
        match self {
            HttpSession::H1(h1) => h1.is_body_done(),
            HttpSession::H2(h2) => h2.response_finished(),
        }
    }

    /// Give up the http session abruptly.
    /// For H1 this will close the underlying connection
    /// For H2 this will send RST_STREAM frame to end this stream if the stream has not ended at all
    pub async fn shutdown(&mut self) {
        match self {
            Self::H1(s) => s.shutdown().await,
            Self::H2(s) => s.shutdown(),
        }
    }

    /// Get the response header of the server
    ///
    /// `None` if the response header is not read yet.
    pub fn response_header(&self) -> Option<&ResponseHeader> {
        match self {
            Self::H1(s) => s.resp_header(),
            Self::H2(s) => s.response_header(),
        }
    }

    /// Return the [Digest] of the connection
    ///
    /// For reused connection, the timing in the digest will reflect its initial handshakes
    /// The caller should check if the connection is reused to avoid misuse of the timing field.
    pub fn digest(&self) -> Option<&Digest> {
        match self {
            Self::H1(s) => Some(s.digest()),
            Self::H2(s) => s.digest(),
        }
    }

    /// Return a mutable [Digest] reference for the connection, see [`digest`] for more details.
    ///
    /// Will return `None` if this is an H2 session and multiple streams are open.
    pub fn digest_mut(&mut self) -> Option<&mut Digest> {
        match self {
            Self::H1(s) => Some(s.digest_mut()),
            Self::H2(s) => s.digest_mut(),
        }
    }

    /// Return the server (peer) address of the connection.
    pub fn server_addr(&self) -> Option<&SocketAddr> {
        match self {
            Self::H1(s) => s.server_addr(),
            Self::H2(s) => s.server_addr(),
        }
    }

    /// Return the client (local) address of the connection.
    pub fn client_addr(&self) -> Option<&SocketAddr> {
        match self {
            Self::H1(s) => s.client_addr(),
            Self::H2(s) => s.client_addr(),
        }
    }

    /// Get the reference of the [Stream] that this HTTP/1 session is operating upon.
    /// None if the HTTP session is over H2
    pub fn stream(&self) -> Option<&Stream> {
        match self {
            Self::H1(s) => Some(s.stream()),
            Self::H2(_) => None,
        }
    }
}
