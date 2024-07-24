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

//! HTTP/2 server session

use bytes::Bytes;
use futures::Future;
use h2::server;
use h2::server::SendResponse;
use h2::{RecvStream, SendStream};
use http::header::HeaderName;
use http::{header, HeaderMap, Response};
use log::{debug, warn};
use pingora_http::{RequestHeader, ResponseHeader};
use std::sync::Arc;

use crate::protocols::http::body_buffer::FixedBuffer;
use crate::protocols::http::date::get_cached_date;
use crate::protocols::http::v1::client::http_req_header_to_wire;
use crate::protocols::http::HttpTask;
use crate::protocols::{Digest, SocketAddr, Stream};
use crate::{Error, ErrorType, OrErr, Result};

const BODY_BUF_LIMIT: usize = 1024 * 64;

type H2Connection<S> = server::Connection<S, Bytes>;

pub use h2::server::Builder as H2Options;

/// Perform HTTP/2 connection handshake with an established (TLS) connection.
///
/// The optional `options` allow to adjust certain HTTP/2 parameters and settings.
/// See [`H2Options`] for more details.
pub async fn handshake(io: Stream, options: Option<H2Options>) -> Result<H2Connection<Stream>> {
    let options = options.unwrap_or_default();
    let res = options.handshake(io).await;
    match res {
        Ok(connection) => {
            debug!("H2 handshake done.");
            Ok(connection)
        }
        Err(e) => Error::e_because(
            ErrorType::HandshakeError,
            "while h2 handshaking with client",
            e,
        ),
    }
}

use futures::task::Context;
use futures::task::Poll;
use std::pin::Pin;
/// The future to poll for an idle session.
///
/// Calling `.await` in this object will not return until the client decides to close this stream.
pub struct Idle<'a>(&'a mut HttpSession);

impl<'a> Future for Idle<'a> {
    type Output = Result<h2::Reason>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if let Some(body_writer) = self.0.send_response_body.as_mut() {
            body_writer.poll_reset(cx)
        } else {
            self.0.send_response.poll_reset(cx)
        }
        .map_err(|e| Error::because(ErrorType::H2Error, "downstream error while idling", e))
    }
}

/// HTTP/2 server session
pub struct HttpSession {
    request_header: RequestHeader,
    request_body_reader: RecvStream,
    send_response: SendResponse<Bytes>,
    send_response_body: Option<SendStream<Bytes>>,
    // Remember what has been written
    response_written: Option<Box<ResponseHeader>>,
    // Indicate that whether a END_STREAM is already sent
    // in order to tell whether needs to send one extra FRAME when this response finishes
    ended: bool,
    // How many (application, not wire) request body bytes have been read so far.
    body_read: usize,
    // How many (application, not wire) response body bytes have been sent so far.
    body_sent: usize,
    // buffered request body for retry logic
    retry_buffer: Option<FixedBuffer>,
    // digest to record underlying connection info
    digest: Arc<Digest>,
}

impl HttpSession {
    /// Create a new [`HttpSession`] from the HTTP/2 connection.
    /// This function returns a new HTTP/2 session when the provided HTTP/2 connection, `conn`,
    /// establishes a new HTTP/2 stream to this server.
    ///
    /// A [`Digest`] from the IO stream is also stored in the resulting session, since the
    /// session doesn't have access to the underlying stream (and the stream itself isn't
    /// accessible from the `h2::server::Connection`).
    ///
    /// Note: in order to handle all **existing** and new HTTP/2 sessions, the server must call
    /// this function in a loop until the client decides to close the connection.
    ///
    /// `None` will be returned when the connection is closing so that the loop can exit.
    ///
    pub async fn from_h2_conn(
        conn: &mut H2Connection<Stream>,
        digest: Arc<Digest>,
    ) -> Result<Option<Self>> {
        // NOTE: conn.accept().await is what drives the entire connection.
        let res = conn.accept().await.transpose().or_err(
            ErrorType::H2Error,
            "while accepting new downstream requests",
        )?;

        Ok(res.map(|(req, send_response)| {
            let (request_header, request_body_reader) = req.into_parts();
            HttpSession {
                request_header: request_header.into(),
                request_body_reader,
                send_response,
                send_response_body: None,
                response_written: None,
                ended: false,
                body_read: 0,
                body_sent: 0,
                retry_buffer: None,
                digest,
            }
        }))
    }

