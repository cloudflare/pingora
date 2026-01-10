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

//! # HTTP server session for subrequests
//!
//! This server session is _very_ similar to the implementation for v1, if not
//! identical in many cases. Though in theory subrequests are HTTP version
//! agnostic in reality this means that they must interpret any version-specific
//! idiosyncracies such as Connection: upgrade headers in H1 because they
//! "stand-in" for the actual main Session when running proxy logic. As much as
//! possible they should defer downstream-specific logic to the actual downstream
//! session and act more or less as a pipe.
//!
//! The session also instantiates a [`SubrequestHandle`] that contains necessary
//! communication channels with the subrequest, to make it possible to send
//! and receive data.
//!
//! Its write calls will send `HttpTask`s to the handle channels, instead of
//! flushing to an actual underlying stream.
//!
//! Connection reuse and keep-alive are not supported because there is no
//! actual underlying stream, only transient channels per request.

use bytes::Bytes;
use http::HeaderValue;
use http::{header, header::AsHeaderName, HeaderMap, Method, Version};
use log::{debug, trace, warn};
use pingora_error::{Error, ErrorType::*, OkOrErr, Result};
use pingora_http::{RequestHeader, ResponseHeader};
use pingora_timeout::timeout;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};

use super::body::{BodyReader, BodyWriter};
use crate::protocols::http::{
    body_buffer::FixedBuffer,
    server::Session as GenericHttpSession,
    subrequest::dummy::DummyIO,
    v1::common::{header_value_content_length, is_header_value_chunked_encoding, BODY_BUF_LIMIT},
    v1::server::HttpSession as SessionV1,
    HttpTask,
};
use crate::protocols::{Digest, SocketAddr};

/// The HTTP server session
pub struct HttpSession {
    // these are only options because we allow dropping them separately on shutdown
    tx: Option<mpsc::Sender<HttpTask>>,
    rx: Option<mpsc::Receiver<HttpTask>>,
    // Currently subrequest session is initialized via a dummy SessionV1 only
    // TODO: need to be able to indicate H2 / other HTTP versions here
    v1_inner: Box<SessionV1>,
    read_req_header: bool,
    response_written: Option<ResponseHeader>,
    read_timeout: Option<Duration>,
    write_timeout: Option<Duration>,
    total_drain_timeout: Option<Duration>,
    body_bytes_sent: usize,
    body_bytes_read: usize,
    retry_buffer: Option<FixedBuffer>,
    body_reader: BodyReader,
    body_writer: BodyWriter,
    upgraded: bool,
    // TODO: likely doesn't need to be a separate bool when/if moving away from dummy SessionV1
    clear_request_body_headers: bool,
    digest: Option<Box<Digest>>,
}

/// A handle to the subrequest session itself to interact or read from it.
pub struct SubrequestHandle {
    /// Channel sender (for subrequest input)
    pub tx: mpsc::Sender<HttpTask>,
    /// Channel receiver (for subrequest output)
    pub rx: mpsc::Receiver<HttpTask>,
    /// Indicates when subrequest wants to start reading body input
    // TODO: use when piping subrequest input/output
    pub subreq_wants_body: oneshot::Receiver<()>,
}

impl SubrequestHandle {
    /// Spawn a task to drain received HttpTasks.
    pub fn drain_tasks(mut self) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let _tx = self.tx; // keep handle to sender alive
            while self.rx.recv().await.is_some() {}
            trace!("subrequest dropped");
        })
    }
}

impl HttpSession {
    /// Create a new http server session for a subrequest.
    /// The created session needs to call [`Self::read_request()`] first before performing
    /// any other operations.
    pub fn new_from_session(session: &GenericHttpSession) -> (Self, SubrequestHandle) {
        let v1_inner = SessionV1::new(Box::new(DummyIO::new(&session.to_h1_raw())));
        let digest = session.digest().cloned();
        // allow buffering a small number of tasks, otherwise exert backpressure
        const CHANNEL_BUFFER_SIZE: usize = 4;
        let (downstream_tx, downstream_rx) = mpsc::channel(CHANNEL_BUFFER_SIZE);
        let (upstream_tx, upstream_rx) = mpsc::channel(CHANNEL_BUFFER_SIZE);
        let (wants_body_tx, wants_body_rx) = oneshot::channel();
        (
            HttpSession {
                v1_inner: Box::new(v1_inner),
                tx: Some(upstream_tx),
                rx: Some(downstream_rx),
                body_reader: BodyReader::new(Some(wants_body_tx)),
                body_writer: BodyWriter::new(),
                read_req_header: false,
                response_written: None,
                read_timeout: None,
                write_timeout: None,
                total_drain_timeout: None,
                body_bytes_sent: 0,
                body_bytes_read: 0,
                retry_buffer: None,
                upgraded: false,
                clear_request_body_headers: false,
                digest: digest.map(Box::new),
            },
            SubrequestHandle {
                tx: downstream_tx,
                rx: upstream_rx,
                subreq_wants_body: wants_body_rx,
            },
        )
    }

