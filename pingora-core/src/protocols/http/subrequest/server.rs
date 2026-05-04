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
use http::{header, header::AsHeaderName, HeaderMap, Method};
use log::{debug, trace, warn};
use pingora_error::{Error, ErrorType::*, OkOrErr, OrErr, Result};
use pingora_http::{RequestHeader, ResponseHeader};
use pingora_timeout::timeout;
use std::collections::VecDeque;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};

use super::body::{BodyMode, BodyReader, BodyWriter, PREMATURE_BODY_END};
use crate::protocols::http::{
    body_buffer::FixedBuffer,
    server::Session as GenericHttpSession,
    subrequest::dummy::DummyIO,
    v1::common::{header_value_content_length, is_chunked_encoding_from_headers, BODY_BUF_LIMIT},
    v1::server::HttpSession as SessionV1,
    HttpTask,
};
use crate::protocols::{Digest, SocketAddr};

#[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
enum StreamEndState {
    #[default]
    Open,
    FinishRequired,
    Finished,
}

/// State for the cancel-safe proxy task write API.
#[derive(Default)]
struct ProxyTaskState {
    /// Tasks remain queued until `Permit::send` commits them to the channel.
    tasks: VecDeque<HttpTask>,
    /// Final `Done` is waiting for channel capacity.
    finish_in_progress: bool,
    /// End-of-stream observed from already-consumed tasks.
    stream_end: StreamEndState,
}

impl ProxyTaskState {
    fn require_finish(&mut self) {
        self.stream_end = StreamEndState::FinishRequired;
    }

    fn mark_finished(&mut self) {
        self.stream_end = StreamEndState::Finished;
    }

    fn clear_stream_end(&mut self) {
        self.stream_end = StreamEndState::Open;
    }

    fn finish_required(&self) -> bool {
        self.stream_end == StreamEndState::FinishRequired
    }

    fn finished(&self) -> bool {
        self.stream_end == StreamEndState::Finished
    }
}

/// The HTTP server session
pub struct HttpSession {
    // these are only options because we allow dropping them separately on shutdown
    tx: Option<mpsc::Sender<HttpTask>>,
    rx: Option<mpsc::Receiver<HttpTask>>,
    // Currently subrequest session is initialized via a dummy SessionV1 only
    // TODO: need to be able to indicate H2 / other HTTP versions here
    v1_inner: Box<SessionV1>,
    proxy_error: Option<oneshot::Sender<Box<Error>>>, // option to consume the sender
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
    /// Whether the cancel-safe proxy task API is enabled for this session.
    /// Defaults to `false`. Toggle via [`set_proxy_tasks_enabled`](Self::set_proxy_tasks_enabled).
    proxy_tasks_enabled: bool,
    /// Cancel-safe proxy task state.
    proxy_task_state: ProxyTaskState,
}

