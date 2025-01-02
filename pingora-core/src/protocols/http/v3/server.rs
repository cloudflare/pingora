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

//! HTTP/3 server session

use crate::protocols::{Digest, SocketAddr, Stream};
use bytes::Bytes;
use http::uri::PathAndQuery;
use http::HeaderMap;
use pingora_error::Result;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use crate::protocols::http::v1::client::http_req_header_to_wire;
use pingora_http::{RequestHeader, ResponseHeader};

use crate::protocols::http::HttpTask;
pub use quiche::h3::Config as H3Options;

/// Perform HTTP/3 connection handshake with an established (QUIC) connection.
///
/// The optional `options` allow to adjust certain HTTP/3 parameters and settings.
/// See [`H3Options`] for more details.
#[allow(unused)] // TODO: remove
pub async fn handshake(io: Stream, options: Option<&H3Options>) -> Result<H3Connection> {
    Ok(H3Connection { _l4stream: io })
}

pub struct H3Connection {
    _l4stream: Stream, // ensure the stream will not be dropped until all sessions are
}

impl H3Connection {
    pub async fn graceful_shutdown(&mut self) {
        todo!();
    }
    pub fn poll_closed(&mut self, _cx: &mut Context<'_>) -> Poll<Result<()>> {
        todo!();
    }
}

/// HTTP/3 server session
#[allow(unused)] // TODO: remove
pub struct HttpSession {
    request_header: Option<RequestHeader>,
    // Remember what has been written
    response_written: Option<Box<ResponseHeader>>,

    // How many (application, not wire) response body bytes have been sent so far.
    body_sent: usize,

    // track if the FIN STREAM frame was already sent
    // quiche::Connection::stream_send fin argument
    send_ended: bool,

    // digest to record underlying connection info
    digest: Arc<Digest>,
}

#[allow(unused)] // TODO: remove
impl HttpSession {
    /// Create a new [`HttpSession`] from the QUIC connection.
    /// This function returns a new HTTP/3 session when the provided HTTP/3 connection, `conn`,
    /// establishes a new HTTP/3 stream to this server.
    ///
    /// A [`Digest`] from the IO stream is also stored in the resulting session, since the
    /// session doesn't have access to the underlying stream (and the stream itself isn't
    /// accessible from the `h3::server::Connection`).
    ///
    /// Note: in order to handle all **existing** and new HTTP/3 sessions, the server must call
    /// this function in a loop until the client decides to close the connection.
    ///
    /// `None` will be returned when the connection is closing so that the loop can exit.
    ///
    pub async fn from_h3_conn(
        conn: &mut H3Connection,
        digest: Arc<Digest>,
    ) -> Result<Option<Self>> {
        todo!();
    }

    /// The request sent from the client
    ///
    /// Different from its HTTP/1.X counterpart, this function never panics as the request is already
    /// read when established a new HTTP/3 stream.
    pub fn req_header(&self) -> &RequestHeader {
        self.request_header.as_ref().unwrap()
    }

    /// A mutable reference to request sent from the client
    ///
    /// Different from its HTTP/1.X counterpart, this function never panics as the request is already
    /// read when established a new HTTP/3 stream.
    pub fn req_header_mut(&mut self) -> &mut RequestHeader {
        self.request_header.as_mut().unwrap()
    }

    /// Read request body bytes. `None` when there is no more body to read.
    pub async fn read_body_bytes(&mut self) -> Result<Option<Bytes>> {
        todo!();
    }

    // the write_* don't have timeouts because the actual writing happens on the connection
    // not here.

    /// Write the response header to the client.
    /// # the `end` flag
    /// `end` marks the end of this session.
    /// If the `end` flag is set, no more header or body can be sent to the client.
    pub async fn write_response_header(
        &mut self,
        mut header: Box<ResponseHeader>,
        end: bool,
    ) -> Result<()> {
        todo!();
    }

    /// Write response body to the client. See [Self::write_response_header] for how to use `end`.
    pub async fn write_body(&mut self, data: Bytes, end: bool) -> Result<()> {
        todo!();
    }

    /// Write response trailers to the client, this also closes the stream.
    pub fn write_trailers(&mut self, trailers: HeaderMap) -> Result<()> {
        // TODO: use async fn?
        todo!();
    }

    /// Similar to [Self::write_response_header], this function takes a reference instead
    pub async fn write_response_header_ref(
        &mut self,
        header: &ResponseHeader,
        end: bool,
    ) -> Result<()> {
        self.write_response_header(Box::new(header.clone()), end)
            .await
    }

    /// Mark the session end. If no `end` flag is already set before this call, this call will
    /// signal the client. Otherwise this call does nothing.
    ///
    /// Dropping this object without sending `end` will cause an error to the client, which will cause
    /// the client to treat this session as bad or incomplete.
    pub async fn finish(&mut self) -> Result<()> {
        // TODO: check/validate with documentation on protocols::http::server::HttpSession
        // TODO: check/validate trailer sending
        todo!();
    }