    /// The request sent from the client
    ///
    /// Different from its HTTP/1.X counterpart, this function never panics as the request is already
    /// read when established a new HTTP/2 stream.
    pub fn req_header(&self) -> &RequestHeader {
        &self.request_header
    }

    /// A mutable reference to request sent from the client
    ///
    /// Different from its HTTP/1.X counterpart, this function never panics as the request is already
    /// read when established a new HTTP/2 stream.
    pub fn req_header_mut(&mut self) -> &mut RequestHeader {
        &mut self.request_header
    }

    /// Read request body bytes. `None` when there is no more body to read.
    pub async fn read_body_bytes(&mut self) -> Result<Option<Bytes>> {
        // TODO: timeout
        let data = self.request_body_reader.data().await.transpose().or_err(
            ErrorType::ReadError,
            "while reading downstream request body",
        )?;
        if let Some(data) = data.as_ref() {
            self.body_read += data.len();
            if let Some(buffer) = self.retry_buffer.as_mut() {
                buffer.write_to_buffer(data);
            }
            let _ = self
                .request_body_reader
                .flow_control()
                .release_capacity(data.len());
        }
        Ok(data)
    }

    // the write_* don't have timeouts because the actual writing happens on the connection
    // not here.

    /// Write the response header to the client.
    /// # the `end` flag
    /// `end` marks the end of this session.
    /// If the `end` flag is set, no more header or body can be sent to the client.
    pub fn write_response_header(
        &mut self,
        mut header: Box<ResponseHeader>,
        end: bool,
    ) -> Result<()> {
        if self.ended {
            // TODO: error or warn?
            return Ok(());
        }

        if header.status.is_informational() {
            // ignore informational response 1xx header because send_response() can only be called once
            // https://github.com/hyperium/h2/issues/167
            debug!("ignoring informational headers");
            return Ok(());
        }

        if self.response_written.as_ref().is_some() {
            warn!("Response header is already sent, cannot send again");
            return Ok(());
        }

        /* update headers */
        header.insert_header(header::DATE, get_cached_date())?;

        // remove other h1 hop headers that cannot be present in H2
        // https://httpwg.org/specs/rfc7540.html#n-connection-specific-header-fields
        header.remove_header(&header::TRANSFER_ENCODING);
        header.remove_header(&header::CONNECTION);
        header.remove_header(&header::UPGRADE);
        header.remove_header(&HeaderName::from_static("keep-alive"));
        header.remove_header(&HeaderName::from_static("proxy-connection"));

        let resp = Response::from_parts(header.as_owned_parts(), ());

        let body_writer = self.send_response.send_response(resp, end).or_err(
            ErrorType::WriteError,
            "while writing h2 response to downstream",
        )?;

        self.response_written = Some(header);
        self.send_response_body = Some(body_writer);
        self.ended = self.ended || end;
        Ok(())
    }

    /// Write response body to the client. See [Self::write_response_header] for how to use `end`.
    pub fn write_body(&mut self, data: Bytes, end: bool) -> Result<()> {
        if self.ended {
            // NOTE: in h1, we also track to see if content-length matches the data
            // We have not tracked that in h2
            warn!("Try to write body after end of stream, dropping the extra data");
            return Ok(());
        }
        let Some(writer) = self.send_response_body.as_mut() else {
            return Err(Error::explain(
                ErrorType::H2Error,
                "try to send body before header is sent",
            ));
        };
        let data_len = data.len();
        writer.reserve_capacity(data_len);
        writer.send_data(data, end).or_err(
            ErrorType::WriteError,
            "while writing h2 response body to downstream",
        )?;
        self.body_sent += data_len;
        self.ended = self.ended || end;
        Ok(())
    }