    /// Read the request header. Return `Ok(Some(n))` where the read and parsing are successful.
    pub async fn read_request(&mut self) -> Result<Option<usize>> {
        let res = self.v1_inner.read_request().await?;
        if res.is_none() {
            // this is when h1 client closes the connection without sending data,
            // which shouldn't be the case for a subrequest session just created
            return Error::e_explain(InternalError, "no session request header provided");
        }
        self.read_req_header = true;
        if self.clear_request_body_headers {
            // indicated that we wanted to clear these headers in the past, do so now
            self.clear_request_body_headers();
        }
        Ok(res)
    }

    /// Validate the request header read. This function must be called after the request header
    /// read.
    /// # Panics
    /// this function and most other functions will panic if called before [`Self::read_request()`]
    pub fn validate_request(&self) -> Result<()> {
        self.v1_inner.validate_request()
    }

    /// Return a reference of the `RequestHeader` this session read
    /// # Panics
    /// this function and most other functions will panic if called before [`Self::read_request()`]
    pub fn req_header(&self) -> &RequestHeader {
        self.v1_inner.req_header()
    }

    /// Return a mutable reference of the `RequestHeader` this session read
    /// # Panics
    /// this function and most other functions will panic if called before [`Self::read_request()`]
    pub fn req_header_mut(&mut self) -> &mut RequestHeader {
        self.v1_inner.req_header_mut()
    }

    /// Get the header value for the given header name
    /// If there are multiple headers under the same name, the first one will be returned
    /// Use `self.req_header().header.get_all(name)` to get all the headers under the same name
    pub fn get_header(&self, name: impl AsHeaderName) -> Option<&HeaderValue> {
        self.v1_inner.get_header(name)
    }

    /// Return the method of this request. None if the request is not read yet.
    pub(super) fn get_method(&self) -> Option<&http::Method> {
        self.v1_inner.get_method()
    }

    /// Return the path of the request (i.e., the `/hello?1` of `GET /hello?1 HTTP1.1`)
    /// An empty slice will be used if there is no path or the request is not read yet
    pub(super) fn get_path(&self) -> &[u8] {
        self.v1_inner.get_path()
    }

    /// Return the host header of the request. An empty slice will be used if there is no host header
    pub(super) fn get_host(&self) -> &[u8] {
        self.v1_inner.get_host()
    }

    /// Return a string `$METHOD $PATH, Host: $HOST`. Mostly for logging and debug purpose
    pub fn request_summary(&self) -> String {
        format!(
            "{} {}, Host: {} (subrequest)",
            self.get_method().map_or("-", |r| r.as_str()),
            String::from_utf8_lossy(self.get_path()),
            String::from_utf8_lossy(self.get_host())
        )
    }

    /// Is the request a upgrade request
    pub fn is_upgrade_req(&self) -> bool {
        self.v1_inner.is_upgrade_req()
    }

    /// Get the request header as raw bytes, `b""` when the header doesn't exist
    pub fn get_header_bytes(&self, name: impl AsHeaderName) -> &[u8] {
        self.v1_inner.get_header_bytes(name)
    }

    /// Read the request body. `Ok(None)` when there is no (more) body to read.
    pub async fn read_body_bytes(&mut self) -> Result<Option<Bytes>> {
        let read = self.read_body().await?;
        Ok(read.inspect(|b| {
            self.body_bytes_read += b.len();
            if let Some(buffer) = self.retry_buffer.as_mut() {
                buffer.write_to_buffer(b);
            }
        }))
    }

    async fn do_read_body(&mut self) -> Result<Option<Bytes>> {
        self.init_body_reader();
        self.body_reader
            .read_body(self.rx.as_mut().expect("rx valid before shutdown"))
            .await
    }

