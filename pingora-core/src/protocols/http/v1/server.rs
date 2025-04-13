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

//! HTTP/1.x server session

use bytes::Bytes;
use bytes::{BufMut, BytesMut};
use http::header::{CONTENT_LENGTH, TRANSFER_ENCODING};
use http::HeaderValue;
use http::{header, header::AsHeaderName, Method, Version};
use log::{debug, warn};
use once_cell::sync::Lazy;
use percent_encoding::{percent_encode, AsciiSet, CONTROLS};
use pingora_error::{Error, ErrorType::*, OrErr, Result};
use pingora_http::{IntoCaseHeaderName, RequestHeader, ResponseHeader};
use pingora_timeout::timeout;
use regex::bytes::Regex;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::body::{BodyReader, BodyWriter};
use super::common::*;
use crate::protocols::http::{body_buffer::FixedBuffer, date, HttpTask};
use crate::protocols::{Digest, SocketAddr, Stream};
use crate::utils::{BufRef, KVRef};

/// The HTTP 1.x server session
pub struct HttpSession {
    underlying_stream: Stream,
    /// The buf that holds the raw request header + possibly a portion of request body
    /// Request body can appear here because they could arrive with the same read() that
    /// sends the request header.
    buf: Bytes,
    /// A slice reference to `buf` which points to the exact range of request header
    raw_header: Option<BufRef>,
    /// A slice reference to `buf` which points to the range of a portion of request body if any
    preread_body: Option<BufRef>,
    /// A state machine to track how to read the request body
    body_reader: BodyReader,
    /// A state machine to track how to write the response body
    body_writer: BodyWriter,
    /// An internal buffer to buf multiple body writes to reduce the underlying syscalls
    body_write_buf: BytesMut,
    /// Track how many application (not on the wire) body bytes already sent
    body_bytes_sent: usize,
    /// Track how many application (not on the wire) body bytes already read
    body_bytes_read: usize,
    /// Whether to update headers like connection, Date
    update_resp_headers: bool,
    /// timeouts:
    keepalive_timeout: KeepaliveStatus,
    read_timeout: Option<Duration>,
    write_timeout: Option<Duration>,
    /// How long to wait to make downstream session reusable, if body needs to be drained.
    total_drain_timeout: Option<Duration>,
    /// A copy of the response that is already written to the client
    response_written: Option<Box<ResponseHeader>>,
    /// The parsed request header
    request_header: Option<Box<RequestHeader>>,
    /// An internal buffer that holds a copy of the request body up to a certain size
    retry_buffer: Option<FixedBuffer>,
    /// Whether this session is an upgraded session. This flag is calculated when sending the
    /// response header to the client.
    upgraded: bool,
    /// Digest to track underlying connection metrics
    digest: Box<Digest>,
    /// Minimum send rate to the client
    min_send_rate: Option<usize>,
    /// When this is enabled informational response headers will not be proxied downstream
    ignore_info_resp: bool,
}

impl HttpSession {
    /// Create a new http server session from an established (TCP or TLS) [`Stream`].
    /// The created session needs to call [`Self::read_request()`] first before performing
    /// any other operations.
    pub fn new(underlying_stream: Stream) -> Self {
        // TODO: maybe we should put digest in the connection itself
        let digest = Box::new(Digest {
            ssl_digest: underlying_stream.get_ssl_digest(),
            timing_digest: underlying_stream.get_timing_digest(),
            proxy_digest: underlying_stream.get_proxy_digest(),
            socket_digest: underlying_stream.get_socket_digest(),
        });

        HttpSession {
            underlying_stream,
            buf: Bytes::new(), // zero size, with be replaced by parsed header later
            raw_header: None,
            preread_body: None,
            body_reader: BodyReader::new(),
            body_writer: BodyWriter::new(),
            body_write_buf: BytesMut::new(),
            keepalive_timeout: KeepaliveStatus::Off,
            update_resp_headers: true,
            response_written: None,
            request_header: None,
            read_timeout: None,
            write_timeout: None,
            total_drain_timeout: None,
            body_bytes_sent: 0,
            body_bytes_read: 0,
            retry_buffer: None,
            upgraded: false,
            digest,
            min_send_rate: None,
            ignore_info_resp: false,
        }
    }

    /// Read the request header. Return `Ok(Some(n))` where the read and parsing are successful.
    /// Return `Ok(None)` when the client closed the connection without sending any data, which
    /// is common on a reused connection.
    pub async fn read_request(&mut self) -> Result<Option<usize>> {
        const MAX_ERR_BUF_LEN: usize = 2048;

        self.buf.clear();
        let mut buf = BytesMut::with_capacity(INIT_HEADER_BUF_SIZE);
        let mut already_read: usize = 0;
        loop {
            if already_read > MAX_HEADER_SIZE {
                /* NOTE: this check only blocks second read. The first large read is allowed
                since the buf is already allocated. The goal is to avoid slowly bloating
                this buffer */
                return Error::e_explain(
                    InvalidHTTPHeader,
                    format!("Request header larger than {MAX_HEADER_SIZE}"),
                );
            }

            let read_result = {
                let read_event = self.underlying_stream.read_buf(&mut buf);
                match self.keepalive_timeout {
                    KeepaliveStatus::Timeout(d) => match timeout(d, read_event).await {
                        Ok(res) => res,
                        Err(e) => {
                            debug!("keepalive timeout {d:?} reached, {e}");
                            return Ok(None);
                        }
                    },
                    _ => read_event.await,
                }
            };
            let n = match read_result {
                Ok(n_read) => {
                    if n_read == 0 {
                        if already_read > 0 {
                            return Error::e_explain(
                                ConnectionClosed,
                                format!(
                                    "while reading request headers, bytes already read: {}",
                                    already_read
                                ),
                            );
                        } else {
                            /* common when client decides to close a keepalived session */
                            debug!("Client prematurely closed connection with 0 byte sent");
                            return Ok(None);
                        }
                    }
                    n_read
                }

                Err(e) => {
                    if already_read > 0 {
                        return Error::e_because(ReadError, "while reading request headers", e);
                    }
                    /* nothing harmful since we have not ready any thing yet */
                    return Ok(None);
                }
            };
            already_read += n;

            // Use loop as GOTO to retry escaped request buffer, not a real loop
            loop {
                let mut headers = [httparse::EMPTY_HEADER; MAX_HEADERS];
                let mut req = httparse::Request::new(&mut headers);
                let parsed = parse_req_buffer(&mut req, &buf);
                match parsed {
                    HeaderParseState::Complete(s) => {
                        self.raw_header = Some(BufRef(0, s));
                        self.preread_body = Some(BufRef(s, already_read));

                        // We have the header name and values we parsed to be just 0 copy Bytes
                        // referencing the original buf. That requires we convert the buf from
                        // BytesMut to Bytes. But `req` holds a reference to `buf`. So we use the
                        // `KVRef`s to record the offset of each piece of data, drop `req`, convert
                        // buf, the do the 0 copy update
                        let base = buf.as_ptr() as usize;
                        let mut header_refs = Vec::<KVRef>::with_capacity(req.headers.len());
                        // Note: req.headers has the correct number of headers
                        // while header_refs doesn't as it is still empty
                        let _num_headers = populate_headers(base, &mut header_refs, req.headers);

                        let mut request_header = Box::new(RequestHeader::build(
                            req.method.unwrap_or(""),
                            // we path httparse to allow unsafe bytes in the str
                            req.path.unwrap_or("").as_bytes(),
                            Some(req.headers.len()),
                        )?);

                        request_header.set_version(match req.version {
                            Some(1) => Version::HTTP_11,
                            Some(0) => Version::HTTP_10,
                            _ => Version::HTTP_09,
                        });

                        let buf = buf.freeze();

                        for header in header_refs {
                            let header_name = header.get_name_bytes(&buf);
                            let header_name = header_name.into_case_header_name();
                            let value_bytes = header.get_value_bytes(&buf);
                            // safe because this is from what we parsed
                            let header_value = unsafe {
                                http::HeaderValue::from_maybe_shared_unchecked(value_bytes)
                            };

                            request_header
                                .append_header(header_name, header_value)
                                .or_err(InvalidHTTPHeader, "while parsing request header")?;
                        }

                        let contains_transfer_encoding =
                            request_header.headers.contains_key(TRANSFER_ENCODING);
                        let contains_content_length =
                            request_header.headers.contains_key(CONTENT_LENGTH);

                        // Transfer encoding overrides content length, so when
                        // both are present, we can remove content length. This
                        // is per https://datatracker.ietf.org/doc/html/rfc9112#section-6.3
                        if contains_content_length && contains_transfer_encoding {
                            request_header.remove_header(&CONTENT_LENGTH);
                        }

                        self.buf = buf;
                        self.request_header = Some(request_header);

                        self.body_reader.reinit();
                        self.response_written = None;
                        self.respect_keepalive();
                        self.validate_request()?;

                        return Ok(Some(s));
                    }
                    HeaderParseState::Partial => {
                        break; /* continue the read loop */
                    }
                    HeaderParseState::Invalid(e) => match e {
                        httparse::Error::Token | httparse::Error::Version => {
                            // try to escape URI
                            if let Some(new_buf) = escape_illegal_request_line(&buf) {
                                buf = new_buf;
                                already_read = buf.len();
                            } else {
                                debug!("Invalid request header from {:?}", self.underlying_stream);
                                buf.truncate(MAX_ERR_BUF_LEN);
                                return Error::e_because(
                                    InvalidHTTPHeader,
                                    format!(
                                        "buf: {}",
                                        String::from_utf8_lossy(&buf).escape_default()
                                    ),
                                    e,
                                );
                            }
                        }
                        _ => {
                            debug!("Invalid request header from {:?}", self.underlying_stream);
                            buf.truncate(MAX_ERR_BUF_LEN);
                            return Error::e_because(
                                InvalidHTTPHeader,
                                format!("buf: {}", String::from_utf8_lossy(&buf).escape_default()),
                                e,
                            );
                        }
                    },
                }
            }
        }
    }