    /// Write response trailers to the client, this also closes the stream.
    pub fn write_trailers(&mut self, trailers: HeaderMap) -> Result<()> {
        if self.ended {
            warn!("Tried to write trailers after end of stream, dropping them");
            return Ok(());
        }
        let Some(writer) = self.send_response_body.as_mut() else {
            return Err(Error::explain(
                ErrorType::H2Error,
                "try to send trailers before header is sent",
            ));
        };
        writer.send_trailers(trailers).or_err(
            ErrorType::WriteError,
            "while writing h2 response trailers to downstream",
        )?;
        // sending trailers closes the stream
        self.ended = true;
        Ok(())
    }

    /// Similar to [Self::write_response_header], this function takes a reference instead
    pub fn write_response_header_ref(&mut self, header: &ResponseHeader, end: bool) -> Result<()> {
        self.write_response_header(Box::new(header.clone()), end)
    }

    // TODO: trailer

    /// Mark the session end. If no `end` flag is already set before this call, this call will
    /// signal the client. Otherwise this call does nothing.
    ///
    /// Dropping this object without sending `end` will cause an error to the client, which will cause
    /// the client to treat this session as bad or incomplete.
    pub fn finish(&mut self) -> Result<()> {
        if self.ended {
            // already ended the stream
            return Ok(());
        }
        if let Some(writer) = self.send_response_body.as_mut() {
            // use an empty data frame to signal the end
            writer.send_data("".into(), true).or_err(
                ErrorType::WriteError,
                "while writing h2 response body to downstream",
            )?;
            self.ended = true;
        };
        // else: the response header is not sent, do nothing now.
        // When send_response_body is dropped, an RST_STREAM will be sent

        Ok(())
    }