    /// Read the body bytes with timeout.
    async fn read_body(&mut self) -> Result<Option<Bytes>> {
        match self.read_timeout {
            Some(t) => match timeout(t, self.do_read_body()).await {
                Ok(res) => res,
                Err(_) => Error::e_explain(
                    ReadTimedout,
                    format!("reading body, timeout: {t:?} (subrequest)"),
                ),
            },
            None => self.do_read_body().await,
        }
    }

    async fn do_drain_request_body(&mut self) -> Result<()> {
        loop {
            match self.read_body_bytes().await {
                Ok(Some(_)) => { /* continue to drain */ }
                Ok(None) => return Ok(()), // done
                Err(e) => return Err(e),
            }
        }
    }

    /// Drain the request body. `Ok(())` when there is no (more) body to read.
    pub async fn drain_request_body(&mut self) -> Result<()> {
        if self.is_body_done() {
            return Ok(());
        }
        match self.total_drain_timeout {
            Some(t) => match timeout(t, self.do_drain_request_body()).await {
                Ok(res) => res,
                Err(_) => Error::e_explain(
                    ReadTimedout,
                    format!("draining body, timeout: {t:?} (subrequest)"),
                ),
            },
            None => self.do_drain_request_body().await,
        }
    }

    /// Whether there is no (more) body to be read.
    pub fn is_body_done(&mut self) -> bool {
        self.init_body_reader();
        self.body_reader.body_done()
    }

    /// Whether the request has an empty body
    /// Because HTTP 1.1 clients have to send either `Content-Length` or `Transfer-Encoding` in order
    /// to signal the server that it will send the body, this function returns accurate results even
    /// only when the request header is just read.
    pub fn is_body_empty(&mut self) -> bool {
        self.init_body_reader();
        self.body_reader.body_empty()
    }

    /// Write the response header to the client.
    /// This function can be called more than once to send 1xx informational headers excluding 101.
    pub async fn write_response_header(&mut self, header: Box<ResponseHeader>) -> Result<()> {
        if let Some(resp) = self.response_written.as_ref() {
            if !resp.status.is_informational() || self.upgraded {
                warn!("Respond header is already sent, cannot send again (subrequest)");
                return Ok(());
            }
        }

        // XXX: don't add additional downstream headers, unlike h1, subreq is mostly treated as a pipe

        // Allow informational header (excluding 101) to pass through without affecting the state
        // of the request
        if header.status == 101 || !header.status.is_informational() {
            // reset request body to done for incomplete upgrade handshakes
            if let Some(upgrade_ok) = self.is_upgrade(&header) {
                if upgrade_ok {
                    debug!("ok upgrade handshake");
                    // For ws we use HTTP1_0 do_read_body_until_closed
                    //
                    // On ws close the initiator sends a close frame and
                    // then waits for a response from the peer, once it receives
                    // a response it closes the conn. After receiving a
                    // control frame indicating the connection should be closed,
                    // a peer discards any further data received.
                    // https://www.rfc-editor.org/rfc/rfc6455#section-1.4
                    self.upgraded = true;
                } else {
                    debug!("bad upgrade handshake!");
                    // reset request body buf and mark as done
                    // safe to reset an upgrade because it doesn't have body
                    self.body_reader.init_content_length(0);
                }
            }
            self.init_body_writer(&header);
        }

        // TODO propagate h2 end
        debug!("send response header (subrequest)");
        match self
            .tx
            .as_mut()
            .expect("tx valid before shutdown")
            .send(HttpTask::Header(header.clone(), false))
            .await
        {
            Ok(()) => {
                self.response_written = Some(*header);
                Ok(())
            }
            Err(e) => Error::e_because(WriteError, "writing response header", e),
        }
    }

    /// Return the response header if it is already sent.
    pub fn response_written(&self) -> Option<&ResponseHeader> {
        self.response_written.as_ref()
    }

    /// `Some(true)` if the this is a successful upgrade
    /// `Some(false)` if the request is an upgrade but the response refuses it
    /// `None` if the request is not an upgrade.
    pub fn is_upgrade(&self, header: &ResponseHeader) -> Option<bool> {
        self.v1_inner.is_upgrade(header)
    }