    // Validate the request header read. This function must be called after the request header
    // read.
    fn validate_request(&self) -> Result<()> {
        let req_header = self.req_header();

        // ad-hoc checks
        super::common::check_dup_content_length(&req_header.headers)?;

        Ok(())
    }

    /// Return a reference of the `RequestHeader` this session read
    /// # Panics
    /// this function and most other functions will panic if called before [`Self::read_request()`]
    pub fn req_header(&self) -> &RequestHeader {
        self.request_header
            .as_ref()
            .expect("Request header is not read yet")
    }

    /// Return a mutable reference of the `RequestHeader` this session read
    /// # Panics
    /// this function and most other functions will panic if called before [`Self::read_request()`]
    pub fn req_header_mut(&mut self) -> &mut RequestHeader {
        self.request_header
            .as_mut()
            .expect("Request header is not read yet")
    }

    /// Get the header value for the given header name
    /// If there are multiple headers under the same name, the first one will be returned
    /// Use `self.req_header().header.get_all(name)` to get all the headers under the same name
    pub fn get_header(&self, name: impl AsHeaderName) -> Option<&HeaderValue> {
        self.request_header
            .as_ref()
            .and_then(|h| h.headers.get(name))
    }

    /// Return the method of this request. None if the request is not read yet.
    pub(super) fn get_method(&self) -> Option<&http::Method> {
        self.request_header.as_ref().map(|r| &r.method)
    }

    /// Return the path of the request (i.e., the `/hello?1` of `GET /hello?1 HTTP1.1`)
    /// An empty slice will be used if there is no path or the request is not read yet
    pub(super) fn get_path(&self) -> &[u8] {
        self.request_header.as_ref().map_or(b"", |r| r.raw_path())
    }

    /// Return the host header of the request. An empty slice will be used if there is no host header
    pub(super) fn get_host(&self) -> &[u8] {
        self.request_header
            .as_ref()
            .and_then(|h| h.headers.get(header::HOST))
            .map_or(b"", |h| h.as_bytes())
    }

    /// Return a string `$METHOD $PATH, Host: $HOST`. Mostly for logging and debug purpose
    pub fn request_summary(&self) -> String {
        format!(
            "{} {}, Host: {}",
            self.get_method().map_or("-", |r| r.as_str()),
            String::from_utf8_lossy(self.get_path()),
            String::from_utf8_lossy(self.get_host())
        )
    }

    /// Is the request a upgrade request
    pub fn is_upgrade_req(&self) -> bool {
        match self.request_header.as_deref() {
            Some(req) => is_upgrade_req(req),
            None => false,
        }
    }

    /// Get the request header as raw bytes, `b""` when the header doesn't exist
    pub fn get_header_bytes(&self, name: impl AsHeaderName) -> &[u8] {
        self.get_header(name).map_or(b"", |v| v.as_bytes())
    }

    /// Read the request body. `Ok(None)` when there is no (more) body to read.
    pub async fn read_body_bytes(&mut self) -> Result<Option<Bytes>> {
        let read = self.read_body().await?;
        Ok(read.map(|b| {
            let bytes = Bytes::copy_from_slice(self.get_body(&b));
            self.body_bytes_read += bytes.len();
            if let Some(buffer) = self.retry_buffer.as_mut() {
                buffer.write_to_buffer(&bytes);
            }
            bytes
        }))
    }

    async fn do_read_body(&mut self) -> Result<Option<BufRef>> {
        self.init_body_reader();
        self.body_reader
            .read_body(&mut self.underlying_stream)
            .await
    }

    /// Read the body into the internal buffer
    async fn read_body(&mut self) -> Result<Option<BufRef>> {
        match self.read_timeout {
            Some(t) => match timeout(t, self.do_read_body()).await {
                Ok(res) => res,
                Err(_) => Error::e_explain(ReadTimedout, format!("reading body, timeout: {t:?}")),
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
                Err(_) => Error::e_explain(ReadTimedout, format!("draining body, timeout: {t:?}")),
            },
            None => self.do_drain_request_body().await,
        }
    }