    pub async fn response_duplex_vec(&mut self, tasks: Vec<HttpTask>) -> Result<bool> {
        let mut end_stream = false;
        for task in tasks.into_iter() {
            end_stream = match task {
                HttpTask::Header(header, end) => {
                    self.write_response_header(header, end)
                        .await
                        .map_err(|e| e.into_down())?;
                    end
                }
                HttpTask::Body(data, end) => match data {
                    Some(d) => {
                        if !d.is_empty() {
                            self.write_body(d, end).await.map_err(|e| e.into_down())?;
                        }
                        end
                    }
                    None => end,
                },
                HttpTask::Trailer(Some(trailers)) => {
                    self.write_trailers(*trailers)?;
                    true
                }
                HttpTask::Trailer(None) => true,
                HttpTask::Done => true,
                HttpTask::Failed(e) => {
                    return Err(e);
                }
            } || end_stream // safe guard in case `end` in tasks flips from true to false
        }
        if end_stream {
            // no-op if finished already
            self.finish().await.map_err(|e| e.into_down())?;
        }
        Ok(end_stream)
    }

    /// Return a string `$METHOD $PATH, Host: $HOST`. Mostly for logging and debug purpose
    pub fn request_summary(&self) -> String {
        let request_header = self.req_header();
        format!(
            "{} {}, Host: {}:{}",
            request_header.method,
            request_header
                .uri
                .path_and_query()
                .map(PathAndQuery::as_str)
                .unwrap_or_default(),
            request_header.uri.host().unwrap_or_default(),
            request_header
                .uri
                .port()
                .as_ref()
                .map(|port| port.as_str())
                .unwrap_or_default()
        )
    }

    /// Return the written response header. `None` if it is not written yet.
    pub fn response_written(&self) -> Option<&ResponseHeader> {
        self.response_written.as_deref()
    }

    /// Give up the stream abruptly.
    ///
    /// This will send a `INTERNAL_ERROR` stream error to the client
    pub fn shutdown(&mut self) {
        // TODO: check/validate with documentation on protocols::http::server::HttpSession
        // TODO: should this set self.ended? it closes the stream which prevents further writes
        todo!();
    }

    // This is a hack for pingora-proxy to create subrequests from h2 server session
    // TODO: be able to convert from h3 to h1 subrequest
    pub fn pseudo_raw_h1_request_header(&self) -> Bytes {
        let buf = http_req_header_to_wire(self.req_header()).unwrap(); // safe, None only when version unknown
        buf.freeze()
    }

    /// Whether there is no more body to read
    pub fn is_body_done(&self) -> bool {
        todo!();
    }

    /// Whether there is any body to read.
    pub fn is_body_empty(&self) -> bool {
        todo!();
    }

    pub fn retry_buffer_truncated(&self) -> bool {
        todo!();
    }

    pub fn enable_retry_buffering(&mut self) {
        todo!();
    }

    pub fn get_retry_buffer(&self) -> Option<Bytes> {
        todo!();
    }

    /// `async fn idle() -> Result<Reason, Error>;`
    /// This async fn will be pending forever until the client closes the stream/connection
    /// This function is used for watching client status so that the server is able to cancel
    /// its internal tasks as the client waiting for the tasks goes away
    pub fn idle(&mut self) -> Idle {
        Idle(self)
    }

    /// Similar to `read_body_bytes()` but will be pending after Ok(None) is returned,
    /// until the client closes the connection
    pub async fn read_body_or_idle(&mut self, no_body_expected: bool) -> Result<Option<Bytes>> {
        todo!();
    }

    /// Return how many response body bytes (application, not wire) already sent downstream
    pub fn body_bytes_sent(&self) -> usize {
        self.body_sent
    }

    /// Return how many request body bytes (application, not wire) already read from downstream
    pub fn body_bytes_read(&self) -> usize {
        todo!();
    }

    /// Return the [Digest] of the connection.
    pub fn digest(&self) -> Option<&Digest> {
        Some(&self.digest)
    }

    /// Return a mutable [Digest] reference for the connection.
    pub fn digest_mut(&mut self) -> Option<&mut Digest> {
        Arc::get_mut(&mut self.digest)
    }

    /// Return the server (local) address recorded in the connection digest.
    pub fn server_addr(&self) -> Option<&SocketAddr> {
        self.digest.socket_digest.as_ref().map(|d| d.local_addr())?
    }

    /// Return the client (peer) address recorded in the connection digest.
    pub fn client_addr(&self) -> Option<&SocketAddr> {
        self.digest.socket_digest.as_ref().map(|d| d.peer_addr())?
    }
}

/// The future to poll for an idle session.
///
/// Calling `.await` in this object will not return until the client decides to close this stream.
#[allow(unused)] // TODO: remove
pub struct Idle<'a>(&'a mut HttpSession);

#[allow(unused)] // TODO: remove
impl<'a> Future for Idle<'a> {
    type Output = u64;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        todo!();
    }
}
