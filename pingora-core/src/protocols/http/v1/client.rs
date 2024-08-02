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

//! HTTP/1.x client session

use bytes::{BufMut, Bytes, BytesMut};
use http::{header, header::AsHeaderName, HeaderValue, StatusCode, Version};
use log::{debug, trace};
use pingora_error::{Error, ErrorType::*, OrErr, Result, RetryType};
use pingora_http::{HMap, IntoCaseHeaderName, RequestHeader, ResponseHeader};
use pingora_timeout::timeout;
use std::io::ErrorKind;
use std::str;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::body::{BodyReader, BodyWriter};
use super::common::*;
use crate::protocols::http::HttpTask;
use crate::protocols::{Digest, SocketAddr, Stream, UniqueID, UniqueIDType};
use crate::utils::{BufRef, KVRef};

/// The HTTP 1.x client session
pub struct HttpSession {
    buf: Bytes,
    pub(crate) underlying_stream: Stream,
    raw_header: Option<BufRef>,
    preread_body: Option<BufRef>,
    body_reader: BodyReader,
    body_writer: BodyWriter,
    // timeouts:
    /// The read timeout, which will be applied to both reading the header and the body.
    /// The timeout is reset on every read. This is not a timeout on the overall duration of the
    /// response.
    pub read_timeout: Option<Duration>,
    /// The write timeout which will be applied to both writing request header and body.
    /// The timeout is reset on every write. This is not a timeout on the overall duration of the
    /// request.
    pub write_timeout: Option<Duration>,
    keepalive_timeout: KeepaliveStatus,
    pub(crate) digest: Box<Digest>,
    response_header: Option<Box<ResponseHeader>>,
    request_written: Option<Box<RequestHeader>>,
    bytes_sent: usize,
    upgraded: bool,
}

/// HTTP 1.x client session
impl HttpSession {
    /// Create a new http client session from an established (TCP or TLS) [`Stream`].
    pub fn new(stream: Stream) -> Self {
        // TODO: maybe we should put digest in the connection itself
        let digest = Box::new(Digest {
            ssl_digest: stream.get_ssl_digest(),
            timing_digest: stream.get_timing_digest(),
            proxy_digest: stream.get_proxy_digest(),
            socket_digest: stream.get_socket_digest(),
        });
        HttpSession {
            underlying_stream: stream,
            buf: Bytes::new(), // zero size, will be replaced by parsed header later
            raw_header: None,
            preread_body: None,
            body_reader: BodyReader::new(),
            body_writer: BodyWriter::new(),
            keepalive_timeout: KeepaliveStatus::Off,
            response_header: None,
            request_written: None,
            read_timeout: None,
            write_timeout: None,
            digest,
            bytes_sent: 0,
            upgraded: false,
        }
    }
    /// Write the request header to the server
    /// After the request header is sent. The caller can either start reading the response or
    /// sending request body if any.
    pub async fn write_request_header(&mut self, req: Box<RequestHeader>) -> Result<usize> {
        // TODO: make sure this can only be called once
        // init body writer
        self.init_req_body_writer(&req);

        let to_wire = http_req_header_to_wire(&req).unwrap();
        trace!("Writing request header: {to_wire:?}");

        let write_fut = self.underlying_stream.write_all(to_wire.as_ref());
        match self.write_timeout {
            Some(t) => match timeout(t, write_fut).await {
                Ok(res) => res,
                Err(_) => Err(std::io::Error::from(ErrorKind::TimedOut)),
            },
            None => write_fut.await,
        }
        .map_err(|e| match e.kind() {
            ErrorKind::TimedOut => {
                Error::because(WriteTimedout, "while writing request headers (timeout)", e)
            }
            _ => Error::because(WriteError, "while writing request headers", e),
        })?;

        self.underlying_stream
            .flush()
            .await
            .or_err(WriteError, "flushing request header")?;

        // write was successful
        self.request_written = Some(req);
        Ok(to_wire.len())
    }

    async fn do_write_body(&mut self, buf: &[u8]) -> Result<Option<usize>> {
        let written = self
            .body_writer
            .write_body(&mut self.underlying_stream, buf)
            .await;

        if let Ok(Some(num_bytes)) = written {
            self.bytes_sent += num_bytes;
        }

        written
    }