    /// Whether there is no (more) body need to be read.
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
    pub async fn write_response_header(&mut self, mut header: Box<ResponseHeader>) -> Result<()> {
        if header.status.is_informational() && self.ignore_info_resp(header.status.into()) {
            debug!("ignoring informational headers");
            return Ok(());
        }

        if let Some(resp) = self.response_written.as_ref() {
            if !resp.status.is_informational() || self.upgraded {
                warn!("Respond header is already sent, cannot send again");
                return Ok(());
            }
        }

        // no need to add these headers to 1xx responses
        if !header.status.is_informational() && self.update_resp_headers {
            /* update headers */
            header.insert_header(header::DATE, date::get_cached_date())?;

            // TODO: make these lazy static
            let connection_value = if self.will_keepalive() {
                "keep-alive"
            } else {
                "close"
            };
            header.insert_header(header::CONNECTION, connection_value)?;
        }

        if header.status == 101 {
            // make sure the connection is closed at the end when 101/upgrade is used
            self.set_keepalive(None);
        }

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
                    self.body_reader.init_content_length(0, b"");
                }
            }
            self.init_body_writer(&header);
        }

        // Don't have to flush response with content length because it is less
        // likely to be real time communication. So do flush when
        // 1.1xx response: client needs to see it before the rest of response
        // 2.No content length: the response could be generated in real time
        let flush = header.status.is_informational()
            || header.headers.get(header::CONTENT_LENGTH).is_none();

        let mut write_buf = BytesMut::with_capacity(INIT_HEADER_BUF_SIZE);
        http_resp_header_to_buf(&header, &mut write_buf).unwrap();
        match self.underlying_stream.write_all(&write_buf).await {
            Ok(()) => {
                // flush the stream if 1xx header or there is no response body
                if flush || self.body_writer.finished() {
                    self.underlying_stream
                        .flush()
                        .await
                        .or_err(WriteError, "flushing response header")?;
                }
                self.response_written = Some(header);
                self.body_bytes_sent += write_buf.len();
                Ok(())
            }
            Err(e) => Error::e_because(WriteError, "writing response header", e),
        }
    }

    /// Return the response header if it is already sent.
    pub fn response_written(&self) -> Option<&ResponseHeader> {
        self.response_written.as_deref()
    }

    /// `Some(true)` if the this is a successful upgrade
    /// `Some(false)` if the request is an upgrade but the response refuses it
    /// `None` if the request is not an upgrade.
    pub fn is_upgrade(&self, header: &ResponseHeader) -> Option<bool> {
        if self.is_upgrade_req() {
            Some(is_upgrade_resp(header))
        } else {
            None
        }
    }

    fn set_keepalive(&mut self, seconds: Option<u64>) {
        match seconds {
            Some(sec) => {
                if sec > 0 {
                    self.keepalive_timeout = KeepaliveStatus::Timeout(Duration::from_secs(sec));
                } else {
                    self.keepalive_timeout = KeepaliveStatus::Infinite;
                }
            }
            None => {
                self.keepalive_timeout = KeepaliveStatus::Off;
            }
        }
    }

    /// Return whether the session will be keepalived for connection reuse.
    pub fn will_keepalive(&self) -> bool {
        // TODO: check self.body_writer. If it is http1.0 type then keepalive
        // cannot be used because the connection close is the signal of end body
        !matches!(self.keepalive_timeout, KeepaliveStatus::Off)
    }

    // `Keep-Alive: timeout=5, max=1000` => 5, 1000
    fn get_keepalive_values(&self) -> (Option<u64>, Option<usize>) {
        // TODO: implement this parsing
        (None, None)
    }

    fn ignore_info_resp(&self, status: u16) -> bool {
        // ignore informational response if ignore flag is set and it's not an Upgrade and Expect: 100-continue isn't set
        self.ignore_info_resp && status != 101 && !(status == 100 && self.is_expect_continue_req())
    }

    fn is_expect_continue_req(&self) -> bool {
        match self.request_header.as_deref() {
            Some(req) => is_expect_continue_req(req),
            None => false,
        }
    }

    fn is_connection_keepalive(&self) -> Option<bool> {
        is_buf_keepalive(self.get_header(header::CONNECTION))
    }

    // calculate write timeout from min_send_rate if set, otherwise return write_timeout
    fn write_timeout(&self, buf_len: usize) -> Option<Duration> {
        let Some(min_send_rate) = self.min_send_rate.filter(|r| *r > 0) else {
            return self.write_timeout;
        };

        // min timeout is 1s
        let ms = (buf_len.max(min_send_rate) as f64 / min_send_rate as f64) * 1000.0;
        // truncates unrealistically large values (we'll be out of memory before this happens)
        Some(Duration::from_millis(ms as u64))
    }

    /// Apply keepalive settings according to the client
    /// For HTTP 1.1, assume keepalive as long as there is no `Connection: Close` request header.
    /// For HTTP 1.0, only keepalive if there is an explicit header `Connection: keep-alive`.
    pub fn respect_keepalive(&mut self) {
        if let Some(keepalive) = self.is_connection_keepalive() {
            if keepalive {
                let (timeout, _max_use) = self.get_keepalive_values();
                // TODO: respect max_use
                match timeout {
                    Some(d) => self.set_keepalive(Some(d)),
                    None => self.set_keepalive(Some(0)), // infinite
                }
            } else {
                self.set_keepalive(None);
            }
        } else if self.req_header().version == Version::HTTP_11 {
            self.set_keepalive(Some(0)); // on by default for http 1.1
        } else {
            self.set_keepalive(None); // off by default for http 1.0
        }
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
            self.body_writer.init_http10();
        } else {
            init_body_writer_comm(&mut self.body_writer, &header.headers);
        }
    }

    /// Same as [`Self::write_response_header()`] but takes a reference.
    pub async fn write_response_header_ref(&mut self, resp: &ResponseHeader) -> Result<()> {
        self.write_response_header(Box::new(resp.clone())).await
    }

    async fn do_write_body(&mut self, buf: &[u8]) -> Result<Option<usize>> {
        let written = self
            .body_writer
            .write_body(&mut self.underlying_stream, buf)
            .await;

        if let Ok(Some(num_bytes)) = written {
            self.body_bytes_sent += num_bytes;
        }

        written
    }

    /// Write response body to the client. Return `Ok(None)` when there shouldn't be more body
    /// to be written, e.g., writing more bytes than what the `Content-Length` header suggests
    pub async fn write_body(&mut self, buf: &[u8]) -> Result<Option<usize>> {
        // TODO: check if the response header is written
        match self.write_timeout(buf.len()) {
            Some(t) => match timeout(t, self.do_write_body(buf)).await {
                Ok(res) => res,
                Err(_) => Error::e_explain(WriteTimedout, format!("writing body, timeout: {t:?}")),
            },
            None => self.do_write_body(buf).await,
        }
    }

    async fn do_write_body_buf(&mut self) -> Result<Option<usize>> {
        // Don't flush empty chunks, they are considered end of body for chunks
        if self.body_write_buf.is_empty() {
            return Ok(None);
        }

        let written = self
            .body_writer
            .write_body(&mut self.underlying_stream, &self.body_write_buf)
            .await;

        if let Ok(Some(num_bytes)) = written {
            self.body_bytes_sent += num_bytes;
        }

        // make sure this buf is safe to reuse
        self.body_write_buf.clear();

        written
    }

    async fn write_body_buf(&mut self) -> Result<Option<usize>> {
        match self.write_timeout(self.body_write_buf.len()) {
            Some(t) => match timeout(t, self.do_write_body_buf()).await {
                Ok(res) => res,
                Err(_) => Error::e_explain(WriteTimedout, format!("writing body, timeout: {t:?}")),
            },
            None => self.do_write_body_buf().await,
        }
    }

    fn maybe_force_close_body_reader(&mut self) {
        if self.upgraded && !self.body_reader.body_done() {
            // response is done, reset the request body to close
            self.body_reader.init_content_length(0, b"");
        }
    }

    /// Signal that there is no more body to write.
    /// This call will try to flush the buffer if there is any un-flushed data.
    /// For chunked encoding response, this call will also send the last chunk.
    /// For upgraded sessions, this call will also close the reading of the client body.
    pub async fn finish_body(&mut self) -> Result<Option<usize>> {
        let res = self.body_writer.finish(&mut self.underlying_stream).await?;
        self.underlying_stream
            .flush()
            .await
            .or_err(WriteError, "flushing body")?;

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

    fn get_content_length(&self) -> Option<usize> {
        buf_to_content_length(
            self.get_header(header::CONTENT_LENGTH)
                .map(|v| v.as_bytes()),
        )
    }

    fn init_body_reader(&mut self) {
        if self.body_reader.need_init() {
            // reset retry buffer
            if let Some(buffer) = self.retry_buffer.as_mut() {
                buffer.clear();
            }

            /* follow https://tools.ietf.org/html/rfc7230#section-3.3.3 */
            let preread_body = self.preread_body.as_ref().unwrap().get(&self.buf[..]);

            if self.req_header().version == Version::HTTP_11 && self.is_upgrade_req() {
                self.body_reader.init_http10(preread_body);
                return;
            }

            if self.is_chunked_encoding() {
                // if chunked encoding, content-length should be ignored
                self.body_reader.init_chunked(preread_body);
            } else {
                let cl = self.get_content_length();
                match cl {
                    Some(i) => {
                        self.body_reader.init_content_length(i, preread_body);
                    }
                    None => {
                        match self.req_header().version {
                            Version::HTTP_11 => {
                                // Per RFC assume no body by default in HTTP 1.1
                                self.body_reader.init_content_length(0, preread_body);
                            }
                            _ => {
                                self.body_reader.init_http10(preread_body);
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

    fn get_body(&self, buf_ref: &BufRef) -> &[u8] {
        // TODO: these get_*() could panic. handle them better
        self.body_reader.get_body(buf_ref)
    }

    /// This function will (async) block forever until the client closes the connection.
    pub async fn idle(&mut self) -> Result<usize> {
        // NOTE: this implementation breaks http pipelining, ideally we need poll_error
        // NOTE: buf cannot be empty, openssl-rs read() requires none empty buf.
        let mut buf: [u8; 1] = [0; 1];
        self.underlying_stream
            .read(&mut buf)
            .await
            .or_err(ReadError, "during HTTP idle state")
    }

    /// This function will return body bytes (same as [`Self::read_body_bytes()`]), but after
    /// the client body finishes (`Ok(None)` is returned), calling this function again will block
    /// forever, same as [`Self::idle()`].
    pub async fn read_body_or_idle(&mut self, no_body_expected: bool) -> Result<Option<Bytes>> {
        if no_body_expected || self.is_body_done() {
            let read = self.idle().await?;
            if read == 0 {
                Error::e_explain(
                    ConnectionClosed,
                    if self.response_written.is_none() {
                        "Prematurely before response header is sent"
                    } else {
                        "Prematurely before response body is complete"
                    },
                )
            } else {
                Error::e_explain(ConnectError, "Sent data after end of body")
            }
        } else {
            self.read_body_bytes().await
        }
    }

    /// Return the raw bytes of the request header.
    pub fn get_headers_raw_bytes(&self) -> Bytes {
        self.raw_header.as_ref().unwrap().get_bytes(&self.buf)
    }

    /// Close the connection abruptly. This allows to signal the client that the connection is closed
    /// before dropping [`HttpSession`]
    pub async fn shutdown(&mut self) {
        let _ = self.underlying_stream.shutdown().await;
    }

    /// Set the server keepalive timeout.
    /// `None`: disable keepalive, this session cannot be reused.
    /// `Some(0)`: reusing this session is allowed and there is no timeout.
    /// `Some(>0)`: reusing this session is allowed within the given timeout in seconds.
    /// If the client disallows connection reuse, then `keepalive` will be ignored.
    pub fn set_server_keepalive(&mut self, keepalive: Option<u64>) {
        if let Some(false) = self.is_connection_keepalive() {
            // connection: close is set
            self.set_keepalive(None);
        } else {
            self.set_keepalive(keepalive);
        }
    }

    /// Sets the downstream read timeout. This will trigger if we're unable
    /// to read from the stream after `timeout`.
    pub fn set_read_timeout(&mut self, timeout: Duration) {
        self.read_timeout = Some(timeout);
    }

    /// Sets the downstream write timeout. This will trigger if we're unable
    /// to write to the stream after `timeout`. If a `min_send_rate` is
    /// configured then the `min_send_rate` calculated timeout has higher priority.
    pub fn set_write_timeout(&mut self, timeout: Duration) {
        self.write_timeout = Some(timeout);
    }

    /// Sets the total drain timeout. For HTTP/1.1, reusing a session requires
    /// ensuring that the request body is consumed. This `timeout` will be used
    /// to determine how long to wait for the entirety of the downstream request
    /// body to finish after the upstream response is completed to return the
    /// session to the reuse pool. If the timeout is exceeded, we will give up
    /// on trying to reuse the session.
    ///
    /// Note that the downstream read timeout still applies between body byte reads.
    pub fn set_total_drain_timeout(&mut self, timeout: Duration) {
        self.total_drain_timeout = Some(timeout);
    }

    /// Sets the minimum downstream send rate in bytes per second. This
    /// is used to calculate a write timeout in seconds based on the size
    /// of the buffer being written. If a `min_send_rate` is configured it
    /// has higher priority over a set `write_timeout`. The minimum send
    /// rate must be greater than zero.
    ///
    /// Calculated write timeout is guaranteed to be at least 1s if `min_send_rate`
    /// is greater than zero, a send rate of zero is a noop.
    pub fn set_min_send_rate(&mut self, min_send_rate: usize) {
        if min_send_rate > 0 {
            self.min_send_rate = Some(min_send_rate);
        }
    }

    /// Sets whether we ignore writing informational responses downstream.
    ///
    /// This is a noop if the response is Upgrade or Continue and
    /// Expect: 100-continue was set on the request.
    pub fn set_ignore_info_resp(&mut self, ignore: bool) {
        self.ignore_info_resp = ignore;
    }

    /// Return the [Digest] of the connection.
    pub fn digest(&self) -> &Digest {
        &self.digest
    }

    /// Return a mutable [Digest] reference for the connection.
    pub fn digest_mut(&mut self) -> &mut Digest {
        &mut self.digest
    }

    /// Return the client (peer) address of the underlying connection.
    pub fn client_addr(&self) -> Option<&SocketAddr> {
        self.digest()
            .socket_digest
            .as_ref()
            .map(|d| d.peer_addr())?
    }

    /// Return the server (local) address of the underlying connection.
    pub fn server_addr(&self) -> Option<&SocketAddr> {
        self.digest()
            .socket_digest
            .as_ref()
            .map(|d| d.local_addr())?
    }

    /// Consume `self`, if the connection can be reused, the underlying stream will be returned
    /// to be fed to the next [`Self::new()`]. This drains any remaining request body if it hasn't
    /// yet been read and the stream is reusable.
    ///
    /// The next session can just call [`Self::read_request()`].
    ///
    /// If the connection cannot be reused, the underlying stream will be closed and `None` will be
    /// returned. If there was an error while draining any remaining request body that error will
    /// be returned.
    pub async fn reuse(mut self) -> Result<Option<Stream>> {
        match self.keepalive_timeout {
            KeepaliveStatus::Off => {
                debug!("HTTP shutdown connection");
                self.shutdown().await;
                Ok(None)
            }
            _ => {
                self.drain_request_body().await?;
                Ok(Some(self.underlying_stream))
            }
        }
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
                        self.write_body(&d).await.map_err(|e| e.into_down())?;
                    }
                    end_stream
                }
                None => end_stream,
            },
            HttpTask::Trailer(_) => true, // h1 trailer is not supported yet
            HttpTask::Done => true,
            HttpTask::Failed(e) => return Err(e),
        };
        if end_stream {
            // no-op if body wasn't initialized or is finished already
            self.finish_body().await.map_err(|e| e.into_down())?;
        }
        Ok(end_stream || self.body_writer.finished())
    }

    // TODO: use vectored write to avoid copying
    pub async fn response_duplex_vec(&mut self, mut tasks: Vec<HttpTask>) -> Result<bool> {
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
                        if !d.is_empty() && !self.body_writer.finished() {
                            self.body_write_buf.put_slice(&d);
                        }
                        end_stream
                    }
                    None => end_stream,
                },
                HttpTask::Trailer(_) => true, // h1 trailer is not supported yet
                HttpTask::Done => true,
                HttpTask::Failed(e) => {
                    // flush the data we have and quit
                    self.write_body_buf().await.map_err(|e| e.into_down())?;
                    self.underlying_stream
                        .flush()
                        .await
                        .or_err(WriteError, "flushing response")?;
                    return Err(e);
                }
            }
        }
        self.write_body_buf().await.map_err(|e| e.into_down())?;
        if end_stream {
            // no-op if body wasn't initialized or is finished already
            self.finish_body().await.map_err(|e| e.into_down())?;
        }
        Ok(end_stream || self.body_writer.finished())
    }

    /// Get the reference of the [Stream] that this HTTP session is operating upon.
    pub fn stream(&self) -> &Stream {
        &self.underlying_stream
    }

    /// Consume `self`, the underlying stream will be returned and can be used
    /// directly, for example, in the case of HTTP upgrade. The stream is not
    /// flushed prior to being returned.
    pub fn into_inner(self) -> Stream {
        self.underlying_stream
    }
}