    fn init_body_writer(&mut self, header: &ResponseHeader) {
        use http::StatusCode;
        /* the following responses don't have body 204, 304, and HEAD */
        if matches!(
            header.status,
            StatusCode::NO_CONTENT | StatusCode::NOT_MODIFIED
        ) || self.get_method() == Some(&Method::HEAD)
        {
            self.body_writer.init_content_length(0);
            return;
        }

        if header.status.is_informational() && header.status != StatusCode::SWITCHING_PROTOCOLS {
            // 1xx response, not enough to init body
            return;
        }

        if self.is_upgrade(header) == Some(true) {
            self.body_writer.init_until_close();
        } else {
            let te_value = header.headers.get(http::header::TRANSFER_ENCODING);
            if is_header_value_chunked_encoding(te_value) {
                // transfer-encoding takes priority over content-length
                self.body_writer.init_until_close();
            } else {
                let content_length =
                    header_value_content_length(header.headers.get(http::header::CONTENT_LENGTH));
                match content_length {
                    Some(length) => {
                        self.body_writer.init_content_length(length);
                    }
                    None => {
                        /* TODO: 1. connection: keepalive cannot be used,
                        2. mark connection must be closed */
                        self.body_writer.init_until_close();
                    }
                }
            }
        }
    }

    /// Same as [`Self::write_response_header()`] but takes a reference.
    pub async fn write_response_header_ref(&mut self, resp: &ResponseHeader) -> Result<()> {
        self.write_response_header(Box::new(resp.clone())).await
    }

    async fn do_write_body(&mut self, buf: Bytes) -> Result<Option<usize>> {
        let written = self
            .body_writer
            .write_body(self.tx.as_mut().expect("tx valid before shutdown"), buf)
            .await;

        if let Ok(Some(num_bytes)) = written {
            self.body_bytes_sent += num_bytes;
        }

        written
    }

    /// Write response body to the client. Return `Ok(None)` when there shouldn't be more body
    /// to be written, e.g., writing more bytes than what the `Content-Length` header suggests
    pub async fn write_body(&mut self, buf: Bytes) -> Result<Option<usize>> {
        // TODO: check if the response header is written
        match self.write_timeout {
            Some(t) => match timeout(t, self.do_write_body(buf)).await {
                Ok(res) => res,
                Err(_) => Error::e_explain(WriteTimedout, format!("writing body, timeout: {t:?}")),
            },
            None => self.do_write_body(buf).await,
        }
    }

    fn maybe_force_close_body_reader(&mut self) {
        if self.upgraded && !self.body_reader.body_done() {
            // response is done, reset the request body to close
            self.body_reader.init_content_length(0);
        }
    }

    /// Signal that there is no more body to write.
    /// This call will try to flush the buffer if there is any un-flushed data.
    /// For chunked encoding response, this call will also send the last chunk.
    /// For upgraded sessions, this call will also close the reading of the client body.
    pub async fn finish(&mut self) -> Result<Option<usize>> {
        let res = self
            .body_writer
            .finish(self.tx.as_mut().expect("tx valid before shutdown"))
            .await?;

        self.maybe_force_close_body_reader();
        Ok(res)
    }

    /// Return how many response body bytes (application, not wire) already sent downstream
    pub fn body_bytes_sent(&self) -> usize {
        self.body_bytes_sent
    }

    /// Return how many request body bytes (application, not wire) already read from downstream
    pub fn body_bytes_read(&self) -> usize {
        self.body_bytes_read
    }

    fn is_chunked_encoding(&self) -> bool {
        is_header_value_chunked_encoding(self.get_header(header::TRANSFER_ENCODING))
    }

    /// Clear body-related subrequest headers.
    ///
    /// This is ok to call before the request is read; the headers will then be cleared after
    /// reading the request header.
    pub fn clear_request_body_headers(&mut self) {
        self.clear_request_body_headers = true;
        if self.read_req_header {
            let req = self.v1_inner.req_header_mut();
            req.remove_header(&header::CONTENT_LENGTH);
            req.remove_header(&header::TRANSFER_ENCODING);
            req.remove_header(&header::CONTENT_TYPE);
            req.remove_header(&header::CONTENT_ENCODING);
        }
    }

