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

//! HTTP server session APIs

use super::error_resp;
use super::v1::server::HttpSession as SessionV1;
use super::v2::server::HttpSession as SessionV2;
use super::HttpTask;
use crate::protocols::{Digest, SocketAddr, Stream};
use bytes::Bytes;
use http::HeaderValue;
use http::{header::AsHeaderName, HeaderMap};
use log::error;
use pingora_error::Result;
use pingora_http::{RequestHeader, ResponseHeader};
use std::time::Duration;

/// HTTP server session object for both HTTP/1.x and HTTP/2
pub enum Session {
    H1(SessionV1),
    H2(SessionV2),
}

impl Session {
    /// Create a new [`Session`] from an established connection for HTTP/1.x
    pub fn new_http1(stream: Stream) -> Self {
        Self::H1(SessionV1::new(stream))
    }

    /// Create a new [`Session`] from an established HTTP/2 stream
    pub fn new_http2(session: SessionV2) -> Self {
        Self::H2(session)
    }

    /// Whether the session is HTTP/2. If not it is HTTP/1.x
    pub fn is_http2(&self) -> bool {
        matches!(self, Self::H2(_))
    }

    /// Read the request header. This method is required to be called first before doing anything
    /// else with the session.
    /// - `Ok(true)`: successful
    /// - `Ok(false)`: client exit without sending any bytes. This is normal on reused connection.
    ///   In this case the user should give up this session.
    pub async fn read_request(&mut self) -> Result<bool> {
        match self {
            Self::H1(s) => {
                let read = s.read_request().await?;
                Ok(read.is_some())
            }
            // This call will always return `Ok(true)` for Http2 because the request is already read
            Self::H2(_) => Ok(true),
        }
    }

    /// Return the request header it just read.
    /// # Panic
    /// This function will panic if [`Self::read_request()`] is not called.
    pub fn req_header(&self) -> &RequestHeader {
        match self {
            Self::H1(s) => s.req_header(),
            Self::H2(s) => s.req_header(),
        }
    }

    /// Return a mutable reference to request header it just read.
    /// # Panic
    /// This function will panic if [`Self::read_request()`] is not called.
    pub fn req_header_mut(&mut self) -> &mut RequestHeader {
        match self {
            Self::H1(s) => s.req_header_mut(),
            Self::H2(s) => s.req_header_mut(),
        }
    }

    /// Return the header by name. None if the header doesn't exist.
    ///
    /// In case there are multiple headers under the same name, the first one will be returned. To
    /// get all the headers: use `self.req_header().headers.get_all()`.
    pub fn get_header<K: AsHeaderName>(&self, key: K) -> Option<&HeaderValue> {
        self.req_header().headers.get(key)
    }

    /// Get the header value in its raw format.
    /// If the header doesn't exist, return an empty slice.
    pub fn get_header_bytes<K: AsHeaderName>(&self, key: K) -> &[u8] {
        self.get_header(key).map_or(b"", |v| v.as_bytes())
    }

    /// Read the request body. Ok(None) if no (more) body to read
    pub async fn read_request_body(&mut self) -> Result<Option<Bytes>> {
        match self {
            Self::H1(s) => s.read_body_bytes().await,
            Self::H2(s) => s.read_body_bytes().await,
        }
    }

    /// Write the response header to client
    /// Informational headers (status code 100-199, excluding 101) can be written multiple times the final
    /// response header (status code 200+ or 101) is written.
    pub async fn write_response_header(&mut self, resp: Box<ResponseHeader>) -> Result<()> {
        match self {
            Self::H1(s) => {
                s.write_response_header(resp).await?;
                Ok(())
            }
            Self::H2(s) => s.write_response_header(resp, false),
        }
    }

    /// Similar to `write_response_header()`, this fn will clone the `resp` internally
    pub async fn write_response_header_ref(&mut self, resp: &ResponseHeader) -> Result<()> {
        match self {
            Self::H1(s) => {
                s.write_response_header_ref(resp).await?;
                Ok(())
            }
            Self::H2(s) => s.write_response_header_ref(resp, false),
        }
    }

    /// Write the response body to client
    pub async fn write_response_body(&mut self, data: Bytes, end: bool) -> Result<()> {
        if data.is_empty() && !end {
            // writing 0 byte to a chunked encoding h1 would finish the stream
            // writing 0 bytes to h2 is noop
            // we don't want to actually write in either cases
            return Ok(());
        }
        match self {
            Self::H1(s) => {
                s.write_body(&data).await?;
                Ok(())
            }
            Self::H2(s) => s.write_body(data, end),
        }
    }