// Regex to parse request line that has illegal chars in it
static REQUEST_LINE_REGEX: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^\w+ (?P<uri>.+) HTTP/\d(?:\.\d)?").unwrap());

// the chars httparse considers illegal in URL
// Almost https://url.spec.whatwg.org/#query-percent-encode-set + {}
const URI_ESC_CHARSET: &AsciiSet = &CONTROLS.add(b' ').add(b'<').add(b'>').add(b'"');

fn escape_illegal_request_line(buf: &BytesMut) -> Option<BytesMut> {
    if let Some(captures) = REQUEST_LINE_REGEX.captures(buf) {
        // return if nothing matches: not a request line at all
        let uri = captures.name("uri")?;

        let escaped_uri = percent_encode(uri.as_bytes(), URI_ESC_CHARSET);

        // rebuild the entire request buf in a new buffer
        // TODO: this might be able to be done in place

        // need to be slightly bigger than the current buf;
        let mut new_buf = BytesMut::with_capacity(buf.len() + 32);
        new_buf.extend_from_slice(&buf[..uri.start()]);

        for s in escaped_uri {
            new_buf.extend_from_slice(s.as_bytes());
        }

        if new_buf.len() == uri.end() {
            // buf unchanged, nothing is escaped, return None to avoid loop
            return None;
        }

        new_buf.extend_from_slice(&buf[uri.end()..]);

        Some(new_buf)
    } else {
        None
    }
}

#[inline]
fn parse_req_buffer<'buf>(
    req: &mut httparse::Request<'_, 'buf>,
    buf: &'buf [u8],
) -> HeaderParseState {
    use httparse::Result;

    #[cfg(feature = "patched_http1")]
    fn parse<'buf>(req: &mut httparse::Request<'_, 'buf>, buf: &'buf [u8]) -> Result<usize> {
        req.parse_unchecked(buf)
    }

    #[cfg(not(feature = "patched_http1"))]
    fn parse<'buf>(req: &mut httparse::Request<'_, 'buf>, buf: &'buf [u8]) -> Result<usize> {
        req.parse(buf)
    }

    let res = match parse(req, buf) {
        Ok(s) => s,
        Err(e) => {
            return HeaderParseState::Invalid(e);
        }
    };
    match res {
        httparse::Status::Complete(s) => HeaderParseState::Complete(s),
        _ => HeaderParseState::Partial,
    }
}