    /// Write request body. Return Ok(None) if no more body should be written, either due to
    /// Content-Length or the last chunk is already sent
    pub async fn write_body(&mut self, buf: &[u8]) -> Result<Option<usize>> {
        // TODO: verify that request header is sent already
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
            // request is done, reset the response body to close
            self.body_reader.init_content_length(0, b"");
        }
    }

    /// Flush local buffer and notify the server by sending the last chunk if chunked encoding is
    /// used.
    pub async fn finish_body(&mut self) -> Result<Option<usize>> {
        let res = self.body_writer.finish(&mut self.underlying_stream).await?;
        self.underlying_stream
            .flush()
            .await
            .or_err(WriteError, "flushing body")?;

        self.maybe_force_close_body_reader();
        Ok(res)
    }

    /// Read the response header from the server
    /// This function can be called multiple times, if the headers received are just informational
    /// headers.
    pub async fn read_response(&mut self) -> Result<usize> {
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
                    format!("Response header larger than {MAX_HEADER_SIZE}"),
                );
            }

            let read_fut = self.underlying_stream.read_buf(&mut buf);
            let read_result = match self.read_timeout {
                Some(t) => timeout(t, read_fut)
                    .await
                    .map_err(|_| Error::explain(ReadTimedout, "while reading response headers"))?,
                None => read_fut.await,
            };
            let n = match read_result {
                Ok(n) => match n {
                    0 => {
                        let mut e = Error::explain(
                            ConnectionClosed,
                            format!(
                                "while reading response headers, bytes already read: {already_read}",
                            ),
                        );
                        e.retry = RetryType::ReusedOnly;
                        return Err(e);
                    }
                    _ => {
                        n /* read n bytes, continue */
                    }
                },
                Err(e) => {
                    let true_io_error = e.raw_os_error().is_some();
                    let mut e = Error::because(
                        ReadError,
                        format!(
                            "while reading response headers, bytes already read: {already_read}",
                        ),
                        e,
                    );
                    // Likely OSError, typical if a previously reused connection drops it
                    if true_io_error {
                        e.retry = RetryType::ReusedOnly;
                    } // else: not safe to retry TLS error
                    return Err(e);
                }
            };
            already_read += n;
            let mut headers = [httparse::EMPTY_HEADER; MAX_HEADERS];
            let mut resp = httparse::Response::new(&mut headers);
            let parsed = parse_resp_buffer(&mut resp, &buf);
            match parsed {
                HeaderParseState::Complete(s) => {
                    self.raw_header = Some(BufRef(0, s));
                    self.preread_body = Some(BufRef(s, already_read));
                    let base = buf.as_ptr() as usize;
                    let mut header_refs = Vec::<KVRef>::with_capacity(resp.headers.len());

                    // Note: resp.headers has the correct number of headers
                    // while header_refs doesn't as it is still empty
                    let _num_headers = populate_headers(base, &mut header_refs, resp.headers);

                    let mut response_header = Box::new(ResponseHeader::build(
                        resp.code.unwrap(),
                        Some(resp.headers.len()),
                    )?);

                    response_header.set_version(match resp.version {
                        Some(1) => Version::HTTP_11,
                        Some(0) => Version::HTTP_10,
                        _ => Version::HTTP_09,
                    });

                    response_header.set_reason_phrase(resp.reason)?;

                    let buf = buf.freeze();

                    for header in header_refs {
                        let header_name = header.get_name_bytes(&buf);
                        let header_name = header_name.into_case_header_name();
                        let value_bytes = header.get_value_bytes(&buf);
                        let header_value = if cfg!(debug_assertions) {
                            // from_maybe_shared_unchecked() in debug mode still checks whether
                            // the header value is valid, which breaks the _obsolete_multiline
                            // support. To work around this, in debug mode, we replace CRLF with
                            // whitespace
                            if let Some(p) = value_bytes.windows(CRLF.len()).position(|w| w == CRLF)
                            {
                                let mut new_header = Vec::from_iter(value_bytes);
                                new_header[p] = b' ';
                                new_header[p + 1] = b' ';
                                unsafe {
                                    http::HeaderValue::from_maybe_shared_unchecked(new_header)
                                }
                            } else {
                                unsafe {
                                    http::HeaderValue::from_maybe_shared_unchecked(value_bytes)
                                }
                            }
                        } else {
                            // safe because this is from what we parsed
                            unsafe { http::HeaderValue::from_maybe_shared_unchecked(value_bytes) }
                        };
                        response_header
                            .append_header(header_name, header_value)
                            .or_err(InvalidHTTPHeader, "while parsing request header")?;
                    }

                    self.buf = buf;
                    self.upgraded = self.is_upgrade(&response_header).unwrap_or(false);
                    self.response_header = Some(response_header);
                    return Ok(s);
                }
                HeaderParseState::Partial => { /* continue the loop */ }
                HeaderParseState::Invalid(e) => {
                    return Error::e_because(
                        InvalidHTTPHeader,
                        format!("buf: {}", String::from_utf8_lossy(&buf).escape_default()),
                        e,
                    );
                }
            }
        }
    }

    /// Similar to [`Self::read_response()`], read the response header and then return a copy of it.
    pub async fn read_resp_header_parts(&mut self) -> Result<Box<ResponseHeader>> {
        self.read_response().await?;
        // safe to unwrap because it is just read
        Ok(Box::new(self.resp_header().unwrap().clone()))
    }

    /// Return a reference of the [`ResponseHeader`] if the response is read
    pub fn resp_header(&self) -> Option<&ResponseHeader> {
        self.response_header.as_deref()
    }

    /// Get the header value for the given header name from the response header
    /// If there are multiple headers under the same name, the first one will be returned
    /// Use `self.resp_header().header.get_all(name)` to get all the headers under the same name
    /// Always return `None` if the response is not read yet.
    pub fn get_header(&self, name: impl AsHeaderName) -> Option<&HeaderValue> {
        self.response_header
            .as_ref()
            .and_then(|h| h.headers.get(name))
    }

    /// Get the request header as raw bytes, `b""` when the header doesn't exist or response not read
    pub fn get_header_bytes(&self, name: impl AsHeaderName) -> &[u8] {
        self.get_header(name).map_or(b"", |v| v.as_bytes())
    }

    /// Return the status code of the response if read
    pub fn get_status(&self) -> Option<StatusCode> {
        self.response_header.as_ref().map(|h| h.status)
    }

    async fn do_read_body(&mut self) -> Result<Option<BufRef>> {
        self.init_body_reader();
        self.body_reader
            .read_body(&mut self.underlying_stream)
            .await
    }

    /// Read the response body into the internal buffer.
    /// Return `Ok(Some(ref)) after a successful read.
    /// Return `Ok(None)` if there is no more body to read.
    pub async fn read_body_ref(&mut self) -> Result<Option<&[u8]>> {
        let result = match self.read_timeout {
            Some(t) => match timeout(t, self.do_read_body()).await {
                Ok(res) => res,
                Err(_) => Error::e_explain(ReadTimedout, format!("reading body, timeout: {t:?}")),
            },
            None => self.do_read_body().await,
        };

        result.map(|maybe_body| maybe_body.map(|body_ref| self.body_reader.get_body(&body_ref)))
    }

    /// Similar to [`Self::read_body_ref`] but return `Bytes` instead of a slice reference.
    pub async fn read_body_bytes(&mut self) -> Result<Option<Bytes>> {
        let read = self.read_body_ref().await?;
        Ok(read.map(Bytes::copy_from_slice))
    }

    /// Whether there is no more body to read.
    pub fn is_body_done(&mut self) -> bool {
        self.init_body_reader();
        self.body_reader.body_done()
    }

    pub(super) fn get_headers_raw(&self) -> &[u8] {
        // TODO: these get_*() could panic. handle them better
        self.raw_header.as_ref().unwrap().get(&self.buf[..])
    }

    /// Get the raw response header bytes
    pub fn get_headers_raw_bytes(&self) -> Bytes {
        self.raw_header.as_ref().unwrap().get_bytes(&self.buf)
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

    /// Apply keepalive settings according to the server's response
    /// For HTTP 1.1, assume keepalive as long as there is no `Connection: Close` request header.
    /// For HTTP 1.0, only keepalive if there is an explicit header `Connection: keep-alive`.
    pub fn respect_keepalive(&mut self) {
        if self.get_status() == Some(StatusCode::SWITCHING_PROTOCOLS) {
            // make sure the connection is closed at the end when 101/upgrade is used
            self.set_keepalive(None);
            return;
        }
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
        } else if self.resp_header().map(|h| h.version) == Some(Version::HTTP_11) {
            self.set_keepalive(Some(0)); // on by default for http 1.1
        } else {
            self.set_keepalive(None); // off by default for http 1.0
        }
    }

    // Whether this session will be kept alive
    pub fn will_keepalive(&self) -> bool {
        // TODO: check self.body_writer. If it is http1.0 type then keepalive
        // cannot be used because the connection close is the signal of end body
        !matches!(self.keepalive_timeout, KeepaliveStatus::Off)
    }

    fn is_connection_keepalive(&self) -> Option<bool> {
        let request_keepalive = self
            .request_written
            .as_ref()
            .and_then(|req| is_buf_keepalive(req.headers.get(header::CONNECTION)));

        match request_keepalive {
            // ignore what the server sends if request disables keepalive explicitly
            Some(false) => Some(false),
            _ => is_buf_keepalive(self.get_header(header::CONNECTION)),
        }
    }

    /// `Keep-Alive: timeout=5, max=1000` => 5, 1000
    /// This is defined in the below spec, this not part of any RFC, so
    /// it's behavior is different on different platforms.
    /// https://developer.mozilla.org/en-US/docs/Web/HTTP/Headers/Keep-Alive
    fn get_keepalive_values(&self) -> (Option<u64>, Option<usize>) {
        let Some(keep_alive_header) = self.get_header("Keep-Alive") else {
            return (None, None);
        };

        let Ok(header_value) = str::from_utf8(keep_alive_header.as_bytes()) else {
            return (None, None);
        };

        let mut timeout = None;
        let mut max = None;

        for param in header_value.split(',') {
            let parts = param.split_once('=').map(|(k, v)| (k.trim(), v));
            match parts {
                Some(("timeout", timeout_value)) => timeout = timeout_value.trim().parse().ok(),
                Some(("max", max_value)) => max = max_value.trim().parse().ok(),
                _ => {}
            }
        }

        (timeout, max)
    }

    /// Close the connection abruptly. This allows to signal the server that the connection is closed
    /// before dropping [`HttpSession`]
    pub async fn shutdown(&mut self) {
        let _ = self.underlying_stream.shutdown().await;
    }

    /// Consume `self`, if the connection can be reused, the underlying stream will be returned.
    /// The returned connection can be kept in a connection pool so that next time the same
    /// server is being contacted. A new client session can be created via [`Self::new()`].
    /// If the connection cannot be reused, the underlying stream will be closed and `None` will be
    /// returned.
    pub async fn reuse(mut self) -> Option<Stream> {
        // TODO: this function is unnecessarily slow for keepalive case
        // because that case does not need async
        match self.keepalive_timeout {
            KeepaliveStatus::Off => {
                debug!("HTTP shutdown connection");
                self.shutdown().await;
                None
            }
            _ => Some(self.underlying_stream),
        }
    }

    fn init_body_reader(&mut self) {
        if self.body_reader.need_init() {
            /* follow https://tools.ietf.org/html/rfc7230#section-3.3.3 */
            let preread_body = self.preread_body.as_ref().unwrap().get(&self.buf[..]);

            if let Some(req) = self.request_written.as_ref() {
                if req.method == http::method::Method::HEAD {
                    self.body_reader.init_content_length(0, preread_body);
                    return;
                }
            }

            let upgraded = if let Some(code) = self.get_status() {
                match code.as_u16() {
                    101 => self.is_upgrade_req(),
                    100..=199 => {
                        // informational headers, not enough to init body reader
                        return;
                    }
                    204 | 304 => {
                        // no body by definition
                        self.body_reader.init_content_length(0, preread_body);
                        return;
                    }
                    _ => false,
                }
            } else {
                false
            };

            if upgraded {
                self.body_reader.init_http10(preread_body);
            } else if self.is_chunked_encoding() {
                // if chunked encoding, content-length should be ignored
                self.body_reader.init_chunked(preread_body);
            } else if let Some(cl) = self.get_content_length() {
                self.body_reader.init_content_length(cl, preread_body);
            } else {
                self.body_reader.init_http10(preread_body);
            }
        }
    }

    /// Whether this request is for upgrade
    pub fn is_upgrade_req(&self) -> bool {
        match self.request_written.as_deref() {
            Some(req) => is_upgrade_req(req),
            None => false,
        }
    }

    /// `Some(true)` if the this is a successful upgrade
    /// `Some(false)` if the request is an upgrade but the response refuses it
    /// `None` if the request is not an upgrade.
    fn is_upgrade(&self, header: &ResponseHeader) -> Option<bool> {
        if self.is_upgrade_req() {
            Some(is_upgrade_resp(header))
        } else {
            None
        }
    }

    fn get_content_length(&self) -> Option<usize> {
        buf_to_content_length(
            self.get_header(header::CONTENT_LENGTH)
                .map(|v| v.as_bytes()),
        )
    }

    fn is_chunked_encoding(&self) -> bool {
        is_header_value_chunked_encoding(self.get_header(header::TRANSFER_ENCODING))
    }

    fn init_req_body_writer(&mut self, header: &RequestHeader) {
        if is_upgrade_req(header) {
            self.body_writer.init_http10();
        } else {
            self.init_body_writer_comm(&header.headers)
        }
    }

    fn init_body_writer_comm(&mut self, headers: &HMap) {
        let te_value = headers.get(http::header::TRANSFER_ENCODING);
        if is_header_value_chunked_encoding(te_value) {
            // transfer-encoding takes priority over content-length
            self.body_writer.init_chunked();
        } else {
            let content_length =
                header_value_content_length(headers.get(http::header::CONTENT_LENGTH));
            match content_length {
                Some(length) => {
                    self.body_writer.init_content_length(length);
                }
                None => {
                    /* TODO: 1. connection: keepalive cannot be used,
                    2. mark connection must be closed */
                    self.body_writer.init_http10();
                }
            }
        }
    }

    // should (continue to) try to read response header or start reading response body
    fn should_read_resp_header(&self) -> bool {
        match self.get_status().map(|s| s.as_u16()) {
            Some(101) => false,      // switching protocol successful, no more header to read
            Some(100..=199) => true, // only informational header read
            Some(_) => false,
            None => true, // no response code, no header read yet
        }
    }

    pub async fn read_response_task(&mut self) -> Result<HttpTask> {
        if self.should_read_resp_header() {
            let resp_header = self.read_resp_header_parts().await?;
            let end_of_body = self.is_body_done();
            debug!("Response header: {resp_header:?}");
            trace!(
                "Raw Response header: {:?}",
                str::from_utf8(self.get_headers_raw()).unwrap()
            );
            Ok(HttpTask::Header(resp_header, end_of_body))
        } else if self.is_body_done() {
            // no body
            debug!("Response is done");
            Ok(HttpTask::Done)
        } else {
            /* need to read body */
            let body = self.read_body_bytes().await?;
            let end_of_body = self.is_body_done();
            debug!(
                "Response body: {} bytes, end: {end_of_body}",
                body.as_ref().map_or(0, |b| b.len())
            );
            trace!("Response body: {body:?}");
            Ok(HttpTask::Body(body, end_of_body))
        }
        // TODO: support h1 trailer
    }

    /// Return the [Digest] of the connection
    ///
    /// For reused connection, the timing in the digest will reflect its initial handshakes
    /// The caller should check if the connection is reused to avoid misuse the timing field.
    pub fn digest(&self) -> &Digest {
        &self.digest
    }

    /// Return a mutable [Digest] reference for the connection, see [`digest`] for more details.
    pub fn digest_mut(&mut self) -> &mut Digest {
        &mut self.digest
    }

    /// Return the server (peer) address recorded in the connection digest.
    pub fn server_addr(&self) -> Option<&SocketAddr> {
        self.digest()
            .socket_digest
            .as_ref()
            .map(|d| d.peer_addr())?
    }

    /// Return the client (local) address recorded in the connection digest.
    pub fn client_addr(&self) -> Option<&SocketAddr> {
        self.digest()
            .socket_digest
            .as_ref()
            .map(|d| d.local_addr())?
    }

    /// Get the reference of the [Stream] that this HTTP session is operating upon.
    pub fn stream(&self) -> &Stream {
        &self.underlying_stream
    }
}