    /// Write the response trailers to client
    pub async fn write_response_trailers(&mut self, trailers: HeaderMap) -> Result<()> {
        match self {
            Self::H1(_) => Ok(()), // TODO: support trailers for h1
            Self::H2(s) => s.write_trailers(trailers),
        }
    }

    /// Finish the life of this request.
    /// For H1, if connection reuse is supported, a Some(Stream) will be returned, otherwise None.
    /// For H2, always return None because H2 stream is not reusable.
    pub async fn finish(self) -> Result<Option<Stream>> {
        match self {
            Self::H1(mut s) => {
                // need to flush body due to buffering
                s.finish_body().await?;
                Ok(s.reuse().await)
            }
            Self::H2(mut s) => {
                s.finish()?;
                Ok(None)
            }
        }
    }

    pub async fn response_duplex_vec(&mut self, tasks: Vec<HttpTask>) -> Result<bool> {
        match self {
            Self::H1(s) => s.response_duplex_vec(tasks).await,
            Self::H2(s) => s.response_duplex_vec(tasks),
        }
    }

    /// Set connection reuse. `duration` defines how long the connection is kept open for the next
    /// request to reuse. Noop for h2
    pub fn set_keepalive(&mut self, duration: Option<u64>) {
        match self {
            Self::H1(s) => s.set_server_keepalive(duration),
            Self::H2(_) => {}
        }
    }

    /// Sets the downstream write timeout. This will trigger if we're unable
    /// to write to the stream after `duration`. If a `min_send_rate` is
    /// configured then the `min_send_rate` calculated timeout has higher priority.
    ///
    /// This is a noop for h2.
    pub fn set_write_timeout(&mut self, timeout: Duration) {
        match self {
            Self::H1(s) => s.set_write_timeout(timeout),
            Self::H2(_) => {}
        }
    }

    /// Sets the minimum downstream send rate in bytes per second. This
    /// is used to calculate a write timeout in seconds based on the size
    /// of the buffer being written. If a `min_send_rate` is configured it
    /// has higher priority over a set `write_timeout`. The minimum send
    /// rate must be greater than zero.
    ///
    /// Calculated write timeout is guaranteed to be at least 1s if `min_send_rate`
    /// is greater than zero, a send rate of zero is a noop.
    ///
    /// This is a noop for h2.
    pub fn set_min_send_rate(&mut self, rate: usize) {
        match self {
            Self::H1(s) => s.set_min_send_rate(rate),
            Self::H2(_) => {}
        }
    }

    /// Sets whether we ignore writing informational responses downstream.
    ///
    /// For HTTP/1.1 this is a noop if the response is Upgrade or Continue and
    /// Expect: 100-continue was set on the request.
    ///
    /// This is a noop for h2 because informational responses are always ignored.
    pub fn set_ignore_info_resp(&mut self, ignore: bool) {
        match self {
            Self::H1(s) => s.set_ignore_info_resp(ignore),
            Self::H2(_) => {} // always ignored
        }
    }

    /// Return a digest of the request including the method, path and Host header
    // TODO: make this use a `Formatter`
    pub fn request_summary(&self) -> String {
        match self {
            Self::H1(s) => s.request_summary(),
            Self::H2(s) => s.request_summary(),
        }
    }

    /// Return the written response header. `None` if it is not written yet.
    /// Only the final (status code >= 200 or 101) response header will be returned
    pub fn response_written(&self) -> Option<&ResponseHeader> {
        match self {
            Self::H1(s) => s.response_written(),
            Self::H2(s) => s.response_written(),
        }
    }

    /// Give up the http session abruptly.
    /// For H1 this will close the underlying connection
    /// For H2 this will send RESET frame to end this stream without impacting the connection
    pub async fn shutdown(&mut self) {
        match self {
            Self::H1(s) => s.shutdown().await,
            Self::H2(s) => s.shutdown(),
        }
    }

    pub fn to_h1_raw(&self) -> Bytes {
        match self {
            Self::H1(s) => s.get_headers_raw_bytes(),
            Self::H2(s) => s.pseudo_raw_h1_request_header(),
        }
    }

    /// Whether the whole request body is sent
    pub fn is_body_done(&mut self) -> bool {
        match self {
            Self::H1(s) => s.is_body_done(),
            Self::H2(s) => s.is_body_done(),
        }
    }

    /// Notify the client that the entire body is sent
    /// for H1 chunked encoding, this will end the last empty chunk
    /// for H1 content-length, this has no effect.
    /// for H2, this will send an empty DATA frame with END_STREAM flag
    pub async fn finish_body(&mut self) -> Result<()> {
        match self {
            Self::H1(s) => s.finish_body().await.map(|_| ()),
            Self::H2(s) => s.finish(),
        }
    }