#[inline]
fn http_resp_header_to_buf(
    resp: &ResponseHeader,
    buf: &mut BytesMut,
) -> std::result::Result<(), ()> {
    // Status-Line
    let version = match resp.version {
        Version::HTTP_09 => "HTTP/0.9 ",
        Version::HTTP_10 => "HTTP/1.0 ",
        Version::HTTP_11 => "HTTP/1.1 ",
        _ => {
            return Err(()); /*TODO: unsupported version */
        }
    };
    buf.put_slice(version.as_bytes());
    let status = resp.status;
    buf.put_slice(status.as_str().as_bytes());
    buf.put_u8(b' ');
    let reason = resp.get_reason_phrase();
    if let Some(reason_buf) = reason {
        buf.put_slice(reason_buf.as_bytes());
    }
    buf.put_slice(CRLF);

    // headers
    // TODO: style: make sure Server and Date headers are the first two
    resp.header_to_h1_wire(buf);

    buf.put_slice(CRLF);
    Ok(())
}

#[cfg(test)]
mod tests_stream {
    use super::*;
    use crate::protocols::http::v1::body::{BodyMode, ParseState};
    use http::StatusCode;
    use rstest::rstest;
    use std::str;
    use tokio_test::io::Builder;

    fn init_log() {
        let _ = env_logger::builder().is_test(true).try_init();
    }

    #[tokio::test]
    async fn read_basic() {
        init_log();
        let input = b"GET / HTTP/1.1\r\n\r\n";
        let mock_io = Builder::new().read(&input[..]).build();
        let mut http_stream = HttpSession::new(Box::new(mock_io));
        let res = http_stream.read_request().await;
        assert_eq!(input.len(), res.unwrap().unwrap());
        assert_eq!(0, http_stream.req_header().headers.len());
    }

    #[cfg(feature = "patched_http1")]
    #[tokio::test]
    async fn read_invalid_path() {
        init_log();
        let input = b"GET /\x01\xF0\x90\x80 HTTP/1.1\r\n\r\n";
        let mock_io = Builder::new().read(&input[..]).build();
        let mut http_stream = HttpSession::new(Box::new(mock_io));
        let res = http_stream.read_request().await;
        assert_eq!(input.len(), res.unwrap().unwrap());
        assert_eq!(0, http_stream.req_header().headers.len());
        assert_eq!(b"/\x01\xF0\x90\x80", http_stream.get_path());
    }

    #[tokio::test]
    async fn read_2_buf() {
        init_log();
        let input1 = b"GET / HTTP/1.1\r\n";
        let input2 = b"Host: pingora.org\r\n\r\n";
        let mock_io = Builder::new().read(&input1[..]).read(&input2[..]).build();
        let mut http_stream = HttpSession::new(Box::new(mock_io));
        let res = http_stream.read_request().await;
        assert_eq!(input1.len() + input2.len(), res.unwrap().unwrap());
        assert_eq!(
            input1.len() + input2.len(),
            http_stream.raw_header.as_ref().unwrap().len()
        );
        assert_eq!(1, http_stream.req_header().headers.len());
        assert_eq!(Some(&Method::GET), http_stream.get_method());
        assert_eq!(b"/", http_stream.get_path());
        assert_eq!(Version::HTTP_11, http_stream.req_header().version);

        assert_eq!(b"pingora.org", http_stream.get_header_bytes("Host"));
    }

    #[tokio::test]
    async fn read_with_body_content_length() {
        init_log();
        let input1 = b"GET / HTTP/1.1\r\n";
        let input2 = b"Host: pingora.org\r\nContent-Length: 3\r\n\r\n";
        let input3 = b"abc";
        let mock_io = Builder::new()
            .read(&input1[..])
            .read(&input2[..])
            .read(&input3[..])
            .build();
        let mut http_stream = HttpSession::new(Box::new(mock_io));
        http_stream.read_request().await.unwrap();
        let res = http_stream.read_body_bytes().await.unwrap().unwrap();
        assert_eq!(res, input3.as_slice());
        assert_eq!(http_stream.body_reader.body_state, ParseState::Complete(3));
        assert_eq!(http_stream.body_bytes_read(), 3);
    }

    #[tokio::test]
    #[should_panic(expected = "There is still data left to read.")]
    async fn read_with_body_timeout() {
        init_log();
        let input1 = b"GET / HTTP/1.1\r\n";
        let input2 = b"Host: pingora.org\r\nContent-Length: 3\r\n\r\n";
        let input3 = b"abc";
        let mock_io = Builder::new()
            .read(&input1[..])
            .read(&input2[..])
            .wait(Duration::from_secs(2))
            .read(&input3[..])
            .build();
        let mut http_stream = HttpSession::new(Box::new(mock_io));
        http_stream.read_timeout = Some(Duration::from_secs(1));
        http_stream.read_request().await.unwrap();
        let res = http_stream.read_body_bytes().await;
        assert_eq!(http_stream.body_bytes_read(), 0);
        assert_eq!(res.unwrap_err().etype(), &ReadTimedout);
    }

    #[tokio::test]
    async fn read_with_body_content_length_single_read() {
        init_log();
        let input1 = b"GET / HTTP/1.1\r\n";
        let input2 = b"Host: pingora.org\r\nContent-Length: 3\r\n\r\nabc";
        let mock_io = Builder::new().read(&input1[..]).read(&input2[..]).build();
        let mut http_stream = HttpSession::new(Box::new(mock_io));
        http_stream.read_request().await.unwrap();
        let res = http_stream.read_body_bytes().await.unwrap().unwrap();
        assert_eq!(res, b"abc".as_slice());
        assert_eq!(http_stream.body_reader.body_state, ParseState::Complete(3));
        assert_eq!(http_stream.body_bytes_read(), 3);
    }

    #[tokio::test]
    async fn read_with_body_http10() {
        init_log();
        let input1 = b"GET / HTTP/1.0\r\n";
        let input2 = b"Host: pingora.org\r\n\r\n";
        let input3 = b"a";
        let input4 = b""; // simulating close
        let mock_io = Builder::new()
            .read(&input1[..])
            .read(&input2[..])
            .read(&input3[..])
            .read(&input4[..])
            .build();
        let mut http_stream = HttpSession::new(Box::new(mock_io));
        http_stream.read_request().await.unwrap();
        let res = http_stream.read_body_bytes().await.unwrap().unwrap();
        assert_eq!(res, input3.as_slice());
        assert_eq!(http_stream.body_reader.body_state, ParseState::HTTP1_0(1));
        assert_eq!(http_stream.body_bytes_read(), 1);
        let res = http_stream.read_body_bytes().await.unwrap();
        assert!(res.is_none());
        assert_eq!(http_stream.body_reader.body_state, ParseState::Complete(1));
        assert_eq!(http_stream.body_bytes_read(), 1);
    }

    #[tokio::test]
    async fn read_with_body_http10_single_read() {
        init_log();
        let input1 = b"GET / HTTP/1.0\r\n";
        let input2 = b"Host: pingora.org\r\n\r\na";
        let input3 = b"b";
        let input4 = b""; // simulating close
        let mock_io = Builder::new()
            .read(&input1[..])
            .read(&input2[..])
            .read(&input3[..])
            .read(&input4[..])
            .build();
        let mut http_stream = HttpSession::new(Box::new(mock_io));
        http_stream.read_request().await.unwrap();
        let res = http_stream.read_body_bytes().await.unwrap().unwrap();
        assert_eq!(res, b"a".as_slice());
        assert_eq!(http_stream.body_reader.body_state, ParseState::HTTP1_0(1));
        let res = http_stream.read_body_bytes().await.unwrap().unwrap();
        assert_eq!(res, b"b".as_slice());
        assert_eq!(http_stream.body_reader.body_state, ParseState::HTTP1_0(2));
        let res = http_stream.read_body_bytes().await.unwrap();
        assert_eq!(http_stream.body_bytes_read(), 2);
        assert!(res.is_none());
        assert_eq!(http_stream.body_reader.body_state, ParseState::Complete(2));
    }