#[inline]
fn parse_resp_buffer<'buf>(
    resp: &mut httparse::Response<'_, 'buf>,
    buf: &'buf [u8],
) -> HeaderParseState {
    let mut parser = httparse::ParserConfig::default();
    parser.allow_spaces_after_header_name_in_responses(true);
    parser.allow_obsolete_multiline_headers_in_responses(true);
    let res = match parser.parse_response(resp, buf) {
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

// TODO: change it to to_buf
#[inline]
pub(crate) fn http_req_header_to_wire(req: &RequestHeader) -> Option<BytesMut> {
    let mut buf = BytesMut::with_capacity(512);

    // Request-Line
    let method = req.method.as_str().as_bytes();
    buf.put_slice(method);
    buf.put_u8(b' ');
    buf.put_slice(req.raw_path());
    buf.put_u8(b' ');

    let version = match req.version {
        Version::HTTP_09 => "HTTP/0.9",
        Version::HTTP_10 => "HTTP/1.0",
        Version::HTTP_11 => "HTTP/1.1",
        Version::HTTP_2 => "HTTP/2",
        _ => {
            return None; /*TODO: unsupported version */
        }
    };
    buf.put_slice(version.as_bytes());
    buf.put_slice(CRLF);

    // headers
    req.header_to_h1_wire(&mut buf);
    buf.put_slice(CRLF);
    Some(buf)
}

impl UniqueID for HttpSession {
    fn id(&self) -> UniqueIDType {
        self.underlying_stream.id()
    }
}

#[cfg(test)]
mod tests_stream {
    use super::*;
    use crate::protocols::http::v1::body::ParseState;
    use crate::ErrorType;
    use tokio_test::io::Builder;

    fn init_log() {
        let _ = env_logger::builder().is_test(true).try_init();
    }

    #[tokio::test]
    async fn read_basic_response() {
        init_log();
        let input = b"HTTP/1.1 200 OK\r\n\r\n";
        let mock_io = Builder::new().read(&input[..]).build();
        let mut http_stream = HttpSession::new(Box::new(mock_io));
        let res = http_stream.read_response().await;
        assert_eq!(input.len(), res.unwrap());
        assert_eq!(0, http_stream.resp_header().unwrap().headers.len());
    }

    #[tokio::test]
    async fn read_response_custom_reason() {
        init_log();
        let input = b"HTTP/1.1 200 Just Fine\r\n\r\n";
        let mock_io = Builder::new().read(&input[..]).build();
        let mut http_stream = HttpSession::new(Box::new(mock_io));
        let res = http_stream.read_response().await;
        assert_eq!(input.len(), res.unwrap());
        assert_eq!(
            http_stream.resp_header().unwrap().get_reason_phrase(),
            Some("Just Fine")
        );
    }

    #[tokio::test]
    async fn read_response_default() {
        init_log();
        let input_header = b"HTTP/1.1 200 OK\r\n\r\n";
        let input_body = b"abc";
        let input_close = b""; // simulating close
        let mock_io = Builder::new()
            .read(&input_header[..])
            .read(&input_body[..])
            .read(&input_close[..])
            .build();
        let mut http_stream = HttpSession::new(Box::new(mock_io));
        let res = http_stream.read_response().await;
        assert_eq!(input_header.len(), res.unwrap());
        let res = http_stream.read_body_ref().await.unwrap();
        assert_eq!(res.unwrap(), input_body);
        assert_eq!(http_stream.body_reader.body_state, ParseState::HTTP1_0(3));
        let res = http_stream.read_body_ref().await.unwrap();
        assert_eq!(res, None);
        assert_eq!(http_stream.body_reader.body_state, ParseState::Complete(3));
    }

    #[tokio::test]
    async fn read_resp_header_with_space() {
        init_log();
        let input = b"HTTP/1.1 200 OK\r\nServer : pingora\r\n\r\n";
        let mock_io = Builder::new().read(&input[..]).build();
        let mut http_stream = HttpSession::new(Box::new(mock_io));
        let res = http_stream.read_response().await;
        assert_eq!(input.len(), res.unwrap());
        assert_eq!(1, http_stream.resp_header().unwrap().headers.len());
        assert_eq!(http_stream.get_header("Server").unwrap(), "pingora");
    }

    #[cfg(feature = "patched_http1")]
    #[tokio::test]
    async fn read_resp_header_with_utf8() {
        init_log();
        let input = "HTTP/1.1 200 OK\r\nServer👍: pingora\r\n\r\n".as_bytes();
        let mock_io = Builder::new().read(input).build();
        let mut http_stream = HttpSession::new(Box::new(mock_io));
        let resp = http_stream.read_resp_header_parts().await.unwrap();
        assert_eq!(1, http_stream.resp_header().unwrap().headers.len());
        assert_eq!(http_stream.get_header("Server👍").unwrap(), "pingora");
        assert_eq!(resp.headers.get("Server👍").unwrap(), "pingora");
    }

    #[tokio::test]
    #[should_panic(expected = "There is still data left to read.")]
    async fn read_timeout() {
        init_log();
        let input = b"HTTP/1.1 200 OK\r\n\r\n";
        let mock_io = Builder::new()
            .wait(Duration::from_secs(2))
            .read(&input[..])
            .build();
        let mut http_stream = HttpSession::new(Box::new(mock_io));
        http_stream.read_timeout = Some(Duration::from_secs(1));
        let res = http_stream.read_response().await;
        assert_eq!(res.unwrap_err().etype(), &ErrorType::ReadTimedout);
    }

    #[tokio::test]
    async fn read_2_buf() {
        init_log();
        let input1 = b"HTTP/1.1 200 OK\r\n";
        let input2 = b"Server: pingora\r\n\r\n";
        let mock_io = Builder::new().read(&input1[..]).read(&input2[..]).build();
        let mut http_stream = HttpSession::new(Box::new(mock_io));
        let res = http_stream.read_response().await;
        assert_eq!(input1.len() + input2.len(), res.unwrap());
        assert_eq!(
            input1.len() + input2.len(),
            http_stream.get_headers_raw().len()
        );
        assert_eq!(1, http_stream.resp_header().unwrap().headers.len());
        assert_eq!(http_stream.get_header("Server").unwrap(), "pingora");

        assert_eq!(Some(StatusCode::OK), http_stream.get_status());
        assert_eq!(Version::HTTP_11, http_stream.resp_header().unwrap().version);
    }

    #[tokio::test]
    #[should_panic(expected = "There is still data left to read.")]
    async fn read_invalid() {
        let input1 = b"HTP/1.1 200 OK\r\n";
        let input2 = b"Server: pingora\r\n\r\n";
        let mock_io = Builder::new().read(&input1[..]).read(&input2[..]).build();
        let mut http_stream = HttpSession::new(Box::new(mock_io));
        let res = http_stream.read_response().await;
        assert_eq!(&ErrorType::InvalidHTTPHeader, res.unwrap_err().etype());
    }

    #[tokio::test]
    async fn write() {
        let wire = b"GET /test HTTP/1.1\r\nFoo: Bar\r\n\r\n";
        let mock_io = Builder::new().write(wire).build();
        let mut http_stream = HttpSession::new(Box::new(mock_io));
        let mut new_request = RequestHeader::build("GET", b"/test", None).unwrap();
        new_request.insert_header("Foo", "Bar").unwrap();
        let n = http_stream
            .write_request_header(Box::new(new_request))
            .await
            .unwrap();
        assert_eq!(wire.len(), n);
    }

    #[tokio::test]
    #[should_panic(expected = "There is still data left to write.")]
    async fn write_timeout() {
        let wire = b"GET /test HTTP/1.1\r\nFoo: Bar\r\n\r\n";
        let mock_io = Builder::new()
            .wait(Duration::from_secs(2))
            .write(wire)
            .build();
        let mut http_stream = HttpSession::new(Box::new(mock_io));
        http_stream.write_timeout = Some(Duration::from_secs(1));
        let mut new_request = RequestHeader::build("GET", b"/test", None).unwrap();
        new_request.insert_header("Foo", "Bar").unwrap();
        let res = http_stream
            .write_request_header(Box::new(new_request))
            .await;
        assert_eq!(res.unwrap_err().etype(), &ErrorType::WriteTimedout);
    }

    #[tokio::test]
    #[should_panic(expected = "There is still data left to write.")]
    async fn write_body_timeout() {
        let header = b"POST /test HTTP/1.1\r\n\r\n";
        let body = b"abc";
        let mock_io = Builder::new()
            .write(&header[..])
            .wait(Duration::from_secs(2))
            .write(&body[..])
            .build();
        let mut http_stream = HttpSession::new(Box::new(mock_io));
        http_stream.write_timeout = Some(Duration::from_secs(1));

        let new_request = RequestHeader::build("POST", b"/test", None).unwrap();
        http_stream
            .write_request_header(Box::new(new_request))
            .await
            .unwrap();
        let res = http_stream.write_body(body).await;
        assert_eq!(res.unwrap_err().etype(), &WriteTimedout);
    }

    #[cfg(feature = "patched_http1")]
    #[tokio::test]
    async fn write_invalid_path() {
        let wire = b"GET /\x01\xF0\x90\x80 HTTP/1.1\r\nFoo: Bar\r\n\r\n";
        let mock_io = Builder::new().write(wire).build();
        let mut http_stream = HttpSession::new(Box::new(mock_io));
        let mut new_request = RequestHeader::build("GET", b"/\x01\xF0\x90\x80", None).unwrap();
        new_request.insert_header("Foo", "Bar").unwrap();
        let n = http_stream
            .write_request_header(Box::new(new_request))
            .await
            .unwrap();
        assert_eq!(wire.len(), n);
    }

    #[tokio::test]
    async fn read_informational() {
        init_log();
        let input1 = b"HTTP/1.1 100 Continue\r\n\r\n";
        let input2 = b"HTTP/1.1 204 OK\r\nServer: pingora\r\n\r\n";
        let mock_io = Builder::new().read(&input1[..]).read(&input2[..]).build();
        let mut http_stream = HttpSession::new(Box::new(mock_io));

        // read 100 header first
        let task = http_stream.read_response_task().await.unwrap();
        match task {
            HttpTask::Header(h, eob) => {
                assert_eq!(h.status, 100);
                assert!(!eob);
            }
            _ => {
                panic!("task should be header")
            }
        }
        // read 200 header next
        let task = http_stream.read_response_task().await.unwrap();
        match task {
            HttpTask::Header(h, eob) => {
                assert_eq!(h.status, 204);
                assert!(eob);
            }
            _ => {
                panic!("task should be header")
            }
        }
    }

    #[tokio::test]
    async fn init_body_for_upgraded_req() {
        use crate::protocols::http::v1::body::BodyMode;

        let wire =
            b"GET / HTTP/1.1\r\nConnection: Upgrade\r\nUpgrade: WS\r\nContent-Length: 0\r\n\r\n";
        let mock_io = Builder::new().write(wire).build();
        let mut http_stream = HttpSession::new(Box::new(mock_io));
        let mut new_request = RequestHeader::build("GET", b"/", None).unwrap();
        new_request.insert_header("Connection", "Upgrade").unwrap();
        new_request.insert_header("Upgrade", "WS").unwrap();
        // CL is ignored when Upgrade presents
        new_request.insert_header("Content-Length", "0").unwrap();
        let _ = http_stream
            .write_request_header(Box::new(new_request))
            .await
            .unwrap();
        assert_eq!(http_stream.body_writer.body_mode, BodyMode::HTTP1_0(0));
    }

    #[tokio::test]
    async fn read_switching_protocol() {
        init_log();
        let input1 = b"HTTP/1.1 101 Continue\r\n\r\n";
        let input2 = b"PAYLOAD";
        let mock_io = Builder::new().read(&input1[..]).read(&input2[..]).build();
        let mut http_stream = HttpSession::new(Box::new(mock_io));

        // read 100 header first
        let task = http_stream.read_response_task().await.unwrap();
        match task {
            HttpTask::Header(h, eob) => {
                assert_eq!(h.status, 101);
                assert!(!eob);
            }
            _ => {
                panic!("task should be header")
            }
        }
        // read body
        let task = http_stream.read_response_task().await.unwrap();
        match task {
            HttpTask::Body(b, eob) => {
                assert_eq!(b.unwrap(), &input2[..]);
                assert!(!eob);
            }
            _ => {
                panic!("task should be body")
            }
        }
        // read body
        let task = http_stream.read_response_task().await.unwrap();
        match task {
            HttpTask::Body(b, eob) => {
                assert!(b.is_none());
                assert!(eob);
            }
            _ => {
                panic!("task should be body with end of stream")
            }
        }
    }

    // Note: in debug mode, due to from_maybe_shared_unchecked() still tries to validate headers
    // values, so the code has to replace CRLF with whitespaces. In release mode, the CRLF is
    // reserved
    #[tokio::test]
    async fn read_obsolete_multiline_headers() {
        init_log();
        let input = b"HTTP/1.1 200 OK\r\nServer : pingora\r\n Foo: Bar\r\n\r\n";
        let mock_io = Builder::new().read(&input[..]).build();
        let mut http_stream = HttpSession::new(Box::new(mock_io));
        let res = http_stream.read_response().await;
        assert_eq!(input.len(), res.unwrap());

        assert_eq!(1, http_stream.resp_header().unwrap().headers.len());
        assert_eq!(
            http_stream.get_header("Server").unwrap(),
            "pingora   Foo: Bar"
        );

        let input = b"HTTP/1.1 200 OK\r\nServer : pingora\r\n\t  Fizz: Buzz\r\n\r\n";
        let mock_io = Builder::new().read(&input[..]).build();
        let mut http_stream = HttpSession::new(Box::new(mock_io));
        let res = http_stream.read_response().await;
        assert_eq!(input.len(), res.unwrap());
        assert_eq!(1, http_stream.resp_header().unwrap().headers.len());
        assert_eq!(
            http_stream.get_header("Server").unwrap(),
            "pingora  \t  Fizz: Buzz"
        );
    }

    #[cfg(feature = "patched_http1")]
    #[tokio::test]
    async fn read_headers_skip_invalid_line() {
        init_log();
        let input = b"HTTP/1.1 200 OK\r\n;\r\nFoo: Bar\r\n\r\n";
        let mock_io = Builder::new().read(&input[..]).build();
        let mut http_stream = HttpSession::new(Box::new(mock_io));
        let res = http_stream.read_response().await;
        assert_eq!(input.len(), res.unwrap());
        assert_eq!(1, http_stream.resp_header().unwrap().headers.len());
        assert_eq!(http_stream.get_header("Foo").unwrap(), "Bar");
    }

    #[tokio::test]
    async fn read_keepalive_headers() {
        init_log();

        async fn build_resp_with_keepalive(conn: &str) -> HttpSession {
            let input = format!("HTTP/1.1 200 OK\r\nConnection: {conn}\r\n\r\n");
            let mock_io = Builder::new().read(input.as_bytes()).build();
            let mut http_stream = HttpSession::new(Box::new(mock_io));
            let res = http_stream.read_response().await;
            assert_eq!(input.len(), res.unwrap());
            http_stream.respect_keepalive();
            http_stream
        }

        assert_eq!(
            build_resp_with_keepalive("close").await.keepalive_timeout,
            KeepaliveStatus::Off
        );

        assert_eq!(
            build_resp_with_keepalive("keep-alive")
                .await
                .keepalive_timeout,
            KeepaliveStatus::Infinite
        );

        assert_eq!(
            build_resp_with_keepalive("foo").await.keepalive_timeout,
            KeepaliveStatus::Infinite
        );

        assert_eq!(
            build_resp_with_keepalive("upgrade,close")
                .await
                .keepalive_timeout,
            KeepaliveStatus::Off
        );

        assert_eq!(
            build_resp_with_keepalive("upgrade, close")
                .await
                .keepalive_timeout,
            KeepaliveStatus::Off
        );

        assert_eq!(
            build_resp_with_keepalive("Upgrade, close")
                .await
                .keepalive_timeout,
            KeepaliveStatus::Off
        );

        assert_eq!(
            build_resp_with_keepalive("Upgrade,close")
                .await
                .keepalive_timeout,
            KeepaliveStatus::Off
        );

        assert_eq!(
            build_resp_with_keepalive("close,upgrade")
                .await
                .keepalive_timeout,
            KeepaliveStatus::Off
        );

        assert_eq!(
            build_resp_with_keepalive("close, upgrade")
                .await
                .keepalive_timeout,
            KeepaliveStatus::Off
        );

        assert_eq!(
            build_resp_with_keepalive("close,Upgrade")
                .await
                .keepalive_timeout,
            KeepaliveStatus::Off
        );

        assert_eq!(
            build_resp_with_keepalive("close, Upgrade")
                .await
                .keepalive_timeout,
            KeepaliveStatus::Off
        );

        async fn build_resp_with_keepalive_values(keep_alive: &str) -> HttpSession {
            let input = format!("HTTP/1.1 200 OK\r\nKeep-Alive: {keep_alive}\r\n\r\n");
            let mock_io = Builder::new().read(input.as_bytes()).build();
            let mut http_stream = HttpSession::new(Box::new(mock_io));
            let res = http_stream.read_response().await;
            assert_eq!(input.len(), res.unwrap());
            http_stream.respect_keepalive();
            http_stream
        }

        assert_eq!(
            build_resp_with_keepalive_values("timeout=5, max=1000")
                .await
                .get_keepalive_values(),
            (Some(5), Some(1000))
        );

        assert_eq!(
            build_resp_with_keepalive_values("max=1000, timeout=5")
                .await
                .get_keepalive_values(),
            (Some(5), Some(1000))
        );

        assert_eq!(
            build_resp_with_keepalive_values(" timeout = 5, max = 1000 ")
                .await
                .get_keepalive_values(),
            (Some(5), Some(1000))
        );

        assert_eq!(
            build_resp_with_keepalive_values("timeout=5")
                .await
                .get_keepalive_values(),
            (Some(5), None)
        );

        assert_eq!(
            build_resp_with_keepalive_values("max=1000")
                .await
                .get_keepalive_values(),
            (None, Some(1000))
        );

        assert_eq!(
            build_resp_with_keepalive_values("a=b")
                .await
                .get_keepalive_values(),
            (None, None)
        );

        assert_eq!(
            build_resp_with_keepalive_values("")
                .await
                .get_keepalive_values(),
            (None, None)
        );
    }

    /* Note: body tests are covered in server.rs */
}

#[cfg(test)]
mod test_sync {
    use super::*;
    use log::error;

    #[test]
    fn test_request_to_wire() {
        let mut new_request = RequestHeader::build("GET", b"/", None).unwrap();
        new_request.insert_header("Foo", "Bar").unwrap();
        let wire = http_req_header_to_wire(&new_request).unwrap();
        let mut headers = [httparse::EMPTY_HEADER; 128];
        let mut req = httparse::Request::new(&mut headers);
        let result = req.parse(wire.as_ref());
        match result {
            Ok(_) => {}
            Err(e) => error!("{:?}", e),
        }
        assert!(result.unwrap().is_complete());
        // FIXME: the order is not guaranteed
        assert_eq!("/", req.path.unwrap());
        assert_eq!(b"Foo", headers[0].name.as_bytes());
        assert_eq!(b"Bar", headers[0].value);
    }
}