    fn init_body_reader(&mut self) {
        if self.body_reader.need_init() {
            // reset retry buffer
            if let Some(buffer) = self.retry_buffer.as_mut() {
                buffer.clear();
            }

            if self.req_header().version == Version::HTTP_11 && self.is_upgrade_req() {
                self.body_reader.init_until_close();
                return;
            }

            if self.is_chunked_encoding() {
                // if chunked encoding, content-length should be ignored
                // TE is not visible at subrequest HttpTask level
                // so this means read until request closure
                self.body_reader.init_until_close();
            } else {
                let cl = header_value_content_length(self.get_header(header::CONTENT_LENGTH));
                match cl {
                    Some(i) => {
                        self.body_reader.init_content_length(i);
                    }
                    None => {
                        match self.req_header().version {
                            Version::HTTP_11 => {
                                // Per RFC assume no body by default in HTTP 1.1
                                self.body_reader.init_content_length(0);
                            }
                            _ => {
                                self.body_reader.init_until_close();
                            }
                        }
                    }
                }
            }
        }
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

    /// This function will (async) block forever until the client closes the connection.
    pub async fn idle(&mut self) -> Result<HttpTask> {
        let rx = self.rx.as_mut().expect("rx valid before shutdown");
        let mut task = rx
            .recv()
            .await
            .or_err(ReadError, "during HTTP idle state")?;
        // just consume empty body or done messages, the downstream channel is not a real
        // connection and only used for this one request
        while matches!(&task, HttpTask::Done)
            || matches!(&task, HttpTask::Body(b, _) if b.as_ref().is_none_or(|b| b.is_empty()))
        {
            task = rx
                .recv()
                .await
                .or_err(ReadError, "during HTTP idle state")?;
        }
        Ok(task)
    }

    /// This function will return body bytes (same as [`Self::read_body_bytes()`]), but after
    /// the client body finishes (`Ok(None)` is returned), calling this function again will block
    /// forever, same as [`Self::idle()`].
    pub async fn read_body_or_idle(&mut self, no_body_expected: bool) -> Result<Option<Bytes>> {
        if no_body_expected || self.is_body_done() {
            let read_task = self.idle().await?;
            Error::e_explain(
                ConnectError,
                format!("Sent unexpected task {read_task:?} after end of body (subrequest)"),
            )
        } else {
            self.read_body_bytes().await
        }
    }

    /// Return the raw bytes of the request header.
    pub fn get_headers_raw_bytes(&self) -> Bytes {
        self.v1_inner.get_headers_raw_bytes()
    }

    /// Close the subrequest channels, indicating that no more data will be sent
    /// or received. This is expected to be called before dropping the `Session` itself.
    pub fn shutdown(&mut self) {
        drop(self.tx.take());
        drop(self.rx.take());
    }

    /// Sets the downstream read timeout. This will trigger if we're unable
    /// to read from the subrequest channels after `timeout`.
    pub fn set_read_timeout(&mut self, timeout: Option<Duration>) {
        self.read_timeout = timeout;
    }

    /// Get the downstream read timeout.
    pub fn get_read_timeout(&self) -> Option<Duration> {
        self.read_timeout
    }

    /// Sets the downstream write timeout. This will trigger if we're unable
    /// to write to the subrequest channel after `timeout`.
    pub fn set_write_timeout(&mut self, timeout: Option<Duration>) {
        self.write_timeout = timeout;
    }

    /// Get the downstream write timeout.
    pub fn get_write_timeout(&self) -> Option<Duration> {
        self.write_timeout
    }

    /// Sets the total drain timeout.
    /// Note that the downstream read timeout still applies between body byte reads.
    pub fn set_total_drain_timeout(&mut self, timeout: Option<Duration>) {
        self.total_drain_timeout = timeout;
    }

    /// Get the downstream total drain timeout.
    pub fn get_total_drain_timeout(&self) -> Option<Duration> {
        self.total_drain_timeout
    }

    /// Return the [Digest], this is originally from the main request.
    pub fn digest(&self) -> Option<&Digest> {
        self.digest.as_deref()
    }

    /// Return a mutable [Digest] reference.
    pub fn digest_mut(&mut self) -> Option<&mut Digest> {
        self.digest.as_deref_mut()
    }

    /// Return the client (peer) address of the main request.
    pub fn client_addr(&self) -> Option<&SocketAddr> {
        self.digest()
            .and_then(|d| d.socket_digest.as_ref())
            .map(|d| d.peer_addr())?
    }

    /// Return the server (local) address of the main request.
    pub fn server_addr(&self) -> Option<&SocketAddr> {
        self.digest()
            .and_then(|d| d.socket_digest.as_ref())
            .map(|d| d.local_addr())?
    }

    /// Write a `100 Continue` response to the client.
    pub async fn write_continue_response(&mut self) -> Result<()> {
        // only send if we haven't already
        if self.response_written.is_none() {
            // size hint Some(0) because default is 8
            return self
                .write_response_header(Box::new(ResponseHeader::build(100, Some(0)).unwrap()))
                .await;
        }
        Ok(())
    }

    async fn response_duplex(&mut self, task: HttpTask) -> Result<bool> {
        let end_stream = match task {
            HttpTask::Header(header, end_stream) => {
                self.write_response_header(header)
                    .await
                    .map_err(|e| e.into_down())?;
                end_stream
            }
            HttpTask::Body(data, end_stream) => match data {
                Some(d) => {
                    if !d.is_empty() {
                        self.write_body(d).await.map_err(|e| e.into_down())?;
                    }
                    end_stream
                }
                None => end_stream,
            },
            HttpTask::Trailer(trailers) => {
                self.write_trailers(trailers).await?;
                true
            }
            HttpTask::Done => true,
            HttpTask::Failed(e) => return Err(e),
        };
        if end_stream {
            // no-op if body wasn't initialized or is finished already
            self.finish().await.map_err(|e| e.into_down())?;
        }
        Ok(end_stream || self.body_writer.finished())
    }

    // TODO: use vectored write to avoid copying
    pub async fn response_duplex_vec(&mut self, mut tasks: Vec<HttpTask>) -> Result<bool> {
        // TODO: send httptask failed on each error?
        let n_tasks = tasks.len();
        if n_tasks == 1 {
            // fallback to single operation to avoid copy
            return self.response_duplex(tasks.pop().unwrap()).await;
        }
        let mut end_stream = false;
        for task in tasks.into_iter() {
            end_stream = match task {
                HttpTask::Header(header, end_stream) => {
                    self.write_response_header(header)
                        .await
                        .map_err(|e| e.into_down())?;
                    end_stream
                }
                HttpTask::Body(data, end_stream) => match data {
                    Some(d) => {
                        if !d.is_empty() {
                            self.write_body(d).await.map_err(|e| e.into_down())?;
                        }
                        end_stream
                    }
                    None => end_stream,
                },
                HttpTask::Done => {
                    // write done
                    // we'll send HttpTask::Done at the end of this loop in finish
                    true
                }
                HttpTask::Trailer(trailers) => {
                    self.write_trailers(trailers).await?;
                    true
                }
                HttpTask::Failed(e) => {
                    // write failed
                    // error should also be returned when sender drops
                    return Err(e);
                }
            } || end_stream; // safe guard in case `end` in tasks flips from true to false
        }
        if end_stream {
            // no-op if body wasn't initialized or is finished already
            self.finish().await.map_err(|e| e.into_down())?;
        }
        Ok(end_stream || self.body_writer.finished())
    }

    /// Write response trailers to the client, this also closes the stream.
    pub async fn write_trailers(&mut self, trailers: Option<Box<HeaderMap>>) -> Result<()> {
        self.body_writer
            .write_trailers(
                self.tx.as_mut().expect("tx valid before shutdown"),
                trailers,
            )
            .await
    }
}

#[cfg(test)]
mod tests_stream {
    use super::*;
    use crate::protocols::http::subrequest::body::{BodyMode, ParseState};
    use http::StatusCode;