    #[tokio::test]
    async fn read_http11_default_no_body() {
        init_log();
        let input1 = b"GET / HTTP/1.1\r\n";
        let input2 = b"Host: pingora.org\r\n\r\n";
        let mock_io = Builder::new().read(&input1[..]).read(&input2[..]).build();
        let mut http_stream = HttpSession::new(Box::new(mock_io));
        http_stream.read_request().await.unwrap();
        let res = http_stream.read_body_bytes().await.unwrap();
        assert!(res.is_none());
        assert_eq!(http_stream.body_bytes_read(), 0);
        assert_eq!(http_stream.body_reader.body_state, ParseState::Complete(0));
    }

    #[tokio::test]
    async fn read_with_body_chunked_0() {
        init_log();
        let input1 = b"GET / HTTP/1.1\r\n";
        let input2 = b"Host: pingora.org\r\nTransfer-Encoding: chunked\r\n\r\n";
        let input3 = b"0\r\n";
        let mock_io = Builder::new()
            .read(&input1[..])
            .read(&input2[..])
            .read(&input3[..])
            .build();
        let mut http_stream = HttpSession::new(Box::new(mock_io));
        http_stream.read_request().await.unwrap();
        assert!(http_stream.is_chunked_encoding());
        let res = http_stream.read_body_bytes().await.unwrap();
        assert!(res.is_none());
        assert_eq!(http_stream.body_bytes_read(), 0);
        assert_eq!(http_stream.body_reader.body_state, ParseState::Complete(0));
    }

    #[tokio::test]
    async fn read_with_body_chunked_single_read() {
        init_log();
        let input1 = b"GET / HTTP/1.1\r\n";
        let input2 = b"Host: pingora.org\r\nTransfer-Encoding: chunked\r\n\r\n1\r\na\r\n";
        let input3 = b"0\r\n\r\n";
        let mock_io = Builder::new()
            .read(&input1[..])
            .read(&input2[..])
            .read(&input3[..])
            .build();
        let mut http_stream = HttpSession::new(Box::new(mock_io));
        http_stream.read_request().await.unwrap();
        assert!(http_stream.is_chunked_encoding());
        let res = http_stream.read_body_bytes().await.unwrap().unwrap();
        assert_eq!(res, b"a".as_slice());
        assert_eq!(
            http_stream.body_reader.body_state,
            ParseState::Chunked(1, 0, 0, 0)
        );
        let res = http_stream.read_body_bytes().await.unwrap();
        assert!(res.is_none());
        assert_eq!(http_stream.body_bytes_read(), 1);
        assert_eq!(http_stream.body_reader.body_state, ParseState::Complete(1));
    }

    #[rstest]
    #[case(None, None)]
    #[case(Some("transfer-encoding"), None)]
    #[case(Some("transfer-encoding"), Some("CONTENT-LENGTH"))]
    #[case(Some("TRANSFER-ENCODING"), Some("CONTENT-LENGTH"))]
    #[case(Some("TRANSFER-ENCODING"), None)]
    #[case(None, Some("CONTENT-LENGTH"))]
    #[case(Some("TRANSFER-ENCODING"), Some("content-length"))]
    #[case(None, Some("content-length"))]
    #[tokio::test]
    async fn transfer_encoding_and_content_length_disallowed(
        #[case] transfer_encoding_header: Option<&str>,
        #[case] content_length_header: Option<&str>,
    ) {
        init_log();
        let input1 = b"GET / HTTP/1.1\r\n";
        let mut input2 = "Host: pingora.org\r\n".to_owned();

        if let Some(transfer_encoding) = transfer_encoding_header {
            input2 += &format!("{transfer_encoding}: chunked\r\n");
        }
        if let Some(content_length) = content_length_header {
            input2 += &format!("{content_length}: 4\r\n")
        }

        input2 += "\r\n3e\r\na\r\n";
        let mock_io = Builder::new()
            .read(&input1[..])
            .read(input2.as_bytes())
            .build();
        let mut http_stream = HttpSession::new(Box::new(mock_io));
        let _ = http_stream.read_request().await.unwrap();

        match (content_length_header, transfer_encoding_header) {
            (Some(_) | None, Some(_)) => {
                assert!(http_stream.get_header(TRANSFER_ENCODING).is_some());
                assert!(http_stream.get_header(CONTENT_LENGTH).is_none());
            }
            (Some(_), None) => {
                assert!(http_stream.get_header(TRANSFER_ENCODING).is_none());
                assert!(http_stream.get_header(CONTENT_LENGTH).is_some());
            }
            _ => {
                assert!(http_stream.get_header(CONTENT_LENGTH).is_none());
                assert!(http_stream.get_header(TRANSFER_ENCODING).is_none());
            }
        }
    }

    #[tokio::test]
    #[should_panic(expected = "There is still data left to read.")]
    async fn read_invalid() {
        let input1 = b"GET / HTP/1.1\r\n";
        let input2 = b"Host: pingora.org\r\n\r\n";
        let mock_io = Builder::new().read(&input1[..]).read(&input2[..]).build();
        let mut http_stream = HttpSession::new(Box::new(mock_io));
        let res = http_stream.read_request().await;
        assert_eq!(&InvalidHTTPHeader, res.unwrap_err().etype());
    }

    async fn build_req(upgrade: &str, conn: &str) -> HttpSession {
        let input = format!("GET / HTTP/1.1\r\nHost: pingora.org\r\nUpgrade: {upgrade}\r\nConnection: {conn}\r\n\r\n");
        let mock_io = Builder::new().read(input.as_bytes()).build();
        let mut http_stream = HttpSession::new(Box::new(mock_io));
        http_stream.read_request().await.unwrap();
        http_stream
    }

    #[tokio::test]
    async fn read_upgrade_req() {
        // http 1.0
        let input = b"GET / HTTP/1.0\r\nHost: pingora.org\r\nUpgrade: websocket\r\nConnection: upgrade\r\n\r\n";
        let mock_io = Builder::new().read(&input[..]).build();
        let mut http_stream = HttpSession::new(Box::new(mock_io));
        http_stream.read_request().await.unwrap();
        assert!(!http_stream.is_upgrade_req());

        // different method
        let input = b"POST / HTTP/1.1\r\nHost: pingora.org\r\nUpgrade: websocket\r\nConnection: upgrade\r\n\r\n";
        let mock_io = Builder::new().read(&input[..]).build();
        let mut http_stream = HttpSession::new(Box::new(mock_io));
        http_stream.read_request().await.unwrap();
        assert!(http_stream.is_upgrade_req());

        // missing upgrade header
        let input = b"GET / HTTP/1.1\r\nHost: pingora.org\r\nConnection: upgrade\r\n\r\n";
        let mock_io = Builder::new().read(&input[..]).build();
        let mut http_stream = HttpSession::new(Box::new(mock_io));
        http_stream.read_request().await.unwrap();
        assert!(!http_stream.is_upgrade_req());

        // no connection header
        let input = b"GET / HTTP/1.1\r\nHost: pingora.org\r\nUpgrade: WebSocket\r\n\r\n";
        let mock_io = Builder::new().read(&input[..]).build();
        let mut http_stream = HttpSession::new(Box::new(mock_io));
        http_stream.read_request().await.unwrap();
        assert!(http_stream.is_upgrade_req());

        assert!(build_req("websocket", "Upgrade").await.is_upgrade_req());

        // mixed case
        assert!(build_req("WebSocket", "Upgrade").await.is_upgrade_req());
    }