    pub fn generate_error(error: u16) -> ResponseHeader {
        match error {
            /* common error responses are pre-generated */
            502 => error_resp::HTTP_502_RESPONSE.clone(),
            400 => error_resp::HTTP_400_RESPONSE.clone(),
            _ => error_resp::gen_error_response(error),
        }
    }

    /// Send error response to client
    pub async fn respond_error(&mut self, error: u16) {
        let resp = Self::generate_error(error);

        // TODO: we shouldn't be closing downstream connections on internally generated errors
        // and possibly other upstream connect() errors (connection refused, timeout, etc)
        //
        // This change is only here because we DO NOT re-use downstream connections
        // today on these errors and we should signal to the client that pingora is dropping it
        // rather than a misleading the client with 'keep-alive'
        self.set_keepalive(None);

        self.write_response_header(Box::new(resp))
            .await
            .unwrap_or_else(|e| {
                error!("failed to send error response to downstream: {e}");
            });
    }

    /// Whether there is no request body
    pub fn is_body_empty(&mut self) -> bool {
        match self {
            Self::H1(s) => s.is_body_empty(),
            Self::H2(s) => s.is_body_empty(),
        }
    }

    pub fn retry_buffer_truncated(&self) -> bool {
        match self {
            Self::H1(s) => s.retry_buffer_truncated(),
            Self::H2(s) => s.retry_buffer_truncated(),
        }
    }

    pub fn enable_retry_buffering(&mut self) {
        match self {
            Self::H1(s) => s.enable_retry_buffering(),
            Self::H2(s) => s.enable_retry_buffering(),
        }
    }

    pub fn get_retry_buffer(&self) -> Option<Bytes> {
        match self {
            Self::H1(s) => s.get_retry_buffer(),
            Self::H2(s) => s.get_retry_buffer(),
        }
    }

    /// Read body (same as `read_request_body()`) or pending forever until downstream
    /// terminates the session.
    pub async fn read_body_or_idle(&mut self, no_body_expected: bool) -> Result<Option<Bytes>> {
        match self {
            Self::H1(s) => s.read_body_or_idle(no_body_expected).await,
            Self::H2(s) => s.read_body_or_idle(no_body_expected).await,
        }
    }

    pub fn as_http1(&self) -> Option<&SessionV1> {
        match self {
            Self::H1(s) => Some(s),
            Self::H2(_) => None,
        }
    }

    pub fn as_http2(&self) -> Option<&SessionV2> {
        match self {
            Self::H1(_) => None,
            Self::H2(s) => Some(s),
        }
    }

    /// Write a 100 Continue response to the client.
    pub async fn write_continue_response(&mut self) -> Result<()> {
        match self {
            Self::H1(s) => s.write_continue_response().await,
            Self::H2(s) => s.write_response_header(
                Box::new(ResponseHeader::build(100, Some(0)).unwrap()),
                false,
            ),
        }
    }

    /// Whether this request is for upgrade (e.g., websocket)
    pub fn is_upgrade_req(&self) -> bool {
        match self {
            Self::H1(s) => s.is_upgrade_req(),
            Self::H2(_) => false,
        }
    }

    /// Return how many response body bytes (application, not wire) already sent downstream
    pub fn body_bytes_sent(&self) -> usize {
        match self {
            Self::H1(s) => s.body_bytes_sent(),
            Self::H2(s) => s.body_bytes_sent(),
        }
    }

    /// Return how many request body bytes (application, not wire) already read from downstream
    pub fn body_bytes_read(&self) -> usize {
        match self {
            Self::H1(s) => s.body_bytes_read(),
            Self::H2(s) => s.body_bytes_read(),
        }
    }

    /// Return the [Digest] for the connection.
    pub fn digest(&self) -> Option<&Digest> {
        match self {
            Self::H1(s) => Some(s.digest()),
            Self::H2(s) => s.digest(),
        }
    }

    /// Return a mutable [Digest] reference for the connection.
    ///
    /// Will return `None` if multiple H2 streams are open.
    pub fn digest_mut(&mut self) -> Option<&mut Digest> {
        match self {
            Self::H1(s) => Some(s.digest_mut()),
            Self::H2(s) => s.digest_mut(),
        }
    }

    /// Return the client (peer) address of the connection.
    pub fn client_addr(&self) -> Option<&SocketAddr> {
        match self {
            Self::H1(s) => s.client_addr(),
            Self::H2(s) => s.client_addr(),
        }
    }

    /// Return the server (local) address of the connection.
    pub fn server_addr(&self) -> Option<&SocketAddr> {
        match self {
            Self::H1(s) => s.server_addr(),
            Self::H2(s) => s.server_addr(),
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