/// A handle to the subrequest session itself to interact or read from it.
pub struct SubrequestHandle {
    /// Channel sender (for subrequest input)
    pub tx: mpsc::Sender<HttpTask>,
    /// Channel receiver (for subrequest output)
    pub rx: mpsc::Receiver<HttpTask>,
    /// Indicates when subrequest wants to start reading body input
    pub subreq_wants_body: oneshot::Receiver<()>,
    /// Any final or downstream error that was encountered while proxying
    pub subreq_proxy_error: oneshot::Receiver<Box<Error>>,
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
        let (proxy_error_tx, proxy_error_rx) = oneshot::channel();
        (
            HttpSession {
                v1_inner: Box::new(v1_inner),
                tx: Some(upstream_tx),
                rx: Some(downstream_rx),
                proxy_error: Some(proxy_error_tx),
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
                proxy_tasks_enabled: false,
                proxy_task_state: ProxyTaskState::default(),
            },
            SubrequestHandle {
                tx: downstream_tx,
                rx: upstream_rx,
                subreq_wants_body: wants_body_rx,
                subreq_proxy_error: proxy_error_rx,
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

        let mut upgrade_ok: Option<bool> = None;
        if header.status == 101 || !header.status.is_informational() {
            upgrade_ok = self.is_upgrade(&header);
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
                self.init_body_writer(&header);
                self.response_written = Some(*header);
                if let Some(true) = upgrade_ok {
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
                    // Now that the upgrade was successful, we need to change
                    // how we interpret the rest of the body as pass-through.
                    if self.body_reader.need_init() {
                        self.init_body_reader();
                    } else {
                        // already initialized
                        // immediately start reading the rest of the body as upgraded
                        // (in theory most upgraded requests shouldn't have any body)
                        //
                        // TODO: https://datatracker.ietf.org/doc/html/rfc9110#name-upgrade
                        // the most spec-compliant behavior is to switch interpretation
                        // after sending the former body. For now we immediately
                        // switch interpretation to match nginx behavior.
                        // TODO: this has no effect resetting the body counter of TE chunked
                        self.body_reader.convert_to_close_delimited();
                    }
                } else if upgrade_ok == Some(false) {
                    debug!("bad upgrade handshake!");
                    // continue to read body as-is, this is now just a regular request
                }
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

    /// Was this request successfully turned into an upgraded connection?
    ///
    /// Both the request had to have been an `Upgrade` request
    /// and the response had to have been a `101 Switching Protocols`.
    // XXX: this should only be valid if subrequest is standing in for
    // a v1 session.
    pub fn was_upgraded(&self) -> bool {
        self.upgraded
    }

    fn init_body_writer(&mut self, header: &ResponseHeader) {
        if let Some(mode) = body_mode_for_header(
            header,
            self.get_method(),
            self.is_upgrade(header) == Some(true),
        ) {
            apply_body_mode(&mut self.body_writer, mode);
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

    /// Signal to error listener held by SubrequestHandle that a proxy error was encountered,
    /// and pass along what that error was.
    ///
    /// This is helpful to signal what errors were encountered outside of the proxy state machine,
    /// e.g. during subrequest request filters.
    ///
    /// Note: in the case of multiple proxy failures e.g. when caching, only the first error will
    /// be propagated (i.e. downstream error first if it goes away before upstream).
    pub fn on_proxy_failure(&mut self, e: Box<Error>) {
        // fine if handle is gone
        if let Some(sender) = self.proxy_error.take() {
            let _ = sender.send(e);
        }
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
        is_chunked_encoding_from_headers(&self.req_header().headers)
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

            if self.was_upgraded() {
                // if upgraded _post_ 101 (and body was not init yet)
                // treat as upgraded body (pass through until closed)
                self.body_reader.init_close_delimited();
            } else if self.is_chunked_encoding() {
                // if chunked encoding, content-length should be ignored
                // TE is not visible at subrequest HttpTask level
                // so this means read until request closure
                self.body_reader.init_close_delimited();
            } else {
                let cl = header_value_content_length(self.get_header(header::CONTENT_LENGTH));
                match cl {
                    Some(i) => {
                        self.body_reader.init_content_length(i);
                    }
                    None => {
                        // Per RFC 9112: "Request messages are never close-delimited because they are
                        // always explicitly framed by length or transfer coding, with the absence of
                        // both implying the request ends immediately after the header section."
                        // All HTTP/1.x requests without Content-Length or Transfer-Encoding have 0 body
                        self.body_reader.init_content_length(0);
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

    async fn write_non_empty_body(&mut self, data: Option<Bytes>, upgraded: bool) -> Result<()> {
        if upgraded != self.upgraded {
            if upgraded {
                panic!("Unexpected UpgradedBody task received on un-upgraded downstream session (subrequest)");
            } else {
                panic!("Unexpected Body task received on upgraded downstream session (subrequest)");
            }
        }
        let Some(d) = data else {
            return Ok(());
        };
        if d.is_empty() {
            return Ok(());
        }
        self.write_body(d).await.map_err(|e| e.into_down())?;
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
            HttpTask::Body(data, end_stream) => {
                self.write_non_empty_body(data, false).await?;
                end_stream
            }
            HttpTask::UpgradedBody(data, end_stream) => {
                self.write_non_empty_body(data, true).await?;
                end_stream
            }
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
                HttpTask::Body(data, end_stream) => {
                    self.write_non_empty_body(data, false).await?;
                    end_stream
                }
                HttpTask::UpgradedBody(data, end_stream) => {
                    self.write_non_empty_body(data, true).await?;
                    end_stream
                }
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

    // Cancel-safe proxy task API. Unlike v1's partial `AsyncWrite` state
    // machines, subrequest uses mpsc `reserve()` + synchronous `Permit::send`.

    /// Whether the cancel-safe proxy task API is enabled for this session.
    pub fn proxy_tasks_enabled(&self) -> bool {
        self.proxy_tasks_enabled
    }

    /// Enable or disable the cancel-safe proxy task API for this session.
    pub fn set_proxy_tasks_enabled(&mut self, enabled: bool) {
        self.proxy_tasks_enabled = enabled;
    }

    /// Queue a proxy task for cancel-safe writing.
    pub fn send_proxy_task(&mut self, task: HttpTask) {
        self.proxy_task_state.tasks.push_back(task);
    }

    /// Whether there are pending proxy tasks queued for writing.
    pub fn has_pending_proxy_tasks(&self) -> bool {
        self.proxy_task_state.finish_in_progress
            || self.proxy_task_state.finish_required()
            || !self.proxy_task_state.tasks.is_empty()
    }

    /// Write queued proxy tasks. Cancelling while waiting for channel capacity
    /// leaves the current task in `proxy_task_state.tasks`.
    pub async fn write_proxy_tasks(&mut self) -> Result<bool> {
        loop {
            if self.proxy_task_state.finished() {
                self.proxy_task_state.tasks.clear();
                return Ok(true);
            }

            if self.proxy_task_state.finish_in_progress {
                self.finish_proxy_task().await?;
                self.proxy_task_state.mark_finished();
                return Ok(true);
            }

            let Some(front) = self.proxy_task_state.tasks.front() else {
                break;
            };

            // Tasks with no underlying channel send: handle synchronously
            // without reserving a permit.
            match front {
                HttpTask::Done => {
                    self.proxy_task_state.tasks.pop_front();
                    self.proxy_task_state.require_finish();
                    continue;
                }
                HttpTask::Failed(_) => {
                    let HttpTask::Failed(e) = self
                        .proxy_task_state
                        .tasks
                        .pop_front()
                        .expect("queue had a Failed task at the front")
                    else {
                        unreachable!()
                    };
                    self.proxy_task_state.clear_stream_end();
                    return Err(e);
                }
                _ => {}
            }

            if let HttpTask::Header(_, header_end) = front {
                let already_sent = match self.response_written.as_ref() {
                    Some(resp) => !resp.status.is_informational() || self.upgraded,
                    None => false,
                };
                if already_sent {
                    warn!("Respond header is already sent, cannot send again (subrequest, proxy task)");
                    if *header_end {
                        self.proxy_task_state.require_finish();
                    }
                    self.proxy_task_state.tasks.pop_front();
                    continue;
                }
            }

            if let Some((upgraded_task, body_end, no_data)) = match front {
                HttpTask::Body(data, end) => {
                    Some((false, *end, data.as_ref().is_none_or(|d| d.is_empty())))
                }
                HttpTask::UpgradedBody(data, end) => {
                    Some((true, *end, data.as_ref().is_none_or(|d| d.is_empty())))
                }
                _ => None,
            } {
                if upgraded_task != self.upgraded {
                    if upgraded_task {
                        panic!("Unexpected UpgradedBody task received on un-upgraded downstream session (subrequest, proxy task)");
                    } else {
                        panic!("Unexpected Body task received on upgraded downstream session (subrequest, proxy task)");
                    }
                }

                if body_end {
                    self.proxy_task_state.require_finish();
                }

                if no_data {
                    self.proxy_task_state.tasks.pop_front();
                    continue;
                }

                match self.body_writer.body_mode {
                    BodyMode::Complete(_) => {
                        self.proxy_task_state.tasks.pop_front();
                        continue;
                    }
                    BodyMode::ContentLength(total, written) if written >= total => {
                        self.proxy_task_state.tasks.pop_front();
                        continue;
                    }
                    BodyMode::ToSelect => {
                        self.proxy_task_state.tasks.pop_front();
                        self.proxy_task_state.clear_stream_end();
                        return Error::e_explain(
                            InternalError,
                            "subrequest body proxy task before header is sent",
                        );
                    }
                    _ => {}
                }
            }

            // `reserve()` is the only cancellation point; the queued task is
            // popped only after a permit is acquired.
            let tx_ref = self
                .tx
                .as_ref()
                .ok_or_else(|| Error::explain(InternalError, "subrequest tx already shut down"))?;
            let permit = match self.write_timeout {
                Some(t) => match timeout(t, tx_ref.reserve()).await {
                    Ok(res) => res.or_err(WriteError, "subrequest channel closed")?,
                    Err(_) => {
                        return Error::e_explain(
                            WriteTimedout,
                            format!("reserving subrequest channel slot, timeout: {t:?}"),
                        );
                    }
                },
                None => tx_ref
                    .reserve()
                    .await
                    .or_err(WriteError, "subrequest channel closed")?,
            };

            // From here until `permit.send`, no `.await`; dispatch is atomic.
            let task = self
                .proxy_task_state
                .tasks
                .pop_front()
                .expect("queue non-empty");

            match task {
                HttpTask::Header(header, hdr_end) => {
                    if hdr_end {
                        self.proxy_task_state.require_finish();
                    }

                    let upgrade_ok = if header.status == 101 || !header.status.is_informational() {
                        let outcome = self.v1_inner.is_upgrade(&header);
                        let mode = body_mode_for_header(
                            &header,
                            self.v1_inner.get_method(),
                            outcome == Some(true),
                        );
                        (outcome, mode)
                    } else {
                        (None, None)
                    };

                    permit.send(HttpTask::Header(header.clone(), false));

                    if let Some(mode) = upgrade_ok.1 {
                        apply_body_mode(&mut self.body_writer, mode);
                    }
                    self.response_written = Some(*header);
                    if let Some(true) = upgrade_ok.0 {
                        debug!("ok upgrade handshake (subrequest, proxy task)");
                        self.upgraded = true;
                        if self.body_reader.need_init() {
                            self.init_body_reader();
                        } else {
                            self.body_reader.convert_to_close_delimited();
                        }
                    } else if upgrade_ok.0 == Some(false) {
                        debug!("bad upgrade handshake! (subrequest, proxy task)");
                    }
                }

                HttpTask::Body(data, end) => {
                    if end {
                        self.proxy_task_state.require_finish();
                    }
                    dispatch_body_inline(
                        &mut self.body_writer,
                        &mut self.body_bytes_sent,
                        self.upgraded,
                        data,
                        /* upgraded_task = */ false,
                        permit,
                    )?;
                }
                HttpTask::UpgradedBody(data, end) => {
                    if end {
                        self.proxy_task_state.require_finish();
                    }
                    dispatch_body_inline(
                        &mut self.body_writer,
                        &mut self.body_bytes_sent,
                        self.upgraded,
                        data,
                        /* upgraded_task = */ true,
                        permit,
                    )?;
                }

                HttpTask::Trailer(trailers) => {
                    permit.send(HttpTask::Trailer(trailers));
                    self.proxy_task_state.require_finish();
                }

                HttpTask::Done | HttpTask::Failed(_) => {
                    unreachable!("Done/Failed are handled above without reserving a permit")
                }
            }
        }

        // Match `response_duplex_vec`: finish whenever any task signalled EOS.
        if self.proxy_task_state.finish_required() || self.body_writer.finished() {
            self.finish_proxy_task().await?;
            self.proxy_task_state.mark_finished();
            return Ok(true);
        }

        Ok(self.body_writer.finished())
    }

    async fn finish_proxy_task(&mut self) -> Result<()> {
        if matches!(
            &self.body_writer.body_mode,
            BodyMode::Complete(_) | BodyMode::ToSelect
        ) {
            self.maybe_force_close_body_reader();
            self.proxy_task_state.finish_in_progress = false;
            return Ok(());
        }

        if let BodyMode::ContentLength(total, written) = self.body_writer.body_mode {
            if written < total {
                self.body_writer.body_mode = BodyMode::Complete(written);
                self.proxy_task_state.finish_in_progress = false;
                self.proxy_task_state.clear_stream_end();
                return Error::e_explain(
                    PREMATURE_BODY_END,
                    format!(
                        "Content-length: {total} bytes written: {written} (subrequest, proxy task)"
                    ),
                );
            }
        }

        self.proxy_task_state.finish_in_progress = true;
        self.dispatch_finish().await?;
        self.proxy_task_state.finish_in_progress = false;
        Ok(())
    }

    /// Dispatch the final `HttpTask::Done`, mirroring `body_writer::finish`.
    async fn dispatch_finish(&mut self) -> Result<()> {
        // Reserve cancel-safely, then synchronously update body_mode and send.
        let tx_ref = self
            .tx
            .as_ref()
            .ok_or_else(|| Error::explain(InternalError, "subrequest tx already shut down"))?;
        let permit = match self.write_timeout {
            Some(t) => match timeout(t, tx_ref.reserve()).await {
                Ok(res) => res.or_err(WriteError, "subrequest channel closed")?,
                Err(_) => {
                    return Error::e_explain(
                        WriteTimedout,
                        format!("reserving subrequest channel slot for finish, timeout: {t:?}"),
                    );
                }
            },
            None => tx_ref
                .reserve()
                .await
                .or_err(WriteError, "subrequest channel closed")?,
        };

        match self.body_writer.body_mode {
            BodyMode::ContentLength(_total, written) => {
                self.body_writer.body_mode = BodyMode::Complete(written);
                permit.send(HttpTask::Done);
            }
            BodyMode::UntilClose(written) => {
                self.body_writer.body_mode = BodyMode::Complete(written);
                permit.send(HttpTask::Done);
            }
            BodyMode::Complete(_) => {
                unreachable!("no-op body modes are handled before reserve")
            }
            BodyMode::ToSelect => {
                unreachable!("no-op body modes are handled before reserve")
            }
        }
        self.maybe_force_close_body_reader();
        Ok(())
    }
}

fn body_mode_for_header(
    header: &ResponseHeader,
    method: Option<&Method>,
    is_upgrade_ok: bool,
) -> Option<BodyMode> {
    use http::StatusCode;
    if header.status.is_informational() && header.status != StatusCode::SWITCHING_PROTOCOLS {
        return None;
    }
    if matches!(
        header.status,
        StatusCode::NO_CONTENT | StatusCode::NOT_MODIFIED
    ) || method == Some(&Method::HEAD)
    {
        return Some(BodyMode::ContentLength(0, 0));
    }
    if is_upgrade_ok || is_chunked_encoding_from_headers(&header.headers) {
        Some(BodyMode::UntilClose(0))
    } else {
        let content_length =
            header_value_content_length(header.headers.get(http::header::CONTENT_LENGTH));
        match content_length {
            Some(length) => Some(BodyMode::ContentLength(length, 0)),
            None => Some(BodyMode::UntilClose(0)),
        }
    }
}

fn apply_body_mode(body_writer: &mut BodyWriter, mode: BodyMode) {
    match mode {
        BodyMode::ContentLength(total, 0) => body_writer.init_content_length(total),
        BodyMode::UntilClose(0) => body_writer.init_close_delimited(),
        _ => body_writer.body_mode = mode,
    }
}

/// Body dispatch variant that avoids borrowing the whole session while a
/// channel permit borrows `self.tx`.
fn dispatch_body_inline(
    body_writer: &mut BodyWriter,
    body_bytes_sent: &mut usize,
    upgraded: bool,
    data: Option<Bytes>,
    upgraded_task: bool,
    permit: mpsc::Permit<'_, HttpTask>,
) -> Result<()> {
    if upgraded_task != upgraded {
        if upgraded_task {
            panic!("Unexpected UpgradedBody task received on un-upgraded downstream session (subrequest, proxy task)");
        } else {
            panic!("Unexpected Body task received on upgraded downstream session (subrequest, proxy task)");
        }
    }

    let Some(d) = data else {
        drop(permit);
        return Ok(());
    };
    if d.is_empty() {
        drop(permit);
        return Ok(());
    }

    let (to_count, next_mode) = match &body_writer.body_mode {
        BodyMode::ContentLength(total, written) => {
            if written >= total {
                drop(permit);
                return Ok(());
            }
            let remaining = *total - *written;
            let to_write = if remaining < d.len() {
                warn!("Trying to write data over content-length (subrequest, proxy task): {total}");
                remaining
            } else {
                d.len()
            };
            (
                to_write,
                BodyMode::ContentLength(*total, *written + to_write),
            )
        }
        BodyMode::UntilClose(written) => (d.len(), BodyMode::UntilClose(*written + d.len())),
        BodyMode::Complete(_) => {
            drop(permit);
            return Ok(());
        }
        BodyMode::ToSelect => {
            drop(permit);
            return Error::e_explain(
                InternalError,
                "subrequest body proxy task before header is sent",
            );
        }
    };

    let to_send = if to_count < d.len() {
        d.slice(..to_count)
    } else {
        d
    };
    permit.send(HttpTask::Body(Some(to_send), false));
    body_writer.body_mode = next_mode;
    *body_bytes_sent += to_count;
    Ok(())
}

#[cfg(test)]
mod tests_stream {
    use super::*;
    use crate::protocols::http::subrequest::body::{BodyMode, ParseState};
    use bytes::BufMut;
    use http::StatusCode;
    use rstest::rstest;

    use std::str;
    use tokio_test::io::Builder;

    fn init_log() {
        let _ = env_logger::builder().is_test(true).try_init();
    }

    fn test_header(status: StatusCode) -> ResponseHeader {
        ResponseHeader::build(status, None)
            .expect("test status code should build a response header")
    }

    fn recv_task(rx: &mut mpsc::Receiver<HttpTask>) -> HttpTask {
        rx.try_recv()
            .expect("expected subrequest output task to be queued")
    }

    async fn session_from_input(input: &[u8]) -> (HttpSession, SubrequestHandle) {
        let mock_io = Builder::new().read(input).build();
        let mut http_stream = GenericHttpSession::new_http1(Box::new(mock_io));
        http_stream
            .read_request()
            .await
            .expect("test async operation should succeed");
        let (mut http_stream, handle) = HttpSession::new_from_session(&http_stream);
        http_stream
            .read_request()
            .await
            .expect("test async operation should succeed");
        (http_stream, handle)
    }

    pub(super) async fn build_upgrade_req(
        upgrade: &str,
        conn: &str,
    ) -> (HttpSession, SubrequestHandle) {
        let input = format!("GET / HTTP/1.1\r\nHost: pingora.org\r\nUpgrade: {upgrade}\r\nConnection: {conn}\r\n\r\n");
        session_from_input(input.as_bytes()).await
    }

    pub(super) async fn build_req() -> (HttpSession, SubrequestHandle) {
        let input = "GET / HTTP/1.1\r\nHost: pingora.org\r\n\r\n".to_string();
        session_from_input(input.as_bytes()).await
    }

    pub(super) async fn build_head_req() -> (HttpSession, SubrequestHandle) {
        let input = "HEAD / HTTP/1.1\r\nHost: pingora.org\r\n\r\n".to_string();
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
        let mut response = test_header(StatusCode::CONTINUE);
        response.set_version(http::Version::HTTP_11);
        http_stream
            .write_response_header(Box::new(response))
            .await
            .expect("test operation should succeed");
        // 100 won't affect body state
        assert!(http_stream.is_body_done());
    }

    #[tokio::test]
    async fn write() {
        let (mut http_stream, mut handle) = build_req().await;
        let mut new_response = test_header(StatusCode::OK);
        new_response
            .append_header("Foo", "Bar")
            .expect("test operation should succeed");
        http_stream
            .write_response_header_ref(&new_response)
            .await
            .expect("test operation should succeed");
        match recv_task(&mut handle.rx) {
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
        let response_100 = test_header(StatusCode::CONTINUE);
        http_stream
            .write_response_header_ref(&response_100)
            .await
            .expect("test operation should succeed");
        match recv_task(&mut handle.rx) {
            HttpTask::Header(header, end) => {
                assert_eq!(header.status, StatusCode::CONTINUE);
                assert!(!end);
            }
            t => panic!("unexpected task {t:?}"),
        }

        let response_200 = test_header(StatusCode::OK);
        http_stream
            .write_response_header_ref(&response_200)
            .await
            .expect("test operation should succeed");
        match recv_task(&mut handle.rx) {
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
        let mut response_101 = test_header(StatusCode::SWITCHING_PROTOCOLS);
        response_101
            .append_header("Foo", "Bar")
            .expect("test operation should succeed");
        http_stream
            .write_response_header_ref(&response_101)
            .await
            .expect("test operation should succeed");

        match recv_task(&mut handle.rx) {
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
            .expect("test operation should succeed");
        assert_eq!(wire_body.len(), n);
        // this write should be ignored
        let response_502 = ResponseHeader::build(StatusCode::BAD_GATEWAY, None)
            .expect("test operation should succeed");
        http_stream
            .write_response_header_ref(&response_502)
            .await
            .expect("test operation should succeed");

        match recv_task(&mut handle.rx) {
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
        let mut new_response = test_header(StatusCode::OK);
        new_response
            .append_header("Content-Length", "1")
            .expect("test operation should succeed");
        http_stream
            .write_response_header_ref(&new_response)
            .await
            .expect("test operation should succeed");
        assert_eq!(
            http_stream.body_writer.body_mode,
            BodyMode::ContentLength(1, 0)
        );
        let n = http_stream
            .write_body(wire_body.clone())
            .await
            .unwrap()
            .expect("test operation should succeed");
        assert_eq!(wire_body.len(), n);
        let n = http_stream
            .finish()
            .await
            .expect("test async operation should succeed")
            .expect("test operation should succeed");
        assert_eq!(wire_body.len(), n);
    }

    #[tokio::test]
    async fn write_body_until_close() {
        let (mut http_stream, _handle) = build_req().await;
        let new_response = test_header(StatusCode::OK);
        http_stream
            .write_response_header_ref(&new_response)
            .await
            .expect("test operation should succeed");
        assert_eq!(http_stream.body_writer.body_mode, BodyMode::UntilClose(0));
        let wire_body = Bytes::from(&b"PAYLOAD"[..]);
        let n = http_stream
            .write_body(wire_body.clone())
            .await
            .unwrap()
            .expect("test operation should succeed");
        assert_eq!(wire_body.len(), n);
        let n = http_stream
            .finish()
            .await
            .expect("test async operation should succeed")
            .expect("test operation should succeed");
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
        http_stream
            .read_request()
            .await
            .expect("test async operation should succeed");
        let (mut http_stream, handle) = HttpSession::new_from_session(&http_stream);
        http_stream
            .read_request()
            .await
            .expect("test async operation should succeed");
        handle
            .tx
            .send(HttpTask::Body(Some(Bytes::from(&input3[..])), false))
            .await
            .expect("test operation should succeed");

        assert_eq!(http_stream.get_path(), &b"/a?q=b%20c"[..]);
        let res = http_stream
            .read_body()
            .await
            .expect("test async operation should succeed")
            .expect("test operation should succeed");
        assert_eq!(res, &input3[..]);
        assert_eq!(http_stream.body_reader.body_state, ParseState::Complete(3));
    }

    #[tokio::test]
    async fn test_write_body_write_timeout() {
        let (mut http_stream, _handle) = build_req().await;
        http_stream.write_timeout = Some(Duration::from_millis(100));
        let mut new_response = test_header(StatusCode::OK);
        new_response
            .append_header("Content-Length", "10")
            .expect("test operation should succeed");
        http_stream
            .write_response_header_ref(&new_response)
            .await
            .expect("test operation should succeed");
        let body_write_buf = Bytes::from(&b"abc"[..]);
        http_stream
            .write_body(body_write_buf.clone())
            .await
            .expect("test operation should succeed");
        http_stream
            .write_body(body_write_buf.clone())
            .await
            .expect("test operation should succeed");
        http_stream
            .write_body(body_write_buf)
            .await
            .expect("test async operation should succeed");
        // channel full
        let last_body = Bytes::from(&b"a"[..]);
        let res = http_stream.write_body(last_body).await;
        assert_eq!(res.unwrap_err().etype(), &WriteTimedout);
    }

    #[tokio::test]
    async fn test_write_continue_resp() {
        let (mut http_stream, mut handle) = build_req().await;
        http_stream
            .write_continue_response()
            .await
            .expect("test async operation should succeed");
        match recv_task(&mut handle.rx) {
            HttpTask::Header(header, end) => {
                assert_eq!(header.status, StatusCode::CONTINUE);
                assert!(!end);
            }
            t => panic!("unexpected task {t:?}"),
        }
    }

    async fn session_from_input_no_validate(input: &[u8]) -> (HttpSession, SubrequestHandle) {
        let mock_io = Builder::new().read(input).build();
        let mut http_stream = GenericHttpSession::new_http1(Box::new(mock_io));
        // Read the request in v1 inner session to set up headers properly
        http_stream
            .read_request()
            .await
            .expect("test async operation should succeed");
        let (http_stream, handle) = HttpSession::new_from_session(&http_stream);
        (http_stream, handle)
    }

    #[rstest]
    #[case::negative("-1")]
    #[case::not_a_number("abc")]
    #[case::float("1.5")]
    #[case::empty("")]
    #[case::spaces("  ")]
    #[case::mixed("123abc")]
    #[tokio::test]
    async fn validate_request_rejects_invalid_content_length(#[case] invalid_value: &str) {
        init_log();
        let input = format!(
            "POST / HTTP/1.1\r\nHost: pingora.org\r\nContent-Length: {}\r\n\r\n",
            invalid_value
        );
        let mock_io = Builder::new().read(input.as_bytes()).build();
        let mut http_stream = GenericHttpSession::new_http1(Box::new(mock_io));
        // read_request calls validate_request internally on the v1 inner stream, so it should fail here
        let res = http_stream.read_request().await;
        assert!(res.is_err());
        assert_eq!(
            res.unwrap_err().etype(),
            &pingora_error::ErrorType::InvalidHTTPHeader
        );
    }

    #[rstest]
    #[case::valid_zero("0")]
    #[case::valid_small("123")]
    #[case::valid_large("999999")]
    #[tokio::test]
    async fn validate_request_accepts_valid_content_length(#[case] valid_value: &str) {
        init_log();
        let input = format!(
            "POST / HTTP/1.1\r\nHost: pingora.org\r\nContent-Length: {}\r\n\r\n",
            valid_value
        );
        let (mut http_stream, _handle) = session_from_input_no_validate(input.as_bytes()).await;
        let res = http_stream.read_request().await;
        assert!(res.is_ok());
    }

    #[tokio::test]
    async fn validate_request_accepts_no_content_length() {
        init_log();
        let input = b"GET / HTTP/1.1\r\nHost: pingora.org\r\n\r\n";
        let (mut http_stream, _handle) = session_from_input_no_validate(input).await;
        let res = http_stream.read_request().await;
        assert!(res.is_ok());
    }

    const POST_CL_UPGRADE_REQ: &[u8] = b"POST / HTTP/1.1\r\nHost: pingora.org\r\nUpgrade: websocket\r\nConnection: upgrade\r\nContent-Length: 10\r\n\r\n";
    const POST_CHUNKED_UPGRADE_REQ: &[u8] = b"POST / HTTP/1.1\r\nHost: pingora.org\r\nUpgrade: websocket\r\nConnection: upgrade\r\nTransfer-Encoding: chunked\r\n\r\n";
    const POST_BODY_DATA: &[u8] = b"abcdefghij";

    async fn build_upgrade_req_with_body(header: &[u8]) -> (HttpSession, SubrequestHandle) {
        let mock_io = Builder::new().read(header).build();
        let mut http_stream = GenericHttpSession::new_http1(Box::new(mock_io));
        http_stream
            .read_request()
            .await
            .expect("test async operation should succeed");
        let (mut http_stream, handle) = HttpSession::new_from_session(&http_stream);
        http_stream
            .read_request()
            .await
            .expect("test async operation should succeed");
        (http_stream, handle)
    }

    #[rstest]
    #[case::content_length(POST_CL_UPGRADE_REQ)]
    #[case::chunked(POST_CHUNKED_UPGRADE_REQ)]
    #[tokio::test]
    async fn read_upgrade_req_with_body(#[case] header: &[u8]) {
        init_log();
        let (mut http_stream, handle) = build_upgrade_req_with_body(header).await;
        assert!(http_stream.is_upgrade_req());
        // request has body
        assert!(!http_stream.is_body_done());

        // Send body via the handle
        handle
            .tx
            .send(HttpTask::Body(Some(Bytes::from(POST_BODY_DATA)), true))
            .await
            .expect("test operation should succeed");

        let mut buf = vec![];
        while let Some(b) = http_stream
            .read_body_bytes()
            .await
            .expect("test async operation should succeed")
        {
            buf.put_slice(&b);
        }
        assert_eq!(buf, POST_BODY_DATA);
        assert_eq!(http_stream.body_reader.body_state, ParseState::Complete(10));
        assert_eq!(http_stream.body_bytes_read(), 10);

        assert!(http_stream.is_body_done());

        let mut response = test_header(StatusCode::SWITCHING_PROTOCOLS);
        response.set_version(http::Version::HTTP_11);
        http_stream
            .write_response_header(Box::new(response))
            .await
            .expect("test operation should succeed");
        // body reader type switches
        assert!(!http_stream.is_body_done());

        // now send ws data
        let ws_data = b"data";
        handle
            .tx
            .send(HttpTask::Body(Some(Bytes::from(&ws_data[..])), false))
            .await
            .expect("test operation should succeed");

        let buf = http_stream
            .read_body_bytes()
            .await
            .expect("test async operation should succeed")
            .expect("test operation should succeed");
        assert_eq!(buf, ws_data.as_slice());
        assert!(!http_stream.is_body_done());

        // EOF ends body
        drop(handle.tx);
        assert!(http_stream
            .read_body_bytes()
            .await
            .expect("test async operation should succeed")
            .is_none());
        assert!(http_stream.is_body_done());
    }
}

#[cfg(test)]
mod test_proxy_tasks {
    //! Cancel-safe proxy task API tests for the subrequest server session.

    use super::tests_stream::{build_head_req, build_req, build_upgrade_req};
    use super::*;
    use http::StatusCode;

    fn init_log() {
        let _ = env_logger::builder().is_test(true).try_init();
    }

    fn test_header(status: StatusCode) -> ResponseHeader {
        ResponseHeader::build(status, None)
            .expect("test status code should build a response header")
    }

    fn recv_task(rx: &mut mpsc::Receiver<HttpTask>) -> HttpTask {
        rx.try_recv()
            .expect("expected subrequest output task to be queued")
    }

    fn assert_rx_empty(rx: &mut mpsc::Receiver<HttpTask>) {
        assert!(matches!(
            rx.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
    }

    /// Dropping a blocked `send` does not deliver its value.
    #[tokio::test(start_paused = true)]
    async fn test_tokio_mpsc_send_cancel_drops_value() {
        let (tx, mut rx) = mpsc::channel::<u32>(1);
        tx.send(1)
            .await
            .expect("test async operation should succeed");
        let send_fut = tx.send(2);
        tokio::pin!(send_fut);
        tokio::select! {
            biased;
            _ = tokio::time::sleep(Duration::from_millis(10)) => {}
            _ = &mut send_fut => panic!("expected the timer to win"),
        };
        assert_eq!(rx.recv().await, Some(1));
        assert_eq!(rx.try_recv(), Err(mpsc::error::TryRecvError::Empty));
    }

    /// Dropping a blocked `reserve` does not consume capacity.
    #[tokio::test(start_paused = true)]
    async fn test_tokio_mpsc_reserve_cancel_releases_slot() {
        let (tx, mut rx) = mpsc::channel::<u32>(1);
        tx.send(1)
            .await
            .expect("test async operation should succeed");
        let reserve_fut = tx.reserve();
        tokio::pin!(reserve_fut);
        tokio::select! {
            biased;
            _ = tokio::time::sleep(Duration::from_millis(10)) => {}
            _ = &mut reserve_fut => panic!("expected the timer to win"),
        };
        assert_eq!(rx.recv().await, Some(1));
        assert_eq!(rx.try_recv(), Err(mpsc::error::TryRecvError::Empty));
    }

    #[tokio::test]
    async fn test_send_proxy_task_and_write() {
        init_log();
        let (mut session, mut handle) = build_req().await;
        session.set_proxy_tasks_enabled(true);
        assert!(session.proxy_tasks_enabled());

        let mut header = test_header(StatusCode::OK);
        header
            .insert_header("content-length", "5")
            .expect("test operation should succeed");
        session.send_proxy_task(HttpTask::Header(Box::new(header), false));
        session.send_proxy_task(HttpTask::Body(Some(Bytes::from("hello")), true));

        let end = session
            .write_proxy_tasks()
            .await
            .expect("test async operation should succeed");
        assert!(end);
        assert!(!session.has_pending_proxy_tasks());
        assert_eq!(session.body_bytes_sent(), 5);

        match recv_task(&mut handle.rx) {
            HttpTask::Header(h, false) => assert_eq!(h.status, StatusCode::OK),
            t => panic!("expected Header, got {t:?}"),
        }
        match recv_task(&mut handle.rx) {
            HttpTask::Body(Some(b), false) => assert_eq!(&b[..], b"hello"),
            t => panic!("expected Body, got {t:?}"),
        }
        assert!(matches!(recv_task(&mut handle.rx), HttpTask::Done));
        assert_rx_empty(&mut handle.rx);
    }

    #[tokio::test]
    async fn test_informational_head_does_not_init_body_writer() {
        init_log();

        let (mut regular, mut regular_handle) = build_head_req().await;
        regular
            .write_response_header(Box::new(test_header(StatusCode::CONTINUE)))
            .await
            .expect("regular informational header write should succeed");
        assert_eq!(regular.body_writer.body_mode, BodyMode::ToSelect);
        assert!(matches!(
            recv_task(&mut regular_handle.rx),
            HttpTask::Header(..)
        ));

        let (mut proxy, mut proxy_handle) = build_head_req().await;
        proxy.set_proxy_tasks_enabled(true);
        proxy.send_proxy_task(HttpTask::Header(
            Box::new(test_header(StatusCode::CONTINUE)),
            false,
        ));
        assert!(!proxy
            .write_proxy_tasks()
            .await
            .expect("proxy task write should succeed"));
        assert_eq!(proxy.body_writer.body_mode, BodyMode::ToSelect);
        assert!(matches!(
            recv_task(&mut proxy_handle.rx),
            HttpTask::Header(..)
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn test_proxy_task_with_timeout() {
        init_log();
        // Do not drain `handle.rx` before the first write; the 5th response task blocks on capacity.
        let (mut session, mut handle) = build_req().await;
        session.set_proxy_tasks_enabled(true);
        session.set_write_timeout(Some(Duration::from_millis(50)));

        let header = test_header(StatusCode::OK);
        session.send_proxy_task(HttpTask::Header(Box::new(header), false));
        for i in 0..5 {
            session.send_proxy_task(HttpTask::Body(
                Some(Bytes::from(format!("body-{i}"))),
                i == 4,
            ));
        }

        let err = session
            .write_proxy_tasks()
            .await
            .expect_err("full subrequest output channel should time out");
        assert_eq!(err.etype(), &WriteTimedout);
        assert!(session.has_pending_proxy_tasks());

        let mut delivered = Vec::new();
        while let Ok(task) = handle.rx.try_recv() {
            delivered.push(task);
        }
        assert_eq!(delivered.len(), 4);
        assert!(matches!(delivered[0], HttpTask::Header(..)));

        session.set_write_timeout(None);
        let end = session
            .write_proxy_tasks()
            .await
            .expect("retry after freeing channel capacity should complete");
        assert!(end);
        while let Ok(task) = handle.rx.try_recv() {
            delivered.push(task);
        }

        assert_eq!(delivered.len(), 7);
        assert!(matches!(delivered.last(), Some(HttpTask::Done)));
        let body_count = delivered
            .iter()
            .filter(|t| matches!(t, HttpTask::Body(..)))
            .count();
        assert_eq!(body_count, 5);
    }

    #[tokio::test]
    async fn test_proxy_task_channel_closed_errors() {
        init_log();
        let (mut session, handle) = build_req().await;
        session.set_proxy_tasks_enabled(true);
        drop(handle.rx);

        session.send_proxy_task(HttpTask::Header(
            Box::new(test_header(StatusCode::OK)),
            false,
        ));
        let err = session
            .write_proxy_tasks()
            .await
            .expect_err("closed subrequest output channel should error");
        assert_eq!(err.etype(), &WriteError);
        assert!(session.has_pending_proxy_tasks());
    }

    /// Repeatedly cancel while blocked on channel capacity, then verify the
    /// receiver sees each queued task exactly once and in order.
    #[tokio::test(start_paused = true)]
    async fn test_proxy_task_cancel_safety() {
        init_log();
        let (mut session, mut handle) = build_req().await;
        session.set_proxy_tasks_enabled(true);

        let mut header = test_header(StatusCode::OK);
        header
            .insert_header("content-length", "5")
            .expect("test operation should succeed");
        session.send_proxy_task(HttpTask::Header(Box::new(header), false));
        for i in 0..4 {
            session.send_proxy_task(HttpTask::Body(
                Some(Bytes::from(vec![b'A' + i as u8; 1])),
                false,
            ));
        }
        session.send_proxy_task(HttpTask::Body(Some(Bytes::from("E")), true));

        let mut cancel_count = 0;
        let mut delivered: Vec<HttpTask> = Vec::new();
        loop {
            if !session.has_pending_proxy_tasks() {
                break;
            }
            tokio::select! {
                biased;
                _ = tokio::time::sleep(Duration::from_millis(5)) => {
                    cancel_count += 1;
                    while let Ok(task) = handle.rx.try_recv() {
                        delivered.push(task);
                    }
                }
                result = session.write_proxy_tasks() => {
                    result.expect("test operation should succeed");
                }
            }
        }

        assert!(
            cancel_count >= 1,
            "expected at least one cancellation during cancel-safe write, got {cancel_count}"
        );

        assert_eq!(session.proxy_task_state.tasks.len(), 0);

        while let Ok(task) = handle.rx.try_recv() {
            delivered.push(task);
        }

        assert!(matches!(delivered[0], HttpTask::Header(_, false)));
        let mut body_bytes = Vec::new();
        let mut saw_done = false;
        for task in &delivered[1..] {
            match task {
                HttpTask::Body(Some(b), false) => body_bytes.extend_from_slice(b),
                HttpTask::Done => {
                    assert!(!saw_done, "Done delivered more than once");
                    saw_done = true;
                }
                t => panic!("unexpected task in delivery: {t:?}"),
            }
        }
        assert!(saw_done, "expected Done to be delivered");
        assert_eq!(
            body_bytes, b"ABCDE",
            "body chunks must arrive exactly once, in order"
        );

        assert_eq!(session.body_bytes_sent(), 5);
    }

    /// `was_upgraded()` must remain false if the 101 send is cancelled before
    /// it reaches the subrequest channel.
    #[tokio::test(start_paused = true)]
    async fn test_proxy_task_upgrade_consistency() {
        init_log();
        let (mut session, mut handle) = build_upgrade_req("websocket", "Upgrade").await;
        assert!(session.is_upgrade_req());
        session.set_proxy_tasks_enabled(true);

        // Four 1xx headers fill the upstream channel; the 101 then blocks.
        for _ in 0..4 {
            session.send_proxy_task(HttpTask::Header(
                Box::new(test_header(StatusCode::CONTINUE)),
                false,
            ));
        }
        let mut h101 = test_header(StatusCode::SWITCHING_PROTOCOLS);
        h101.set_version(http::Version::HTTP_11);
        h101.insert_header("upgrade", "websocket")
            .expect("test operation should succeed");
        h101.insert_header("connection", "Upgrade")
            .expect("test operation should succeed");
        session.send_proxy_task(HttpTask::Header(Box::new(h101), false));

        tokio::select! {
            biased;
            _ = tokio::time::sleep(Duration::from_millis(5)) => {}
            _ = session.write_proxy_tasks() => panic!("expected reserve to be cancelled"),
        };

        assert!(
            !session.was_upgraded(),
            "was_upgraded must remain false until the 101 send actually completes"
        );

        for _ in 0..4 {
            recv_task(&mut handle.rx);
        }
        session
            .write_proxy_tasks()
            .await
            .expect("test async operation should succeed");
        assert!(
            session.was_upgraded(),
            "after the 101 send completes, was_upgraded must be true"
        );
    }

    /// Same upgrade consistency check for the regular `write_response_header`
    /// path, which also awaits on the subrequest output channel.
    #[tokio::test(start_paused = true)]
    async fn test_write_response_header_upgrade_cancel_consistency() {
        init_log();
        let (mut session, mut handle) = build_upgrade_req("websocket", "Upgrade").await;

        for _ in 0..4 {
            session
                .write_response_header(Box::new(test_header(StatusCode::CONTINUE)))
                .await
                .expect("test operation should succeed");
        }

        let mut h101 = test_header(StatusCode::SWITCHING_PROTOCOLS);
        h101.set_version(http::Version::HTTP_11);
        h101.insert_header("upgrade", "websocket")
            .expect("test operation should succeed");
        h101.insert_header("connection", "Upgrade")
            .expect("test operation should succeed");

        tokio::select! {
            biased;
            _ = tokio::time::sleep(Duration::from_millis(5)) => {}
            _ = session.write_response_header(Box::new(h101.clone())) => {
                panic!("expected header send to be cancelled")
            }
        };
        assert!(!session.was_upgraded());
        assert_eq!(session.body_writer.body_mode, BodyMode::ToSelect);

        for _ in 0..4 {
            recv_task(&mut handle.rx);
        }
        session
            .write_response_header(Box::new(h101))
            .await
            .expect("test async operation should succeed");
        assert!(session.was_upgraded());
    }

    /// Trailers are dispatched correctly through `write_proxy_tasks`.
    /// Matching regular `response_duplex_vec`, a final `Done` follows Trailer.
    #[tokio::test]
    async fn test_proxy_task_trailers() {
        init_log();
        let (mut session, mut handle) = build_req().await;
        session.set_proxy_tasks_enabled(true);

        let header = test_header(StatusCode::OK);
        session.send_proxy_task(HttpTask::Header(Box::new(header), false));
        session.send_proxy_task(HttpTask::Body(Some(Bytes::from("hi")), false));
        let mut trailers = http::HeaderMap::new();
        trailers.insert("x-final", http::HeaderValue::from_static("done"));
        session.send_proxy_task(HttpTask::Trailer(Some(Box::new(trailers))));

        let end = session
            .write_proxy_tasks()
            .await
            .expect("test async operation should succeed");
        assert!(end);

        assert!(matches!(recv_task(&mut handle.rx), HttpTask::Header(..)));
        assert!(matches!(recv_task(&mut handle.rx), HttpTask::Body(..)));
        match recv_task(&mut handle.rx) {
            HttpTask::Trailer(Some(t)) => {
                assert_eq!(
                    t.get("x-final").expect("test trailer should be present"),
                    "done"
                );
            }
            t => panic!("expected Trailer, got {t:?}"),
        }
        assert!(matches!(recv_task(&mut handle.rx), HttpTask::Done));
        assert_rx_empty(&mut handle.rx);
    }

    #[tokio::test]
    async fn test_proxy_task_trailer_before_content_length_complete_errors() {
        init_log();
        let (mut session, mut handle) = build_req().await;
        session.set_proxy_tasks_enabled(true);

        let mut header = test_header(StatusCode::OK);
        header
            .insert_header("content-length", "5")
            .expect("test content-length header is valid");
        session.send_proxy_task(HttpTask::Header(Box::new(header), false));
        let mut trailers = HeaderMap::new();
        trailers.insert("x-final", http::HeaderValue::from_static("done"));
        session.send_proxy_task(HttpTask::Trailer(Some(Box::new(trailers))));

        let err = session
            .write_proxy_tasks()
            .await
            .expect_err("trailers before content-length body completion should error");
        assert_eq!(err.etype(), &PREMATURE_BODY_END);
        assert!(matches!(recv_task(&mut handle.rx), HttpTask::Header(..)));
        assert!(matches!(recv_task(&mut handle.rx), HttpTask::Trailer(..)));
        assert_rx_empty(&mut handle.rx);
    }

    #[tokio::test(start_paused = true)]
    async fn test_proxy_task_trailer_cancel_safety() {
        init_log();
        let (mut session, mut handle) = build_req().await;
        session.set_proxy_tasks_enabled(true);

        for _ in 0..4 {
            session.send_proxy_task(HttpTask::Header(
                Box::new(test_header(StatusCode::CONTINUE)),
                false,
            ));
        }
        let mut trailers = HeaderMap::new();
        trailers.insert("x-final", http::HeaderValue::from_static("done"));
        session.send_proxy_task(HttpTask::Trailer(Some(Box::new(trailers))));

        tokio::select! {
            biased;
            _ = tokio::time::sleep(Duration::from_millis(5)) => {}
            _ = session.write_proxy_tasks() => panic!("expected trailer reserve to be cancelled"),
        };

        let mut prefix = Vec::new();
        while let Ok(task) = handle.rx.try_recv() {
            prefix.push(task);
        }
        assert_eq!(prefix.len(), 4);
        assert!(prefix.iter().all(|t| matches!(t, HttpTask::Header(..))));
        let end = session
            .write_proxy_tasks()
            .await
            .expect("resume after trailer cancellation should complete");
        assert!(end);

        match recv_task(&mut handle.rx) {
            HttpTask::Trailer(Some(t)) => {
                assert_eq!(t.get("x-final").expect("trailer present"), "done")
            }
            t => panic!("expected Trailer after resume, got {t:?}"),
        }
        assert_rx_empty(&mut handle.rx);
    }

    #[tokio::test(start_paused = true)]
    async fn test_proxy_task_trailer_cancel_safety_after_body() {
        init_log();
        let (mut session, mut handle) = build_req().await;
        session.set_proxy_tasks_enabled(true);

        let header = test_header(StatusCode::OK);
        session.send_proxy_task(HttpTask::Header(Box::new(header), false));
        for _ in 0..3 {
            session.send_proxy_task(HttpTask::Body(Some(Bytes::from("x")), false));
        }
        let mut trailers = HeaderMap::new();
        trailers.insert("x-final", http::HeaderValue::from_static("done"));
        session.send_proxy_task(HttpTask::Trailer(Some(Box::new(trailers))));

        tokio::select! {
            biased;
            _ = tokio::time::sleep(Duration::from_millis(5)) => {}
            _ = session.write_proxy_tasks() => panic!("expected trailer reserve to be cancelled"),
        };

        let mut prefix = Vec::new();
        while let Ok(task) = handle.rx.try_recv() {
            prefix.push(task);
        }
        assert_eq!(prefix.len(), 4);
        assert!(matches!(prefix[0], HttpTask::Header(..)));
        assert!(prefix[1..].iter().all(|t| matches!(t, HttpTask::Body(..))));
        let end = session
            .write_proxy_tasks()
            .await
            .expect("resume after body trailer cancellation should complete");
        assert!(end);

        match recv_task(&mut handle.rx) {
            HttpTask::Trailer(Some(t)) => {
                assert_eq!(t.get("x-final").expect("trailer present"), "done")
            }
            t => panic!("expected Trailer after resume, got {t:?}"),
        }
        assert!(matches!(recv_task(&mut handle.rx), HttpTask::Done));
        assert_rx_empty(&mut handle.rx);
    }

    /// `body_bytes_sent` is only incremented after the synchronous
    /// `Permit::send`, not on a cancelled `reserve().await`.
    #[tokio::test(start_paused = true)]
    async fn test_proxy_task_body_counter_no_double_count() {
        init_log();
        let (mut session, mut handle) = build_req().await;
        session.set_proxy_tasks_enabled(true);

        let mut header = test_header(StatusCode::OK);
        header
            .insert_header("content-length", "12")
            .expect("test operation should succeed");
        session.send_proxy_task(HttpTask::Header(Box::new(header), false));
        session.send_proxy_task(HttpTask::Body(Some(Bytes::from("AAAA")), false));
        session.send_proxy_task(HttpTask::Body(Some(Bytes::from("BBBB")), false));
        session.send_proxy_task(HttpTask::Body(Some(Bytes::from("CCCC")), true));

        let mut received_body_bytes = Vec::new();
        loop {
            if !session.has_pending_proxy_tasks() {
                break;
            }
            tokio::select! {
                biased;
                _ = tokio::time::sleep(Duration::from_millis(5)) => {
                    while let Ok(task) = handle.rx.try_recv() {
                        if let HttpTask::Body(Some(b), _) = &task {
                            received_body_bytes.extend_from_slice(b);
                        }
                    }
                    assert!(
                        session.body_bytes_sent() <= received_body_bytes.len(),
                        "body_bytes_sent ({}) must not exceed bytes actually delivered ({})",
                        session.body_bytes_sent(),
                        received_body_bytes.len(),
                    );
                }
                result = session.write_proxy_tasks() => {
                    result.expect("test operation should succeed");
                }
            }
        }

        while let Ok(task) = handle.rx.try_recv() {
            if let HttpTask::Body(Some(b), _) = &task {
                received_body_bytes.extend_from_slice(b);
            }
        }

        assert_eq!(session.body_bytes_sent(), 12);
        assert_eq!(&received_body_bytes[..], b"AAAABBBBCCCC");
    }

    /// Cancelling while reserving capacity for the final Done must leave the
    /// finish operation resumable.
    #[tokio::test(start_paused = true)]
    async fn test_proxy_task_finish_cancel_safety() {
        init_log();
        let (mut session, mut handle) = build_req().await;
        session.set_proxy_tasks_enabled(true);

        let header = test_header(StatusCode::OK);
        session.send_proxy_task(HttpTask::Header(Box::new(header), false));
        session.send_proxy_task(HttpTask::Body(Some(Bytes::from("a")), false));
        session.send_proxy_task(HttpTask::Body(Some(Bytes::from("b")), false));
        session.send_proxy_task(HttpTask::Body(Some(Bytes::from("c")), true));

        // Header + three bodies fill the 4-slot channel. The final Done
        // reserve blocks and is cancelled.
        tokio::select! {
            biased;
            _ = tokio::time::sleep(Duration::from_millis(5)) => {}
            _ = session.write_proxy_tasks() => panic!("expected finish reserve to be cancelled"),
        };
        assert!(session.proxy_task_state.finish_in_progress);

        let first = recv_task(&mut handle.rx);
        assert!(matches!(first, HttpTask::Header(..)));

        // Only one slot is available. Resuming must emit exactly one Done and
        // return without trying to reserve a second slot.
        let end = tokio::time::timeout(Duration::from_millis(5), session.write_proxy_tasks())
            .await
            .expect("resume should not need a second channel slot")
            .expect("test operation should succeed");
        assert!(end);

        let mut delivered = Vec::new();
        while let Ok(task) = handle.rx.try_recv() {
            delivered.push(task);
        }
        assert_eq!(delivered.len(), 4);
        assert!(matches!(delivered.last(), Some(HttpTask::Done)));
        assert_eq!(
            delivered
                .iter()
                .filter(|t| matches!(t, HttpTask::Done))
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn test_proxy_task_done_only_noops_without_channel() {
        init_log();
        let (mut session, _handle) = build_req().await;
        session.set_proxy_tasks_enabled(true);
        session.send_proxy_task(HttpTask::Done);
        session.shutdown();

        let end = session
            .write_proxy_tasks()
            .await
            .expect("test async operation should succeed");
        assert!(end);
    }

    #[tokio::test]
    async fn test_proxy_task_done_only_noops_with_live_channel() {
        init_log();
        let (mut session, mut handle) = build_req().await;
        session.set_proxy_tasks_enabled(true);
        session.send_proxy_task(HttpTask::Done);

        let end = session
            .write_proxy_tasks()
            .await
            .expect("Done-only proxy task should complete without channel output");
        assert!(end);
        assert!(!session.has_pending_proxy_tasks());
        assert_rx_empty(&mut handle.rx);
    }

    #[tokio::test]
    async fn test_proxy_task_header_only_end() {
        init_log();
        let (mut session, mut handle) = build_req().await;
        session.set_proxy_tasks_enabled(true);

        let header = test_header(StatusCode::NO_CONTENT);
        session.send_proxy_task(HttpTask::Header(Box::new(header), true));

        let end = session
            .write_proxy_tasks()
            .await
            .expect("test async operation should succeed");
        assert!(end);
        assert!(matches!(recv_task(&mut handle.rx), HttpTask::Header(..)));
        assert!(matches!(recv_task(&mut handle.rx), HttpTask::Done));
        assert_rx_empty(&mut handle.rx);
    }

    #[tokio::test]
    async fn test_proxy_task_head_response_drops_body() {
        init_log();
        let (mut session, mut handle) = build_head_req().await;
        session.set_proxy_tasks_enabled(true);

        let mut header = test_header(StatusCode::OK);
        header
            .insert_header("content-length", "10")
            .expect("test content-length header is valid");
        session.send_proxy_task(HttpTask::Header(Box::new(header), false));
        session.send_proxy_task(HttpTask::Body(Some(Bytes::from("not-sent")), true));

        let end = session
            .write_proxy_tasks()
            .await
            .expect("HEAD proxy task response should complete");
        assert!(end);
        assert!(matches!(recv_task(&mut handle.rx), HttpTask::Header(..)));
        assert!(matches!(recv_task(&mut handle.rx), HttpTask::Done));
        assert_rx_empty(&mut handle.rx);
        assert_eq!(session.body_bytes_sent(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn test_proxy_task_duplicate_final_header_does_not_reserve() {
        init_log();
        let (mut session, _handle) = build_req().await;
        session.set_proxy_tasks_enabled(true);

        let header = test_header(StatusCode::OK);
        session.send_proxy_task(HttpTask::Header(Box::new(header), false));
        session
            .write_proxy_tasks()
            .await
            .expect("test async operation should succeed");

        for _ in 0..3 {
            session.send_proxy_task(HttpTask::Body(Some(Bytes::from("x")), false));
        }
        let duplicate = test_header(StatusCode::OK);
        session.send_proxy_task(HttpTask::Header(Box::new(duplicate), false));

        tokio::time::timeout(Duration::from_millis(5), session.write_proxy_tasks())
            .await
            .expect("duplicate final header should be dropped without reserving")
            .expect("test operation should succeed");
        assert!(!session.has_pending_proxy_tasks());
    }

    #[tokio::test]
    async fn test_proxy_task_duplicate_final_header_preserves_end() {
        init_log();
        let (mut session, mut handle) = build_req().await;
        session.set_proxy_tasks_enabled(true);

        let header = test_header(StatusCode::OK);
        session.send_proxy_task(HttpTask::Header(Box::new(header), false));
        session
            .write_proxy_tasks()
            .await
            .expect("test async operation should succeed");
        recv_task(&mut handle.rx);

        let duplicate = test_header(StatusCode::OK);
        session.send_proxy_task(HttpTask::Header(Box::new(duplicate), true));

        let end = session
            .write_proxy_tasks()
            .await
            .expect("test async operation should succeed");
        assert!(end);
        assert!(matches!(recv_task(&mut handle.rx), HttpTask::Done));
        assert_rx_empty(&mut handle.rx);
    }

    #[tokio::test]
    async fn test_proxy_task_failed_propagates_without_sending_later_tasks() {
        init_log();
        let (mut session, mut handle) = build_req().await;
        session.set_proxy_tasks_enabled(true);
        session.send_proxy_task(HttpTask::Failed(Error::explain(InternalError, "boom")));
        session.send_proxy_task(HttpTask::Body(Some(Bytes::from("late")), true));

        let err = session
            .write_proxy_tasks()
            .await
            .expect_err("Failed proxy task should propagate error");
        assert_eq!(err.etype(), &InternalError);
        assert!(session.has_pending_proxy_tasks());
        assert_rx_empty(&mut handle.rx);
    }

    #[tokio::test]
    async fn test_proxy_task_failed_clears_sticky_eos() {
        init_log();
        let (mut session, mut handle) = build_req().await;
        session.set_proxy_tasks_enabled(true);

        let header = test_header(StatusCode::OK);
        session.send_proxy_task(HttpTask::Header(Box::new(header), false));
        session.send_proxy_task(HttpTask::Body(None, true));
        session.send_proxy_task(HttpTask::Failed(Error::explain(InternalError, "boom")));

        let err = session
            .write_proxy_tasks()
            .await
            .expect_err("Failed after EOS should still propagate error");
        assert_eq!(err.etype(), &InternalError);
        assert!(!session.has_pending_proxy_tasks());
        assert!(matches!(recv_task(&mut handle.rx), HttpTask::Header(..)));
        assert_rx_empty(&mut handle.rx);

        let end = session
            .write_proxy_tasks()
            .await
            .expect("retry after Failed should not emit stale Done");
        assert!(!end);
        assert_rx_empty(&mut handle.rx);
    }

    #[tokio::test]
    async fn test_proxy_task_body_before_header_errors() {
        init_log();
        let (mut session, mut handle) = build_req().await;
        session.set_proxy_tasks_enabled(true);
        session.send_proxy_task(HttpTask::Body(Some(Bytes::from("body")), true));

        let err = session
            .write_proxy_tasks()
            .await
            .expect_err("body before response header should be rejected");
        assert_eq!(err.etype(), &InternalError);
        assert!(!session.has_pending_proxy_tasks());
        assert_rx_empty(&mut handle.rx);
    }

    #[tokio::test(start_paused = true)]
    async fn test_proxy_task_none_body_does_not_reserve() {
        init_log();
        let (mut session, _handle) = build_req().await;
        session.set_proxy_tasks_enabled(true);

        let header = test_header(StatusCode::OK);
        session.send_proxy_task(HttpTask::Header(Box::new(header), false));
        for _ in 0..3 {
            session.send_proxy_task(HttpTask::Body(Some(Bytes::from("x")), false));
        }
        session.send_proxy_task(HttpTask::Body(None, false));

        tokio::time::timeout(Duration::from_millis(5), session.write_proxy_tasks())
            .await
            .expect("Body(None) should not reserve channel capacity")
            .expect("test operation should succeed");
        assert!(!session.has_pending_proxy_tasks());
    }

    #[tokio::test(start_paused = true)]
    async fn test_proxy_task_no_data_eos_survives_cancellation() {
        init_log();
        let (mut session, mut handle) = build_req().await;
        session.set_proxy_tasks_enabled(true);

        let header = test_header(StatusCode::OK);
        session.send_proxy_task(HttpTask::Header(Box::new(header), false));
        for _ in 0..3 {
            session.send_proxy_task(HttpTask::Body(Some(Bytes::from("x")), false));
        }
        session.send_proxy_task(HttpTask::Body(None, true));
        session.send_proxy_task(HttpTask::Body(Some(Bytes::new()), true));
        // This later body blocks on the full channel after the no-data EOS
        // task has been consumed. The sticky EOS flag must survive that cancel.
        session.send_proxy_task(HttpTask::Body(Some(Bytes::from("y")), false));

        tokio::select! {
            biased;
            _ = tokio::time::sleep(Duration::from_millis(5)) => {}
            _ = session.write_proxy_tasks() => panic!("expected reserve after no-data EOS to be cancelled"),
        };

        let mut prefix = Vec::new();
        while let Ok(task) = handle.rx.try_recv() {
            prefix.push(task);
        }
        assert_eq!(prefix.len(), 4);
        assert!(matches!(prefix[0], HttpTask::Header(..)));
        assert!(prefix[1..].iter().all(|t| matches!(t, HttpTask::Body(..))));
        let end = session
            .write_proxy_tasks()
            .await
            .expect("resume after no-data EOS cancellation should complete");
        assert!(end);

        let mut delivered = Vec::new();
        while let Ok(task) = handle.rx.try_recv() {
            delivered.push(task);
        }
        assert_eq!(delivered.len(), 2);
        match &delivered[0] {
            HttpTask::Body(Some(b), false) => assert_eq!(&b[..], b"y"),
            t => panic!("expected Body(y) after resume, got {t:?}"),
        }
        assert!(matches!(delivered[1], HttpTask::Done));
        assert_eq!(session.body_bytes_sent(), 4);
    }

    #[tokio::test]
    async fn test_proxy_task_content_length_overrun_truncates() {
        init_log();
        let (mut session, mut handle) = build_req().await;
        session.set_proxy_tasks_enabled(true);

        let mut header = test_header(StatusCode::OK);
        header
            .insert_header("content-length", "3")
            .expect("test operation should succeed");
        session.send_proxy_task(HttpTask::Header(Box::new(header), false));
        session.send_proxy_task(HttpTask::Body(Some(Bytes::from("abcdef")), true));

        let end = session
            .write_proxy_tasks()
            .await
            .expect("test async operation should succeed");
        assert!(end);
        assert!(matches!(recv_task(&mut handle.rx), HttpTask::Header(..)));
        match recv_task(&mut handle.rx) {
            HttpTask::Body(Some(b), false) => assert_eq!(&b[..], b"abc"),
            t => panic!("expected truncated Body, got {t:?}"),
        }
        assert!(matches!(recv_task(&mut handle.rx), HttpTask::Done));
        assert_rx_empty(&mut handle.rx);
        assert_eq!(session.body_bytes_sent(), 3);
    }

    #[tokio::test]
    async fn test_proxy_task_exact_content_length_without_end_sends_done() {
        init_log();
        let (mut session, mut handle) = build_req().await;
        session.set_proxy_tasks_enabled(true);

        let mut header = test_header(StatusCode::OK);
        header
            .insert_header("content-length", "3")
            .expect("test content-length header is valid");
        session.send_proxy_task(HttpTask::Header(Box::new(header), false));
        session.send_proxy_task(HttpTask::Body(Some(Bytes::from("abc")), false));

        let end = session
            .write_proxy_tasks()
            .await
            .expect("exact content-length proxy task response should complete");
        assert!(end);
        assert!(matches!(recv_task(&mut handle.rx), HttpTask::Header(..)));
        match recv_task(&mut handle.rx) {
            HttpTask::Body(Some(b), false) => assert_eq!(&b[..], b"abc"),
            t => panic!("expected exact content-length Body, got {t:?}"),
        }
        assert!(matches!(recv_task(&mut handle.rx), HttpTask::Done));
        assert_rx_empty(&mut handle.rx);
    }

    #[tokio::test]
    async fn test_proxy_task_late_tasks_after_finished_are_dropped() {
        init_log();
        let (mut session, mut handle) = build_req().await;
        session.set_proxy_tasks_enabled(true);

        let mut header = test_header(StatusCode::OK);
        header
            .insert_header("content-length", "3")
            .expect("test content-length header is valid");
        session.send_proxy_task(HttpTask::Header(Box::new(header), false));
        session.send_proxy_task(HttpTask::Body(Some(Bytes::from("abc")), true));
        session
            .write_proxy_tasks()
            .await
            .expect("initial response should complete");

        assert!(matches!(recv_task(&mut handle.rx), HttpTask::Header(..)));
        assert!(matches!(recv_task(&mut handle.rx), HttpTask::Body(..)));
        assert!(matches!(recv_task(&mut handle.rx), HttpTask::Done));

        let mut trailers = HeaderMap::new();
        trailers.insert("x-late", http::HeaderValue::from_static("ignored"));
        session.send_proxy_task(HttpTask::Trailer(Some(Box::new(trailers))));
        session.send_proxy_task(HttpTask::Body(Some(Bytes::from("late")), true));
        session.send_proxy_task(HttpTask::Failed(Error::explain(InternalError, "late")));

        let end = session
            .write_proxy_tasks()
            .await
            .expect("late tasks after finished stream should be dropped");
        assert!(end);
        assert_rx_empty(&mut handle.rx);
        assert!(!session.has_pending_proxy_tasks());
    }

    #[tokio::test]
    async fn test_proxy_task_chunked_header_uses_until_close() {
        init_log();
        let (mut session, mut handle) = build_req().await;
        session.set_proxy_tasks_enabled(true);

        let mut header = test_header(StatusCode::OK);
        header
            .insert_header("transfer-encoding", "chunked")
            .expect("test transfer-encoding header is valid");
        session.send_proxy_task(HttpTask::Header(Box::new(header), false));
        session.send_proxy_task(HttpTask::Body(Some(Bytes::from("chunk")), true));

        let end = session
            .write_proxy_tasks()
            .await
            .expect("chunked proxy task response should complete");
        assert!(end);
        assert!(matches!(recv_task(&mut handle.rx), HttpTask::Header(..)));
        match recv_task(&mut handle.rx) {
            HttpTask::Body(Some(b), false) => assert_eq!(&b[..], b"chunk"),
            t => panic!("expected chunked body task, got {t:?}"),
        }
        assert!(matches!(recv_task(&mut handle.rx), HttpTask::Done));
        assert_eq!(session.body_bytes_sent(), 5);
    }

    #[tokio::test]
    async fn test_proxy_task_premature_content_length_errors_before_done() {
        init_log();
        let (mut session, mut handle) = build_req().await;
        session.set_proxy_tasks_enabled(true);

        let mut header = test_header(StatusCode::OK);
        header
            .insert_header("content-length", "5")
            .expect("test operation should succeed");
        session.send_proxy_task(HttpTask::Header(Box::new(header), false));
        session.send_proxy_task(HttpTask::Body(Some(Bytes::from("hi")), true));

        let err = session
            .write_proxy_tasks()
            .await
            .expect_err("premature content-length should error before Done");
        assert_eq!(err.etype(), &PREMATURE_BODY_END);
        assert_eq!(session.body_writer.body_mode, BodyMode::Complete(2));
        assert!(!session.has_pending_proxy_tasks());
        assert!(matches!(recv_task(&mut handle.rx), HttpTask::Header(..)));
        match recv_task(&mut handle.rx) {
            HttpTask::Body(Some(b), false) => assert_eq!(&b[..], b"hi"),
            t => panic!("expected body before premature end, got {t:?}"),
        }
        assert_rx_empty(&mut handle.rx);
    }

    #[tokio::test]
    #[should_panic(
        expected = "Unexpected UpgradedBody task received on un-upgraded downstream session"
    )]
    async fn test_upgraded_body_on_non_upgraded_session_panics() {
        init_log();
        let (mut session, _handle) = build_req().await;
        session.set_proxy_tasks_enabled(true);
        assert!(!session.was_upgraded());

        let header = test_header(StatusCode::OK);
        session.send_proxy_task(HttpTask::Header(Box::new(header), false));
        session.send_proxy_task(HttpTask::UpgradedBody(Some(Bytes::from("ws")), true));

        let _ = session.write_proxy_tasks().await;
    }

    #[tokio::test]
    #[should_panic(expected = "Unexpected Body task received on upgraded downstream session")]
    async fn test_body_on_upgraded_session_panics() {
        init_log();
        let (mut session, _handle) = build_upgrade_req("websocket", "Upgrade").await;
        session.set_proxy_tasks_enabled(true);

        let mut h101 = test_header(StatusCode::SWITCHING_PROTOCOLS);
        h101.set_version(http::Version::HTTP_11);
        h101.insert_header("upgrade", "websocket")
            .expect("test operation should succeed");
        h101.insert_header("connection", "Upgrade")
            .expect("test operation should succeed");
        session.send_proxy_task(HttpTask::Header(Box::new(h101), false));
        session
            .write_proxy_tasks()
            .await
            .expect("test async operation should succeed");

        session.send_proxy_task(HttpTask::Body(Some(Bytes::from("plain")), true));
        let _ = session.write_proxy_tasks().await;
    }

    #[tokio::test]
    async fn test_proxy_task_upgraded_body_happy_path() {
        init_log();
        let (mut session, mut handle) = build_upgrade_req("websocket", "Upgrade").await;
        session.set_proxy_tasks_enabled(true);

        let mut h101 = test_header(StatusCode::SWITCHING_PROTOCOLS);
        h101.set_version(http::Version::HTTP_11);
        h101.insert_header("upgrade", "websocket")
            .expect("test upgrade header is valid");
        h101.insert_header("connection", "Upgrade")
            .expect("test connection header is valid");
        session.send_proxy_task(HttpTask::Header(Box::new(h101), false));
        session.send_proxy_task(HttpTask::UpgradedBody(Some(Bytes::from("ws")), true));

        let end = session
            .write_proxy_tasks()
            .await
            .expect("upgraded proxy task response should complete");
        assert!(end);
        assert!(session.was_upgraded());
        assert!(matches!(recv_task(&mut handle.rx), HttpTask::Header(..)));
        match recv_task(&mut handle.rx) {
            HttpTask::Body(Some(b), false) => assert_eq!(&b[..], b"ws"),
            t => panic!("expected upgraded body task, got {t:?}"),
        }
        assert!(matches!(recv_task(&mut handle.rx), HttpTask::Done));
        assert_eq!(session.body_bytes_sent(), 2);
    }

    #[tokio::test]
    #[should_panic(
        expected = "Unexpected UpgradedBody task received on un-upgraded downstream session"
    )]
    async fn test_upgraded_body_on_non_upgraded_session_panics_while_full() {
        init_log();
        let (mut session, _handle) = build_req().await;
        session.set_proxy_tasks_enabled(true);

        for _ in 0..4 {
            session.send_proxy_task(HttpTask::Header(
                Box::new(test_header(StatusCode::CONTINUE)),
                false,
            ));
        }
        session.send_proxy_task(HttpTask::UpgradedBody(Some(Bytes::from("ws")), true));
        let _ = session.write_proxy_tasks().await;
    }
}