    use std::str;
    use tokio_test::io::Builder;

    fn init_log() {
        let _ = env_logger::builder().is_test(true).try_init();
    }

    async fn session_from_input(input: &[u8]) -> (HttpSession, SubrequestHandle) {
        let mock_io = Builder::new().read(input).build();
        let mut http_stream = GenericHttpSession::new_http1(Box::new(mock_io));
        http_stream.read_request().await.unwrap();
        let (mut http_stream, handle) = HttpSession::new_from_session(&http_stream);
        http_stream.read_request().await.unwrap();
        (http_stream, handle)
    }

    async fn build_upgrade_req(upgrade: &str, conn: &str) -> (HttpSession, SubrequestHandle) {
        let input = format!("GET / HTTP/1.1\r\nHost: pingora.org\r\nUpgrade: {upgrade}\r\nConnection: {conn}\r\n\r\n");
        session_from_input(input.as_bytes()).await
    }

    async fn build_req() -> (HttpSession, SubrequestHandle) {
        let input = "GET / HTTP/1.1\r\nHost: pingora.org\r\n\r\n".to_string();
        session_from_input(input.as_bytes()).await
    }

    #[tokio::test]
    async fn read_basic() {
        init_log();
        let input = b"GET / HTTP/1.1\r\n\r\n";
        let (http_stream, _handle) = session_from_input(input).await;
        assert_eq!(0, http_stream.req_header().headers.len());
        assert_eq!(Method::GET, http_stream.req_header().method);
        assert_eq!(b"/", http_stream.req_header().uri.path().as_bytes());
    }