    pub fn response_duplex_vec(&mut self, tasks: Vec<HttpTask>) -> Result<bool> {
        let mut end_stream = false;
        for task in tasks.into_iter() {
            end_stream = match task {
                HttpTask::Header(header, end) => {
                    self.write_response_header(header, end)
                        .map_err(|e| e.into_down())?;
                    end
                }
                HttpTask::Body(data, end) => match data {
                    Some(d) => {
                        if !d.is_empty() {
                            self.write_body(d, end).map_err(|e| e.into_down())?;
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
            self.finish().map_err(|e| e.into_down())?;
        }
        Ok(end_stream)
    }

    /// Return a string `$METHOD $PATH $HOST`. Mostly for logging and debug purpose
    pub fn request_summary(&self) -> String {
        format!(
            "{} {}, Host: {}",
            self.request_header.method,
            self.request_header.uri,
            self.request_header
                .headers
                .get(header::HOST)
                .map(|v| String::from_utf8_lossy(v.as_bytes()))
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
        if !self.ended {
            self.send_response.send_reset(h2::Reason::INTERNAL_ERROR);
        }
    }

    // This is a hack for pingora-proxy to create subrequests from h2 server session
    // TODO: be able to convert from h2 to h1 subrequest
    pub fn pseudo_raw_h1_request_header(&self) -> Bytes {
        let buf = http_req_header_to_wire(&self.request_header).unwrap(); // safe, None only when version unknown
        buf.freeze()
    }

    /// Whether there is no more body to read
    pub fn is_body_done(&self) -> bool {
        self.request_body_reader.is_end_stream()
    }

    /// Whether there is any body to read.
    pub fn is_body_empty(&self) -> bool {
        self.body_read == 0
            && (self.is_body_done()
                || self
                    .request_header
                    .headers
                    .get(header::CONTENT_LENGTH)
                    .map_or(false, |cl| cl.as_bytes() == b"0"))
    }

    pub fn retry_buffer_truncated(&self) -> bool {
        self.retry_buffer
            .as_ref()
            .map_or_else(|| false, |r| r.is_truncated())
    }

    pub fn enable_retry_buffering(&mut self) {
        if self.retry_buffer.is_none() {
            self.retry_buffer = Some(FixedBuffer::new(BODY_BUF_LIMIT))
        }
    }

    pub fn get_retry_buffer(&self) -> Option<Bytes> {
        self.retry_buffer.as_ref().and_then(|b| {
            if b.is_truncated() {
                None
            } else {
                b.get_buffer()
            }
        })
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
        if no_body_expected || self.is_body_done() {
            let reason = self.idle().await?;
            Error::e_explain(
                ErrorType::H2Error,
                format!("Client closed H2, reason: {reason}"),
            )
        } else {
            self.read_body_bytes().await
        }
    }

    /// Return how many response body bytes (application, not wire) already sent downstream
    pub fn body_bytes_sent(&self) -> usize {
        self.body_sent
    }

    /// Return how many request body bytes (application, not wire) already read from downstream
    pub fn body_bytes_read(&self) -> usize {
        self.body_read
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

#[cfg(test)]
mod test {
    use super::*;
    use http::{HeaderValue, Method, Request};
    use tokio::io::duplex;

    #[tokio::test]
    async fn test_server_handshake_accept_request() {
        let (client, server) = duplex(65536);
        let client_body = "test client body";
        let server_body = "test server body";

        let mut expected_trailers = HeaderMap::new();
        expected_trailers.insert("test", HeaderValue::from_static("trailers"));
        let trailers = expected_trailers.clone();

        let mut handles = vec![];
        handles.push(tokio::spawn(async move {
            let (h2, connection) = h2::client::handshake(client).await.unwrap();
            tokio::spawn(async move {
                connection.await.unwrap();
            });

            let mut h2 = h2.ready().await.unwrap();

            let request = Request::builder()
                .method(Method::GET)
                .uri("https://www.example.com/")
                .body(())
                .unwrap();

            let (response, mut req_body) = h2.send_request(request, false).unwrap();
            req_body.reserve_capacity(client_body.len());
            req_body.send_data(client_body.into(), true).unwrap();

            let (head, mut body) = response.await.unwrap().into_parts();
            assert_eq!(head.status, 200);
            let data = body.data().await.unwrap().unwrap();
            assert_eq!(data, server_body);
            let resp_trailers = body.trailers().await.unwrap().unwrap();
            assert_eq!(resp_trailers, expected_trailers);
        }));

        let mut connection = handshake(Box::new(server), None).await.unwrap();
        let digest = Arc::new(Digest::default());

        while let Some(mut http) = HttpSession::from_h2_conn(&mut connection, digest.clone())
            .await
            .unwrap()
        {
            let trailers = trailers.clone();
            handles.push(tokio::spawn(async move {
                let req = http.req_header();
                assert_eq!(req.method, Method::GET);
                assert_eq!(req.uri, "https://www.example.com/");

                http.enable_retry_buffering();

                assert!(!http.is_body_empty());
                assert!(!http.is_body_done());

                let body = http.read_body_or_idle(false).await.unwrap().unwrap();
                assert_eq!(body, client_body);
                assert!(http.is_body_done());
                assert_eq!(http.body_bytes_read(), 16);

                let retry_body = http.get_retry_buffer().unwrap();
                assert_eq!(retry_body, client_body);

                // test idling before response header is sent
                tokio::select! {
                    _ = http.idle() => {panic!("downstream should be idling")},
                    _= tokio::time::sleep(tokio::time::Duration::from_secs(1)) => {}
                }

                let response_header = Box::new(ResponseHeader::build(200, None).unwrap());
                assert!(http
                    .write_response_header(response_header.clone(), false)
                    .is_ok());
                // this write should be ignored otherwise we will error
                assert!(http.write_response_header(response_header, false).is_ok());

                // test idling after response header is sent
                tokio::select! {
                    _ = http.read_body_or_idle(false) => {panic!("downstream should be idling")},
                    _= tokio::time::sleep(tokio::time::Duration::from_secs(1)) => {}
                }

                // end: false here to verify finish() closes the stream nicely
                http.write_body(server_body.into(), false).unwrap();
                assert_eq!(http.body_bytes_sent(), 16);

                http.write_trailers(trailers).unwrap();
                http.finish().unwrap();
            }));
        }
        for handle in handles {
            // ensure no panics
            assert!(handle.await.is_ok());
        }
    }
}