    #[tokio::test]
    async fn read_upgrade_req_with_1xx_response() {
        let input = b"GET / HTTP/1.1\r\nHost: pingora.org\r\nUpgrade: websocket\r\nConnection: upgrade\r\n\r\n";
        let mock_io = Builder::new()
            .read(&input[..])
            .write(b"HTTP/1.1 100 Continue\r\n\r\n")
            .build();
        let mut http_stream = HttpSession::new(Box::new(mock_io));
        http_stream.read_request().await.unwrap();
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
    async fn set_server_keepalive() {
        // close
        let input = b"GET / HTTP/1.1\r\nHost: pingora.org\r\nConnection: close\r\n\r\n";
        let mock_io = Builder::new().read(&input[..]).build();
        let mut http_stream = HttpSession::new(Box::new(mock_io));
        http_stream.read_request().await.unwrap();
        // verify close
        assert_eq!(http_stream.keepalive_timeout, KeepaliveStatus::Off);
        http_stream.set_server_keepalive(Some(60));
        // verify no change on override
        assert_eq!(http_stream.keepalive_timeout, KeepaliveStatus::Off);

        // explicit keep-alive
        let input = b"GET / HTTP/1.1\r\nHost: pingora.org\r\nConnection: keep-alive\r\n\r\n";
        let mock_io = Builder::new().read(&input[..]).build();
        let mut http_stream = HttpSession::new(Box::new(mock_io));
        // default is infinite for 1.1
        http_stream.read_request().await.unwrap();
        assert_eq!(http_stream.keepalive_timeout, KeepaliveStatus::Infinite);
        http_stream.set_server_keepalive(Some(60));
        // override respected
        assert_eq!(
            http_stream.keepalive_timeout,
            KeepaliveStatus::Timeout(Duration::from_secs(60))
        );

        // not specified
        let input = b"GET / HTTP/1.1\r\nHost: pingora.org\r\n\r\n";
        let mock_io = Builder::new().read(&input[..]).build();
        let mut http_stream = HttpSession::new(Box::new(mock_io));
        http_stream.read_request().await.unwrap();
        // default is infinite for 1.1
        assert_eq!(http_stream.keepalive_timeout, KeepaliveStatus::Infinite);
        http_stream.set_server_keepalive(Some(60));
        // override respected
        assert_eq!(
            http_stream.keepalive_timeout,
            KeepaliveStatus::Timeout(Duration::from_secs(60))
        );
    }

    #[tokio::test]
    async fn write() {
        let wire = b"HTTP/1.1 200 OK\r\nFoo: Bar\r\n\r\n";
        let mock_io = Builder::new().write(wire).build();
        let mut http_stream = HttpSession::new(Box::new(mock_io));
        let mut new_response = ResponseHeader::build(StatusCode::OK, None).unwrap();
        new_response.append_header("Foo", "Bar").unwrap();
        http_stream.update_resp_headers = false;
        http_stream
            .write_response_header_ref(&new_response)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn write_custom_reason() {
        let wire = b"HTTP/1.1 200 Just Fine\r\nFoo: Bar\r\n\r\n";
        let mock_io = Builder::new().write(wire).build();
        let mut http_stream = HttpSession::new(Box::new(mock_io));
        let mut new_response = ResponseHeader::build(StatusCode::OK, None).unwrap();
        new_response.set_reason_phrase(Some("Just Fine")).unwrap();
        new_response.append_header("Foo", "Bar").unwrap();
        http_stream.update_resp_headers = false;
        http_stream
            .write_response_header_ref(&new_response)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn write_informational() {
        let wire = b"HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 200 OK\r\nFoo: Bar\r\n\r\n";
        let mock_io = Builder::new().write(wire).build();
        let mut http_stream = HttpSession::new(Box::new(mock_io));
        let response_100 = ResponseHeader::build(StatusCode::CONTINUE, None).unwrap();
        http_stream
            .write_response_header_ref(&response_100)
            .await
            .unwrap();
        let mut response_200 = ResponseHeader::build(StatusCode::OK, None).unwrap();
        response_200.append_header("Foo", "Bar").unwrap();
        http_stream.update_resp_headers = false;
        http_stream
            .write_response_header_ref(&response_200)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn write_informational_ignored() {
        let wire = b"HTTP/1.1 200 OK\r\nFoo: Bar\r\n\r\n";
        let mock_io = Builder::new().write(wire).build();
        let mut http_stream = HttpSession::new(Box::new(mock_io));
        // ignore the 100 Continue
        http_stream.ignore_info_resp = true;
        let response_100 = ResponseHeader::build(StatusCode::CONTINUE, None).unwrap();
        http_stream
            .write_response_header_ref(&response_100)
            .await
            .unwrap();
        let mut response_200 = ResponseHeader::build(StatusCode::OK, None).unwrap();
        response_200.append_header("Foo", "Bar").unwrap();
        http_stream.update_resp_headers = false;
        http_stream
            .write_response_header_ref(&response_200)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn write_informational_100_not_ignored_if_expect_continue() {
        let input = b"GET / HTTP/1.1\r\nExpect: 100-continue\r\n\r\n";
        let output = b"HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 200 OK\r\nFoo: Bar\r\n\r\n";

        let mock_io = Builder::new().read(&input[..]).write(output).build();
        let mut http_stream = HttpSession::new(Box::new(mock_io));
        http_stream.read_request().await.unwrap();
        http_stream.ignore_info_resp = true;
        // 100 Continue is not ignored due to Expect: 100-continue on request
        let response_100 = ResponseHeader::build(StatusCode::CONTINUE, None).unwrap();
        http_stream
            .write_response_header_ref(&response_100)
            .await
            .unwrap();
        let mut response_200 = ResponseHeader::build(StatusCode::OK, None).unwrap();
        response_200.append_header("Foo", "Bar").unwrap();
        http_stream.update_resp_headers = false;
        http_stream
            .write_response_header_ref(&response_200)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn write_informational_1xx_ignored_if_expect_continue() {
        let input = b"GET / HTTP/1.1\r\nExpect: 100-continue\r\n\r\n";
        let output = b"HTTP/1.1 200 OK\r\nFoo: Bar\r\n\r\n";

        let mock_io = Builder::new().read(&input[..]).write(output).build();
        let mut http_stream = HttpSession::new(Box::new(mock_io));
        http_stream.read_request().await.unwrap();
        http_stream.ignore_info_resp = true;
        // 102 Processing is ignored
        let response_102 = ResponseHeader::build(StatusCode::PROCESSING, None).unwrap();
        http_stream
            .write_response_header_ref(&response_102)
            .await
            .unwrap();
        let mut response_200 = ResponseHeader::build(StatusCode::OK, None).unwrap();
        response_200.append_header("Foo", "Bar").unwrap();
        http_stream.update_resp_headers = false;
        http_stream
            .write_response_header_ref(&response_200)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn write_101_switching_protocol() {
        let wire = b"HTTP/1.1 101 Switching Protocols\r\nFoo: Bar\r\n\r\n";
        let wire_body = b"nPAYLOAD";
        let mock_io = Builder::new().write(wire).write(wire_body).build();
        let mut http_stream = HttpSession::new(Box::new(mock_io));
        let mut response_101 =
            ResponseHeader::build(StatusCode::SWITCHING_PROTOCOLS, None).unwrap();
        response_101.append_header("Foo", "Bar").unwrap();
        http_stream
            .write_response_header_ref(&response_101)
            .await
            .unwrap();
        let n = http_stream.write_body(wire_body).await.unwrap().unwrap();
        assert_eq!(wire_body.len(), n);
        // simulate upgrade
        http_stream.upgraded = true;
        // this write should be ignored
        let response_502 = ResponseHeader::build(StatusCode::BAD_GATEWAY, None).unwrap();
        http_stream
            .write_response_header_ref(&response_502)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn write_body_cl() {
        let wire_header = b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\n\r\n";
        let wire_body = b"a";
        let mock_io = Builder::new().write(wire_header).write(wire_body).build();
        let mut http_stream = HttpSession::new(Box::new(mock_io));
        let mut new_response = ResponseHeader::build(StatusCode::OK, None).unwrap();
        new_response.append_header("Content-Length", "1").unwrap();
        http_stream.update_resp_headers = false;
        http_stream
            .write_response_header_ref(&new_response)
            .await
            .unwrap();
        assert_eq!(
            http_stream.body_writer.body_mode,
            BodyMode::ContentLength(1, 0)
        );
        let n = http_stream.write_body(wire_body).await.unwrap().unwrap();
        assert_eq!(wire_body.len(), n);
        let n = http_stream.finish_body().await.unwrap().unwrap();
        assert_eq!(wire_body.len(), n);
    }

    #[tokio::test]
    async fn write_body_http10() {
        let wire_header = b"HTTP/1.1 200 OK\r\n\r\n";
        let wire_body = b"a";
        let mock_io = Builder::new().write(wire_header).write(wire_body).build();
        let mut http_stream = HttpSession::new(Box::new(mock_io));
        let new_response = ResponseHeader::build(StatusCode::OK, None).unwrap();
        http_stream.update_resp_headers = false;
        http_stream
            .write_response_header_ref(&new_response)
            .await
            .unwrap();
        assert_eq!(http_stream.body_writer.body_mode, BodyMode::HTTP1_0(0));
        let n = http_stream.write_body(wire_body).await.unwrap().unwrap();
        assert_eq!(wire_body.len(), n);
        let n = http_stream.finish_body().await.unwrap().unwrap();
        assert_eq!(wire_body.len(), n);
    }

    #[tokio::test]
    async fn write_body_chunk() {
        let wire_header = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n";
        let wire_body = b"1\r\na\r\n";
        let wire_end = b"0\r\n\r\n";
        let mock_io = Builder::new()
            .write(wire_header)
            .write(wire_body)
            .write(wire_end)
            .build();
        let mut http_stream = HttpSession::new(Box::new(mock_io));
        let mut new_response = ResponseHeader::build(StatusCode::OK, None).unwrap();
        new_response
            .append_header("Transfer-Encoding", "chunked")
            .unwrap();
        http_stream.update_resp_headers = false;
        http_stream
            .write_response_header_ref(&new_response)
            .await
            .unwrap();
        assert_eq!(
            http_stream.body_writer.body_mode,
            BodyMode::ChunkedEncoding(0)
        );
        let n = http_stream.write_body(b"a").await.unwrap().unwrap();
        assert_eq!(b"a".len(), n);
        let n = http_stream.finish_body().await.unwrap().unwrap();
        assert_eq!(b"a".len(), n);
    }

    #[tokio::test]
    async fn read_with_illegal() {
        init_log();
        let input1 = b"GET /a?q=b c HTTP/1.1\r\n";
        let input2 = b"Host: pingora.org\r\nContent-Length: 3\r\n\r\n";
        let input3 = b"abc";
        let mock_io = Builder::new()
            .read(&input1[..])
            .read(&input2[..])
            .read(&input3[..])
            .build();
        let mut http_stream = HttpSession::new(Box::new(mock_io));
        http_stream.read_request().await.unwrap();
        assert_eq!(http_stream.get_path(), &b"/a?q=b%20c"[..]);
        let res = http_stream.read_body().await.unwrap().unwrap();
        assert_eq!(res, BufRef::new(0, 3));
        assert_eq!(http_stream.body_reader.body_state, ParseState::Complete(3));
        assert_eq!(input3, http_stream.get_body(&res));
    }

    #[test]
    fn escape_illegal() {
        init_log();
        // in query string
        let input = BytesMut::from(
            &b"GET /a?q=<\"b c\"> HTTP/1.1\r\nHost: pingora.org\r\nContent-Length: 3\r\n\r\n"[..],
        );
        let output = escape_illegal_request_line(&input).unwrap();
        assert_eq!(
            &output,
            &b"GET /a?q=%3C%22b%20c%22%3E HTTP/1.1\r\nHost: pingora.org\r\nContent-Length: 3\r\n\r\n"[..]
        );

        // in path
        let input = BytesMut::from(
            &b"GET /a:\"bc\" HTTP/1.1\r\nHost: pingora.org\r\nContent-Length: 3\r\n\r\n"[..],
        );
        let output = escape_illegal_request_line(&input).unwrap();
        assert_eq!(
            &output,
            &b"GET /a:%22bc%22 HTTP/1.1\r\nHost: pingora.org\r\nContent-Length: 3\r\n\r\n"[..]
        );

        // empty uri, unable to parse
        let input =
            BytesMut::from(&b"GET  HTTP/1.1\r\nHost: pingora.org\r\nContent-Length: 3\r\n\r\n"[..]);
        assert!(escape_illegal_request_line(&input).is_none());
    }

    #[tokio::test]
    async fn test_write_body_buf() {
        let wire = b"HTTP/1.1 200 OK\r\nFoo: Bar\r\n\r\n";
        let mock_io = Builder::new().write(wire).build();
        let mut http_stream = HttpSession::new(Box::new(mock_io));
        let mut new_response = ResponseHeader::build(StatusCode::OK, None).unwrap();
        new_response.append_header("Foo", "Bar").unwrap();
        http_stream.update_resp_headers = false;
        http_stream
            .write_response_header_ref(&new_response)
            .await
            .unwrap();
        let written = http_stream.write_body_buf().await.unwrap();
        assert!(written.is_none());
    }

    #[tokio::test]
    #[should_panic(expected = "There is still data left to write.")]
    async fn test_write_body_buf_write_timeout() {
        let wire1 = b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\n\r\n";
        let wire2 = b"abc";
        let mock_io = Builder::new()
            .write(wire1)
            .wait(Duration::from_millis(500))
            .write(wire2)
            .build();
        let mut http_stream = HttpSession::new(Box::new(mock_io));
        http_stream.write_timeout = Some(Duration::from_millis(100));
        let mut new_response = ResponseHeader::build(StatusCode::OK, None).unwrap();
        new_response.append_header("Content-Length", "3").unwrap();
        http_stream.update_resp_headers = false;
        http_stream
            .write_response_header_ref(&new_response)
            .await
            .unwrap();
        http_stream.body_write_buf = BytesMut::from(&b"abc"[..]);
        let res = http_stream.write_body_buf().await;
        assert_eq!(res.unwrap_err().etype(), &WriteTimedout);
    }

    #[tokio::test]
    async fn test_write_continue_resp() {
        let wire = b"HTTP/1.1 100 Continue\r\n\r\n";
        let mock_io = Builder::new().write(wire).build();
        let mut http_stream = HttpSession::new(Box::new(mock_io));
        http_stream.write_continue_response().await.unwrap();
    }

    #[test]
    fn test_is_upgrade_resp() {
        let mut response = ResponseHeader::build(StatusCode::SWITCHING_PROTOCOLS, None).unwrap();
        response.set_version(http::Version::HTTP_11);
        response.insert_header("Upgrade", "websocket").unwrap();
        response.insert_header("Connection", "upgrade").unwrap();
        assert!(is_upgrade_resp(&response));

        // wrong http version
        response.set_version(http::Version::HTTP_10);
        response.insert_header("Upgrade", "websocket").unwrap();
        response.insert_header("Connection", "upgrade").unwrap();
        assert!(!is_upgrade_resp(&response));

        // not 101
        response.set_status(http::StatusCode::OK).unwrap();
        response.set_version(http::Version::HTTP_11);
        assert!(!is_upgrade_resp(&response));
    }

    #[test]
    fn test_get_write_timeout() {
        let mut http_stream = HttpSession::new(Box::new(Builder::new().build()));
        let expected = Duration::from_secs(5);

        http_stream.set_write_timeout(expected);
        assert_eq!(Some(expected), http_stream.write_timeout(50));
    }

    #[test]
    fn test_get_write_timeout_none() {
        let http_stream = HttpSession::new(Box::new(Builder::new().build()));
        assert!(http_stream.write_timeout(50).is_none());
    }

    #[test]
    fn test_get_write_timeout_min_send_rate_zero_noop() {
        let mut http_stream = HttpSession::new(Box::new(Builder::new().build()));
        http_stream.set_min_send_rate(0);
        assert!(http_stream.write_timeout(50).is_none());
    }

    #[test]
    fn test_get_write_timeout_min_send_rate_overrides_write_timeout() {
        let mut http_stream = HttpSession::new(Box::new(Builder::new().build()));
        let expected = Duration::from_millis(29800);

        http_stream.set_write_timeout(Duration::from_secs(60));
        http_stream.set_min_send_rate(5000);

        assert_eq!(Some(expected), http_stream.write_timeout(149000));
    }

    #[test]
    fn test_get_write_timeout_min_send_rate_max_zero_buf() {
        let mut http_stream = HttpSession::new(Box::new(Builder::new().build()));
        let expected = Duration::from_secs(1);

        http_stream.set_min_send_rate(1);
        assert_eq!(Some(expected), http_stream.write_timeout(0));
    }
}

#[cfg(test)]
mod test_sync {
    use super::*;
    use http::StatusCode;
    use log::{debug, error};
    use std::str;

    fn init_log() {
        let _ = env_logger::builder().is_test(true).try_init();
    }

    #[test]
    fn test_response_to_wire() {
        init_log();
        let mut new_response = ResponseHeader::build(StatusCode::OK, None).unwrap();
        new_response.append_header("Foo", "Bar").unwrap();
        let mut wire = BytesMut::with_capacity(INIT_HEADER_BUF_SIZE);
        http_resp_header_to_buf(&new_response, &mut wire).unwrap();
        debug!("{}", str::from_utf8(wire.as_ref()).unwrap());
        let mut headers = [httparse::EMPTY_HEADER; 128];
        let mut resp = httparse::Response::new(&mut headers);
        let result = resp.parse(wire.as_ref());
        match result {
            Ok(_) => {}
            Err(e) => error!("{:?}", e),
        }
        assert!(result.unwrap().is_complete());
        // FIXME: the order is not guaranteed
        assert_eq!(b"Foo", headers[0].name.as_bytes());
        assert_eq!(b"Bar", headers[0].value);
    }
}