    #[tokio::test]
    async fn read_upgrade_req() {
        // http 1.0
        let input = b"GET / HTTP/1.0\r\nHost: pingora.org\r\nUpgrade: websocket\r\nConnection: upgrade\r\n\r\n";
        let (http_stream, _handle) = session_from_input(input).await;
        assert!(!http_stream.is_upgrade_req());

        // different method
        let input = b"POST / HTTP/1.1\r\nHost: pingora.org\r\nUpgrade: websocket\r\nConnection: upgrade\r\n\r\n";
        let (http_stream, _handle) = session_from_input(input).await;
        assert!(http_stream.is_upgrade_req());

        // missing upgrade header
        let input = b"GET / HTTP/1.1\r\nHost: pingora.org\r\nConnection: upgrade\r\n\r\n";
        let (http_stream, _handle) = session_from_input(input).await;
        assert!(!http_stream.is_upgrade_req());

        // no connection header
        let input = b"GET / HTTP/1.1\r\nHost: pingora.org\r\nUpgrade: WebSocket\r\n\r\n";
        let (http_stream, _handle) = session_from_input(input).await;
        assert!(http_stream.is_upgrade_req());

        let (http_stream, _handle) = build_upgrade_req("websocket", "Upgrade").await;
        assert!(http_stream.is_upgrade_req());

        // mixed case
        let (http_stream, _handle) = build_upgrade_req("WebSocket", "Upgrade").await;
        assert!(http_stream.is_upgrade_req());
    }

    #[tokio::test]
    async fn read_upgrade_req_with_1xx_response() {
        let (mut http_stream, _handle) = build_upgrade_req("websocket", "upgrade").await;
        assert!(http_stream.is_upgrade_req());
        let mut response = ResponseHeader::build(StatusCode::CONTINUE, None).unwrap();
        response.set_version(http::Version::HTTP_11);
        http_stream
            .write_response_header(Box::new(response))
            .await
            .unwrap();
        // 100 won't affect body state
        assert!(!http_stream.is_body_done());
    }

    #[tokio::test]
    async fn write() {
        let (mut http_stream, mut handle) = build_req().await;
        let mut new_response = ResponseHeader::build(StatusCode::OK, None).unwrap();
        new_response.append_header("Foo", "Bar").unwrap();
        http_stream
            .write_response_header_ref(&new_response)
            .await
            .unwrap();
        match handle.rx.try_recv().unwrap() {
            HttpTask::Header(header, end) => {
                assert_eq!(header.status, StatusCode::OK);
                assert_eq!(header.headers["foo"], "Bar");
                assert!(!end);
            }
            t => panic!("unexpected task {t:?}"),
        }
    }

    #[tokio::test]
    async fn write_informational() {
        let (mut http_stream, mut handle) = build_req().await;
        let response_100 = ResponseHeader::build(StatusCode::CONTINUE, None).unwrap();
        http_stream
            .write_response_header_ref(&response_100)
            .await
            .unwrap();
        match handle.rx.try_recv().unwrap() {
            HttpTask::Header(header, end) => {
                assert_eq!(header.status, StatusCode::CONTINUE);
                assert!(!end);
            }
            t => panic!("unexpected task {t:?}"),
        }

        let response_200 = ResponseHeader::build(StatusCode::OK, None).unwrap();
        http_stream
            .write_response_header_ref(&response_200)
            .await
            .unwrap();
        match handle.rx.try_recv().unwrap() {
            HttpTask::Header(header, end) => {
                assert_eq!(header.status, StatusCode::OK);
                assert!(!end);
            }
            t => panic!("unexpected task {t:?}"),
        }
    }

    #[tokio::test]
    async fn write_101_switching_protocol() {
        let (mut http_stream, mut handle) = build_upgrade_req("WebSocket", "Upgrade").await;
        let mut response_101 =
            ResponseHeader::build(StatusCode::SWITCHING_PROTOCOLS, None).unwrap();
        response_101.append_header("Foo", "Bar").unwrap();
        http_stream
            .write_response_header_ref(&response_101)
            .await
            .unwrap();

        match handle.rx.try_recv().unwrap() {
            HttpTask::Header(header, end) => {
                assert_eq!(header.status, StatusCode::SWITCHING_PROTOCOLS);
                assert!(!end);
            }
            t => panic!("unexpected task {t:?}"),
        }
        assert!(http_stream.upgraded);

        let wire_body = Bytes::from(&b"PAYLOAD"[..]);
        let n = http_stream
            .write_body(wire_body.clone())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(wire_body.len(), n);
        // this write should be ignored
        let response_502 = ResponseHeader::build(StatusCode::BAD_GATEWAY, None).unwrap();
        http_stream
            .write_response_header_ref(&response_502)
            .await
            .unwrap();

        match handle.rx.try_recv().unwrap() {
            HttpTask::Body(body, _end) => {
                assert_eq!(body.unwrap().len(), n);
            }
            t => panic!("unexpected task {t:?}"),
        }
        assert_eq!(
            handle.rx.try_recv().unwrap_err(),
            mpsc::error::TryRecvError::Empty
        );
    }

    #[tokio::test]
    async fn write_body_cl() {
        let (mut http_stream, _handle) = build_req().await;
        let wire_body = Bytes::from(&b"a"[..]);
        let mut new_response = ResponseHeader::build(StatusCode::OK, None).unwrap();
        new_response.append_header("Content-Length", "1").unwrap();
        http_stream
            .write_response_header_ref(&new_response)
            .await
            .unwrap();
        assert_eq!(
            http_stream.body_writer.body_mode,
            BodyMode::ContentLength(1, 0)
        );
        let n = http_stream
            .write_body(wire_body.clone())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(wire_body.len(), n);
        let n = http_stream.finish().await.unwrap().unwrap();
        assert_eq!(wire_body.len(), n);
    }

    #[tokio::test]
    async fn write_body_until_close() {
        let (mut http_stream, _handle) = build_req().await;
        let new_response = ResponseHeader::build(StatusCode::OK, None).unwrap();
        http_stream
            .write_response_header_ref(&new_response)
            .await
            .unwrap();
        assert_eq!(http_stream.body_writer.body_mode, BodyMode::UntilClose(0));
        let wire_body = Bytes::from(&b"PAYLOAD"[..]);
        let n = http_stream
            .write_body(wire_body.clone())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(wire_body.len(), n);
        let n = http_stream.finish().await.unwrap().unwrap();
        assert_eq!(wire_body.len(), n);
    }

    #[tokio::test]
    async fn read_with_illegal() {
        init_log();
        let input1 = b"GET /a?q=b c HTTP/1.1\r\n";
        let input2 = b"Host: pingora.org\r\nContent-Length: 3\r\n\r\n";
        let input3 = b"abc";
        let mock_io = Builder::new().read(&input1[..]).read(&input2[..]).build();
        let mut http_stream = GenericHttpSession::new_http1(Box::new(mock_io));
        http_stream.read_request().await.unwrap();
        let (mut http_stream, handle) = HttpSession::new_from_session(&http_stream);
        http_stream.read_request().await.unwrap();
        handle
            .tx
            .send(HttpTask::Body(Some(Bytes::from(&input3[..])), false))
            .await
            .unwrap();

        assert_eq!(http_stream.get_path(), &b"/a?q=b%20c"[..]);
        let res = http_stream.read_body().await.unwrap().unwrap();
        assert_eq!(res, &input3[..]);
        assert_eq!(http_stream.body_reader.body_state, ParseState::Complete(3));
    }

    #[tokio::test]
    async fn test_write_body_write_timeout() {
        let (mut http_stream, _handle) = build_req().await;
        http_stream.write_timeout = Some(Duration::from_millis(100));
        let mut new_response = ResponseHeader::build(StatusCode::OK, None).unwrap();
        new_response.append_header("Content-Length", "10").unwrap();
        http_stream
            .write_response_header_ref(&new_response)
            .await
            .unwrap();
        let body_write_buf = Bytes::from(&b"abc"[..]);
        http_stream
            .write_body(body_write_buf.clone())
            .await
            .unwrap();
        http_stream
            .write_body(body_write_buf.clone())
            .await
            .unwrap();
        http_stream.write_body(body_write_buf).await.unwrap();
        // channel full
        let last_body = Bytes::from(&b"a"[..]);
        let res = http_stream.write_body(last_body).await;
        assert_eq!(res.unwrap_err().etype(), &WriteTimedout);
    }

    #[tokio::test]
    async fn test_write_continue_resp() {
        let (mut http_stream, mut handle) = build_req().await;
        http_stream.write_continue_response().await.unwrap();
        match handle.rx.try_recv().unwrap() {
            HttpTask::Header(header, end) => {
                assert_eq!(header.status, StatusCode::CONTINUE);
                assert!(!end);
            }
            t => panic!("unexpected task {t:?}"),
        }
    }
}
