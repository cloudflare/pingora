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

//! HTTP/2 server session

use bytes::Bytes;
use futures::Future;
use h2::server;
use h2::server::SendResponse;
use h2::{RecvStream, SendStream};
use http::header::HeaderName;
use http::uri::PathAndQuery;
use http::{header, HeaderMap, Response, StatusCode};
use log::{debug, warn};
use pingora_http::{RequestHeader, ResponseHeader};
use pingora_timeout::timeout;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::task::ready;
use std::time::Duration;
use tokio::sync::Notify;

use crate::protocols::http::body_buffer::FixedBuffer;
use crate::protocols::http::date::get_cached_date;
use crate::protocols::http::v1::client::{
    http_req_header_to_wire, request_target_has_forbidden_byte,
};
use crate::protocols::http::v1::common::validate_content_length_without_transfer_encoding;
use crate::protocols::http::HttpTask;
use crate::protocols::{Digest, SocketAddr, Stream};
use crate::server::ShutdownWatch;
use crate::{Error, ErrorType, OrErr, Result};

const BODY_BUF_LIMIT: usize = 1024 * 64;

type H2Connection<S> = server::Connection<S, Bytes>;

pub use h2::server::Builder as H2Options;

// 64 KiB decoded header-list limit.
const DEFAULT_MAX_HEADER_LIST_SIZE: u32 = 64 * 1024;
const DEFAULT_MAX_CONCURRENT_STREAMS: u32 = 100;

// Per-connection lifetime budget for selected malformed downstream requests
// rejected during acceptance (currently ambiguous Content-Length framing and
// conflicting authority/Host fields). The count is NOT reset by valid streams,
// so a client cannot evade the bound by interleaving valid requests. A
// well-behaved client never sends these requests, so this is never tripped in
// practice; it bounds the total rejection work a misbehaving or malicious
// client can drive over the life of a single connection.
// TODO: expose this through HTTP/2 server configuration if deployments need a
// different tolerance for malformed stream rejections.
const MAX_MALFORMED_STREAMS_PER_CONN: usize = 32;

/// Build [`H2Options`] with bounded defaults for received requests.
///
/// Use this as the starting point when customizing options to retain the default
/// decoded header-list and concurrent-stream limits.
pub fn default_h2_options() -> H2Options {
    let mut options = H2Options::default();
    options.max_header_list_size(DEFAULT_MAX_HEADER_LIST_SIZE);
    options.max_concurrent_streams(DEFAULT_MAX_CONCURRENT_STREAMS);
    options
}

/// Perform HTTP/2 connection handshake with an established (TLS) connection.
///
/// The optional `options` allow to adjust certain HTTP/2 parameters and settings.
/// When `options` is [`None`], bounded defaults from [`default_h2_options`] are
/// used. See [`H2Options`] for more details.
pub async fn handshake(io: Stream, options: Option<H2Options>) -> Result<H2Connection<Stream>> {
    let options = options.unwrap_or_else(default_h2_options);
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

/// Drive a server-side HTTP/2 connection's accept loop, dispatching each new
/// stream to `on_session` until the connection closes.
///
/// This loop ends when:
///   * the client closes the H2 connection cleanly ([`HttpSession::from_h2_conn`]
///     returns `Ok(None)` after the final GOAWAY is flushed),
///   * the codec hits a connection error, or
///   * the configured idle timeout expires while no streams are active, or
///   * the runtime-level `graceful_shutdown_timeout_seconds` ceiling fires and
///     force-kills the task driving this future.
///
/// On a shutdown signal:
///   1. [`h2::server::Connection::graceful_shutdown`] is called, which
///      enqueues a GOAWAY with the maximum possible last_stream_id per
///      RFC 9113 §6.8. The codec emits a second, real GOAWAY when the
///      connection finishes draining.
///   2. The loop continues calling [`HttpSession::from_h2_conn`] so that:
///      - streams whose HEADERS were buffered in the codec before the shutdown
///        signal arrived are still surfaced and dispatched,
///      - streams the client opens after observing GOAWAY(MAX) but below the
///        eventual last_stream_id are also dispatched, and
///      - the codec is driven to completion so the final GOAWAY can be
///        flushed and the connection closed cleanly.
///
/// `on_session` is invoked once per accepted stream, together with a
/// [`StreamGuard`]. Typical callers spawn a task to process the session so the
/// accept loop is not blocked, and move the guard into that task so its lifetime
/// matches the session.
///
/// Note: this function does not impose its own per-connection drain timeout
/// (after a shutdown signal). The runtime-level `graceful_shutdown_timeout_seconds`
/// is the only ceiling there, so a slow client can keep this future alive up to
/// that bound during drain.
// TODO: add a per-connection drain timeout to bound how long a single
// misbehaving client can keep this task alive after GOAWAY.
pub(crate) async fn accept_downstream_sessions<F>(
    mut conn: H2Connection<Stream>,
    digest: Arc<Digest>,
    mut shutdown: ShutdownWatch,
    idle_timeout: Option<Duration>,
    mut on_session: F,
) where
    F: FnMut(HttpSession, StreamGuard),
{
    let mut shutdown_initiated = false;
    // Per-connection budget for malformed streams (see MAX_MALFORMED_STREAMS_PER_CONN).
    let mut malformed_streams = 0usize;
    // In-flight sessions, decremented by the `StreamGuard` given to `on_session`.
    let active = Arc::new(ActiveSessions::new());
    loop {
        let h2_stream = if shutdown_initiated {
            HttpSession::from_h2_conn_with_malformed_budget(
                &mut conn,
                digest.clone(),
                &mut malformed_streams,
            )
            .await
        } else {
            tokio::select! {
                // Poll the shutdown signal first so a concurrent signal is
                // observed deterministically. `from_h2_conn` is cancel-safe
                // and is polled again on the next iteration.
                biased;
                _ = shutdown.changed() => {
                    conn.graceful_shutdown();
                    shutdown_initiated = true;
                    continue;
                }
                h2_stream = HttpSession::from_h2_conn_with_malformed_budget(
                    &mut conn,
                    digest.clone(),
                    &mut malformed_streams,
                ) => h2_stream,
                // Any accepted stream cancels this future. The next iteration
                // waits for all active streams to finish before starting a fresh
                // idle period.
                _ = wait_for_idle_timeout(&active, idle_timeout.unwrap_or_default()), if idle_timeout.is_some() => {
                    // Idle with nothing in flight: drop `conn` to close the
                    // socket now (no graceful GOAWAY wait that could hang on
                    // a dead peer).
                    return;
                }
            }
        };
        match h2_stream {
            Err(e) => {
                // It is common for the client to just disconnect TCP without
                // properly closing H2. So we don't log the errors here
                debug!("H2 error when accepting new stream {e}");
                return;
            }
            // None means the connection is ready to be closed
            Ok(None) => return,
            // The offending stream was already answered or reset; keep the
            // connection alive and continue accepting sibling streams.
            Ok(Some(H2Accept::Rejected)) => continue,
            Ok(Some(H2Accept::Session(session))) => {
                on_session(session, active.start_session());
            }
        }
    }
}

/// Tracks one in-flight downstream H2 session for [`accept_downstream_sessions`].
/// `on_session` receives it alongside each session; keep it alive for as long as
/// the session is being processed (e.g. move it into the spawned task) so the
/// accept loop's idle timeout can tell a busy connection from an idle one. It
/// decrements the in-flight counter when dropped.
pub(crate) struct StreamGuard(Arc<ActiveSessions>);

impl Drop for StreamGuard {
    fn drop(&mut self) {
        if self.0.count.fetch_sub(1, Ordering::Relaxed) == 1 {
            self.0.idle.notify_one();
        }
    }
}

struct ActiveSessions {
    count: AtomicUsize,
    idle: Notify,
}

impl ActiveSessions {
    fn new() -> Self {
        Self {
            count: AtomicUsize::new(0),
            idle: Notify::new(),
        }
    }

    fn start_session(self: &Arc<Self>) -> StreamGuard {
        self.count.fetch_add(1, Ordering::Relaxed);
        StreamGuard(self.clone())
    }

    async fn wait_until_idle(&self) {
        while self.count.load(Ordering::Relaxed) != 0 {
            self.idle.notified().await;
        }
    }
}

async fn wait_for_idle_timeout(active: &ActiveSessions, idle_timeout: Duration) {
    active.wait_until_idle().await;
    if !idle_timeout.is_zero() {
        pingora_timeout::sleep(idle_timeout).await;
    }
}

use futures::task::Context;
use futures::task::Poll;
use std::pin::Pin;
/// The future to poll for an idle session.
///
/// Calling `.await` in this object will not return until the client decides to close this stream.
pub struct Idle<'a>(&'a mut HttpSession);

impl Future for Idle<'_> {
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
    /// The write timeout which will be applied to writing response body.
    /// The timeout is reset on every write. This is not a timeout on the overall duration of the
    /// response.
    pub write_timeout: Option<Duration>,
    // How long to wait when draining (discarding) request body
    total_drain_timeout: Option<Duration>,
}

/// The outcome of accepting the next event on an HTTP/2 downstream connection.
///
/// Returned by [`HttpSession::from_h2_conn`] so the accept loop can react to a
/// rejected stream without tearing down the whole connection.
///
/// The session is stored inline to avoid a heap allocation for every accepted
/// HTTP/2 stream.
#[allow(clippy::large_enum_variant)]
pub enum H2Accept {
    /// A new request stream was established and is ready to be served.
    Session(HttpSession),
    /// The next stream was rejected during acceptance (for example, its request
    /// target contained a forbidden byte) and has already been answered or
    /// reset. Sibling streams and the connection are unaffected; the caller
    /// should continue accepting.
    Rejected,
}

fn authority_host_mismatch(request: &RequestHeader) -> bool {
    let Some(authority) = request.uri.authority() else {
        return false;
    };

    let mut hosts = request.headers.get_all(header::HOST).iter();
    match (hosts.next(), hosts.next()) {
        (Some(host), None) => host.as_bytes() != authority.as_str().as_bytes(),
        (Some(_), Some(_)) => true,
        (None, _) => false,
    }
}

fn account_malformed_stream(malformed_streams: &mut usize) -> Result<()> {
    *malformed_streams += 1;
    if *malformed_streams >= MAX_MALFORMED_STREAMS_PER_CONN {
        // Rare, connection-level abuse signal (at most once per torn-down
        // connection), so warn! is flood-safe here and useful for detecting
        // abuse in production.
        warn!(
            "tearing down downstream h2 connection after \
             {malformed_streams} malformed requests"
        );
        return Error::e_explain(
            ErrorType::H2Error,
            "too many malformed downstream requests on connection",
        );
    }
    Ok(())
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
    /// The return value distinguishes three outcomes:
    /// * `Ok(Some(`[`H2Accept::Session`]`))` — a new stream is ready to serve.
    /// * `Ok(Some(`[`H2Accept::Rejected`]`))` — the stream was answered or reset
    ///   during acceptance; the caller should keep accepting sibling streams.
    /// * `Ok(None)` — the connection is closing, so the loop can exit.
    ///
    /// This convenience wrapper uses a fresh malformed-stream counter on every
    /// call. It preserves the public API, but does not enforce the
    /// [`MAX_MALFORMED_STREAMS_PER_CONN`] budget across repeated calls by an
    /// external accept loop. Pingora's built-in downstream accept loop uses the
    /// internal budgeted helper to share one counter for the connection lifetime.
    pub async fn from_h2_conn(
        conn: &mut H2Connection<Stream>,
        digest: Arc<Digest>,
    ) -> Result<Option<H2Accept>> {
        let mut malformed_streams = 0usize;
        Self::from_h2_conn_with_malformed_budget(conn, digest, &mut malformed_streams).await
    }

    /// Like [`Self::from_h2_conn`], but shares a malformed-stream counter across
    /// calls for the same connection.
    ///
    /// `malformed_streams` is a per-connection counter, owned by the caller and
    /// shared across every call for the same connection. It tracks the total
    /// number of malformed streams counted by these acceptance checks over the
    /// connection's lifetime. Valid streams do not reset it, so a client cannot
    /// evade the [`MAX_MALFORMED_STREAMS_PER_CONN`] bound by interleaving valid
    /// requests.
    /// Callers should initialize it to `0` once per connection.
    async fn from_h2_conn_with_malformed_budget(
        conn: &mut H2Connection<Stream>,
        digest: Arc<Digest>,
        malformed_streams: &mut usize,
    ) -> Result<Option<H2Accept>> {
        // NOTE: conn.accept().await is what drives the entire connection.
        let res = conn.accept().await.transpose().or_err(
            ErrorType::H2Error,
            "while accepting new downstream requests",
        )?;

        let Some((req, mut send_response)) = res else {
            return Ok(None);
        };

        let (request_header, request_body_reader) = req.into_parts();
        let request_header: RequestHeader = request_header.into();

        // Depending on how the request URI is parsed, control bytes
        // (including CR and LF) may be accepted in the `:path`
        // pseudo-header. Reject them here as defense-in-depth: these
        // bytes are not permitted in a URI, and they would be dangerous
        // if forwarded to an HTTP/1.1 upstream. Reset only the offending
        // stream so sibling streams on the connection are unaffected.
        if request_target_has_forbidden_byte(request_header.raw_path()) {
            debug!("Rejecting H2 request: forbidden delimiter byte in request target");
            send_response.send_reset(h2::Reason::PROTOCOL_ERROR);
            return Ok(Some(H2Accept::Rejected));
        }

        // Reject ambiguous Content-Length framing at the stream level.
        // Identical duplicates and comma-combined identical values are
        // reconciled (RFC 9110 section 8.6), but conflicting or unparseable
        // values are an unrecoverable error and are treated as a stream
        // error per RFC 9113 section 8.1.1. This keeps the request path
        // consistent with HTTP/1 and prevents ambiguous framing from being
        // forwarded (e.g. when downgraded to an HTTP/1 upstream).
        if let Err(e) = validate_content_length_without_transfer_encoding(&request_header.headers) {
            // debug, not warn: per-stream, client-influenced; avoids log
            // floods. Connection-level abuse is surfaced via the error below.
            debug!("rejecting downstream h2 request: {e}");
            send_response.send_reset(h2::Reason::PROTOCOL_ERROR);

            account_malformed_stream(malformed_streams)?;
            return Ok(Some(H2Accept::Rejected));
        }

        if authority_host_mismatch(&request_header) {
            // RFC 9113 section 8.3.1 says a server SHOULD treat a request as
            // malformed when Host does not match :authority after
            // normalization. Until shared authority normalization exists,
            // conservatively require identical field values:
            // https://www.rfc-editor.org/rfc/rfc9113.html#section-8.3.1
            //
            // RFC 9112 section 3.2 requires HTTP/1.1 servers to reject more
            // than one Host field. When :authority is present, reject
            // duplicates that cannot be compared unambiguously before a
            // possible H1 downgrade:
            // https://www.rfc-editor.org/rfc/rfc9112.html#section-3.2
            debug!("rejecting downstream h2 request: conflicting :authority and Host fields");
            let mut response = Response::new(());
            *response.status_mut() = StatusCode::BAD_REQUEST;
            // RFC 9113 section 8.1.1 requires malformed requests to use a
            // PROTOCOL_ERROR stream error and permits sending an HTTP response
            // first. h2 replaces the queued response when reset immediately,
            // so prioritize an observable 400. An unfinished request body can
            // still cause h2 to reset the stream when its handles are dropped:
            // https://www.rfc-editor.org/rfc/rfc9113.html#section-8.1.1
            if let Err(e) = send_response.send_response(response, true) {
                // The client can reset this stream before the rejection is
                // written. Keep that stream-local failure from closing the
                // connection and dropping sibling streams.
                debug!("failed to send downstream h2 authority rejection: {e}");
            }
            account_malformed_stream(malformed_streams)?;
            return Ok(Some(H2Accept::Rejected));
        }

        Ok(Some(H2Accept::Session(HttpSession {
            request_header,
            request_body_reader,
            send_response,
            send_response_body: None,
            response_written: None,
            ended: false,
            body_read: 0,
            body_sent: 0,
            retry_buffer: None,
            digest,
            write_timeout: None,
            total_drain_timeout: None,
        })))
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

    #[doc(hidden)]
    pub fn poll_read_body_bytes(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Bytes, h2::Error>>> {
        let data = match ready!(self.request_body_reader.poll_data(cx)).transpose() {
            Ok(data) => data,
            Err(err) => return Poll::Ready(Some(Err(err))),
        };

        if let Some(data) = data {
            self.body_read += data.len();
            self.request_body_reader
                .flow_control()
                .release_capacity(data.len())?;
            return Poll::Ready(Some(Ok(data)));
        }

        Poll::Ready(None)
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
    // NOTE for h2 it may be worth allowing cancellation of the stream via reset.
    pub async fn drain_request_body(&mut self) -> Result<()> {
        if self.is_body_done() {
            return Ok(());
        }
        match self.total_drain_timeout {
            Some(t) => match timeout(t, self.do_drain_request_body()).await {
                Ok(res) => res,
                Err(_) => Error::e_explain(
                    ErrorType::ReadTimedout,
                    format!("draining body, timeout: {t:?}"),
                ),
            },
            None => self.do_drain_request_body().await,
        }
    }

    /// Sets the downstream write timeout. This will trigger if we're unable
    /// to write to the stream after `timeout`.
    pub fn set_write_timeout(&mut self, timeout: Option<Duration>) {
        self.write_timeout = timeout;
    }

    /// Get the write timeout.
    pub fn get_write_timeout(&self) -> Option<Duration> {
        self.write_timeout
    }

    /// Sets the total drain timeout. This `timeout` will be used while draining
    /// the request body.
    pub fn set_total_drain_timeout(&mut self, timeout: Option<Duration>) {
        self.total_drain_timeout = timeout;
    }

    /// Get the total drain timeout.
    pub fn get_total_drain_timeout(&self) -> Option<Duration> {
        self.total_drain_timeout
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
    pub async fn write_body(&mut self, data: Bytes, end: bool) -> Result<()> {
        match self.write_timeout {
            Some(t) => match timeout(t, self.do_write_body(data, end)).await {
                Ok(res) => res,
                Err(_) => Error::e_explain(
                    ErrorType::WriteTimedout,
                    format!("writing body, timeout: {t:?}"),
                ),
            },
            None => self.do_write_body(data, end).await,
        }
    }

    async fn do_write_body(&mut self, data: Bytes, end: bool) -> Result<()> {
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
        super::write_body(writer, data, end, self.write_timeout)
            .await
            .map_err(|e| e.into_down())?;
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

    pub async fn response_duplex_vec(&mut self, tasks: Vec<HttpTask>) -> Result<bool> {
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
                            self.write_body(d, end).await.map_err(|e| e.into_down())?;
                        }
                        end
                    }
                    None => end,
                },
                HttpTask::UpgradedBody(..) => {
                    // Seeing an Upgraded body means that the upstream session
                    // was H1.1 that upgraded.
                    //
                    // While the downstream H2 session may encapsulate the opaque body bytes,
                    // this represents an undefined discrepancy and change between how
                    // the upstream and downstream sessions began intepreting the response body.
                    return Error::e_explain(
                        ErrorType::InternalError,
                        "upgraded body on h2 server session",
                    );
                }
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

    /// Return a string `$METHOD $PATH, Host: $HOST`. Mostly for logging and debug purpose
    pub fn request_summary(&self) -> String {
        format!(
            "{} {}, Host: {}:{}",
            self.request_header.method,
            self.request_header
                .uri
                .path_and_query()
                .map(PathAndQuery::as_str)
                .unwrap_or_default(),
            self.request_header.uri.host().unwrap_or_default(),
            self.req_header()
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
    /// This will send an `INTERNAL_ERROR` stream error to the client.
    pub fn shutdown(&mut self) {
        self.shutdown_with_reason(h2::Reason::INTERNAL_ERROR);
    }

    /// Give up the stream abruptly with a custom reason.
    ///
    /// This will send a `RST_STREAM` frame with the given reason to the client.
    ///
    /// Useful reasons include:
    /// - [`h2::Reason::HTTP_1_1_REQUIRED`] - Signal to the client that HTTP/1.1 should be used
    ///   instead. Per RFC 7540 §9.1.2, clients should retry the request over HTTP/1.1.
    /// - [`h2::Reason::CANCEL`] - Indicate the stream is no longer needed.
    /// - [`h2::Reason::REFUSED_STREAM`] - Indicate the stream was refused before processing.
    pub fn shutdown_with_reason(&mut self, reason: h2::Reason) {
        if !self.ended {
            self.send_response.send_reset(reason);
        }
    }

    #[doc(hidden)]
    pub fn take_response_body_writer(&mut self) -> Option<SendStream<Bytes>> {
        self.send_response_body.take()
    }

    // This is a hack for pingora-proxy to create subrequests from h2 server session
    // TODO: be able to convert from h2 to h1 subrequest
    pub fn pseudo_raw_h1_request_header(&self) -> Bytes {
        // `http_req_header_to_wire` returns `None` for an unsupported HTTP
        // version or a request target containing forbidden delimiter bytes.
        // Neither should happen here: H2 sessions always carry `HTTP_2`, and
        // forbidden targets are rejected when the session is created (see
        // `from_h2_conn`).
        http_req_header_to_wire(&self.request_header)
            .map(|buf| buf.freeze())
            .expect("http_req_header_to_wire should not fail for a validated h2 request")
    }

    /// Whether there is no more body to read
    pub fn is_body_done(&self) -> bool {
        // Check no body in request
        // Also check we hit end of stream
        self.is_body_empty() || self.request_body_reader.is_end_stream()
    }

    /// Whether there is any body to read. true means there no body in request.
    pub fn is_body_empty(&self) -> bool {
        self.body_read == 0
            && (self.request_body_reader.is_end_stream()
                || self
                    .request_header
                    .headers
                    .get(header::CONTENT_LENGTH)
                    .is_some_and(|cl| cl.as_bytes() == b"0"))
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
    pub fn idle(&mut self) -> Idle<'_> {
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
    use bytes::Bytes;
    use futures::SinkExt;
    use h2::frame::{Frame, Headers, Pseudo, Reset, Settings};
    use http::{HeaderValue, Method, Request, Uri};
    use tokio::io::{duplex, AsyncWriteExt, DuplexStream};
    use tokio::sync::oneshot;
    use tokio_stream::StreamExt;

    async fn advertised_settings(options: Option<H2Options>) -> Settings {
        let (mut client, server) = duplex(65536);
        let handshake = tokio::spawn(async move { handshake(Box::new(server), options).await });

        client
            .write_all(b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n")
            .await
            .unwrap();
        let mut codec: h2::Codec<DuplexStream, Bytes> = h2::Codec::new(client);
        let settings = match codec.next().await.unwrap().unwrap() {
            Frame::Settings(settings) => settings,
            frame => panic!("expected SETTINGS frame, received {frame:?}"),
        };

        let _ = handshake.await.unwrap().unwrap();
        settings
    }

    #[test]
    fn test_authority_host_mismatch() {
        let request = |hosts: &[&str]| {
            let mut request = Request::builder()
                .uri("https://authority.example/test")
                .body(())
                .unwrap();
            for host in hosts {
                request
                    .headers_mut()
                    .append(header::HOST, HeaderValue::from_str(host).unwrap());
            }
            RequestHeader::from(request.into_parts().0)
        };

        assert!(!authority_host_mismatch(&request(&[])));
        assert!(!authority_host_mismatch(&request(&["authority.example"])));
        assert!(authority_host_mismatch(&request(&["other.example"])));
        assert!(authority_host_mismatch(&request(&["AUTHORITY.EXAMPLE"])));
        assert!(authority_host_mismatch(&request(&[
            "authority.example:443"
        ])));
        assert!(authority_host_mismatch(&request(&[
            "authority.example",
            "authority.example",
        ])));
        assert!(authority_host_mismatch(&request(&[
            "authority.example",
            "other.example",
        ])));

        let request = RequestHeader::from(
            Request::builder()
                .uri("/test")
                .header(header::HOST, "host.example")
                .body(())
                .unwrap()
                .into_parts()
                .0,
        );
        assert!(!authority_host_mismatch(&request));
    }

    #[tokio::test]
    async fn test_server_rejects_authority_host_mismatch_with_400() {
        let (client, server) = duplex(65536);

        let client = tokio::spawn(async move {
            let (h2, connection) = h2::client::handshake(client).await.unwrap();
            tokio::spawn(async move {
                let _ = connection.await;
            });

            let mut h2 = h2.ready().await.unwrap();
            let mismatched = Request::builder()
                .method(Method::GET)
                .uri("https://authority.example/test")
                .header(header::HOST, "other.example")
                .body(())
                .unwrap();
            let (response, request_body) = h2.send_request(mismatched, false).unwrap();

            let response = response.await.unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
            drop(response);
            drop(request_body);

            let mut h2 = h2.ready().await.unwrap();
            let duplicate = Request::builder()
                .method(Method::GET)
                .uri("https://authority.example/test")
                .header(header::HOST, "authority.example")
                .header(header::HOST, "authority.example")
                .body(())
                .unwrap();
            let (response, _) = h2.send_request(duplicate, true).unwrap();

            let response = response.await.unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
            let mut body = response.into_body();
            assert!(body.data().await.is_none());

            let mut h2 = h2.ready().await.unwrap();
            let matching = Request::builder()
                .method(Method::GET)
                .uri("https://authority.example/test")
                .header(header::HOST, "authority.example")
                .body(())
                .unwrap();
            let (response, _) = h2.send_request(matching, true).unwrap();

            assert_eq!(response.await.unwrap().status(), StatusCode::NO_CONTENT);
        });

        let mut connection = handshake(Box::new(server), None).await.unwrap();
        let digest = Arc::new(Digest::default());
        let accepted = HttpSession::from_h2_conn(&mut connection, digest.clone())
            .await
            .unwrap();
        assert!(
            matches!(accepted, Some(H2Accept::Rejected)),
            "mismatched authority reached the application"
        );

        let accepted = HttpSession::from_h2_conn(&mut connection, digest.clone())
            .await
            .unwrap();
        assert!(
            matches!(accepted, Some(H2Accept::Rejected)),
            "duplicate Host fields reached the application"
        );

        let Some(H2Accept::Session(mut session)) =
            HttpSession::from_h2_conn(&mut connection, digest.clone())
                .await
                .unwrap()
        else {
            panic!("matching authority did not reach the application");
        };
        assert_eq!(
            session.req_header().headers[header::HOST],
            "authority.example"
        );
        session
            .write_response_header(
                Box::new(ResponseHeader::build(StatusCode::NO_CONTENT, Some(0)).unwrap()),
                true,
            )
            .unwrap();
        drop(session);

        let done = timeout(
            Duration::from_secs(1),
            HttpSession::from_h2_conn(&mut connection, digest),
        )
        .await
        .expect("connection did not finish after authority mismatch test")
        .expect("connection failed after authority mismatch test");
        assert!(done.is_none());

        client.await.unwrap();
    }

    #[tokio::test]
    async fn test_failed_authority_rejection_write_is_stream_local() {
        let (mut client, server) = duplex(65536);
        let (frames_sent, wait_for_frames) = oneshot::channel();

        let client = tokio::spawn(async move {
            client
                .write_all(b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n")
                .await
                .unwrap();
            let mut codec: h2::Codec<DuplexStream, Bytes> = h2::Codec::new(client);
            codec.send(Settings::default().into()).await.unwrap();
            codec.send(Settings::ack().into()).await.unwrap();

            let mut fields = HeaderMap::new();
            fields.insert(header::HOST, HeaderValue::from_static("other.example"));
            let mut mismatched = Headers::new(
                1.into(),
                Pseudo::request(
                    Method::GET,
                    Uri::from_static("https://authority.example/test"),
                    None,
                ),
                fields,
            );
            mismatched.set_end_headers();
            codec.send(mismatched.into()).await.unwrap();
            codec
                .send(Reset::new(1.into(), h2::Reason::CANCEL).into())
                .await
                .unwrap();

            let mut fields = HeaderMap::new();
            fields.insert(header::HOST, HeaderValue::from_static("authority.example"));
            let mut matching = Headers::new(
                3.into(),
                Pseudo::request(
                    Method::GET,
                    Uri::from_static("https://authority.example/test"),
                    None,
                ),
                fields,
            );
            matching.set_end_headers();
            matching.set_end_stream();
            codec.send(matching.into()).await.unwrap();
            frames_sent.send(()).unwrap();

            timeout(Duration::from_secs(1), async {
                while let Some(frame) = codec.next().await {
                    if let Frame::Headers(headers) = frame.unwrap() {
                        if headers.stream_id() == 3u32 {
                            return;
                        }
                    }
                }
                panic!("connection closed before the sibling response");
            })
            .await
            .expect("timed out waiting for the sibling response");
        });

        let mut connection = handshake(Box::new(server), None).await.unwrap();
        wait_for_frames.await.unwrap();
        let digest = Arc::new(Digest::default());

        let accepted = HttpSession::from_h2_conn(&mut connection, digest.clone())
            .await
            .unwrap();
        assert!(
            matches!(accepted, Some(H2Accept::Rejected)),
            "reset authority mismatch reached the application"
        );

        let Some(H2Accept::Session(mut session)) =
            HttpSession::from_h2_conn(&mut connection, digest.clone())
                .await
                .unwrap()
        else {
            panic!("sibling stream was dropped after the failed 400 write");
        };
        session
            .write_response_header(
                Box::new(ResponseHeader::build(StatusCode::NO_CONTENT, Some(0)).unwrap()),
                true,
            )
            .unwrap();
        drop(session);

        let done = timeout(
            Duration::from_secs(1),
            HttpSession::from_h2_conn(&mut connection, digest),
        )
        .await
        .expect("connection did not finish after the sibling response")
        .expect("connection failed after the sibling response");
        assert!(done.is_none());
        client.await.unwrap();
    }

    #[tokio::test]
    async fn test_authority_mismatch_exhausts_malformed_stream_budget() {
        let (client, server) = duplex(65536);

        let client = tokio::spawn(async move {
            let (h2, connection) = h2::client::handshake(client).await.unwrap();
            tokio::spawn(async move {
                let _ = connection.await;
            });

            let mut h2 = h2.ready().await.unwrap();
            let mismatched = Request::builder()
                .method(Method::GET)
                .uri("https://authority.example/test")
                .header(header::HOST, "other.example")
                .body(())
                .unwrap();
            let (response, _) = h2.send_request(mismatched, true).unwrap();

            // The connection can close before the queued 400 is flushed when
            // this request exhausts the connection-level malformed budget.
            let _ = response.await;
        });

        let mut connection = handshake(Box::new(server), None).await.unwrap();
        let digest = Arc::new(Digest::default());
        let mut malformed_streams = MAX_MALFORMED_STREAMS_PER_CONN - 1;

        let err = match HttpSession::from_h2_conn_with_malformed_budget(
            &mut connection,
            digest,
            &mut malformed_streams,
        )
        .await
        {
            Ok(Some(_)) => panic!("authority mismatch must not surface as a session"),
            Ok(None) => panic!("connection ended before malformed budget was exhausted"),
            Err(err) => err,
        };

        assert_eq!(err.etype(), &ErrorType::H2Error);
        assert_eq!(malformed_streams, MAX_MALFORMED_STREAMS_PER_CONN);

        drop(connection);
        client.await.unwrap();
    }

    #[tokio::test]
    async fn test_server_handshake_uses_bounded_default_options() {
        let settings = advertised_settings(None).await;

        assert_eq!(
            settings.max_header_list_size(),
            Some(DEFAULT_MAX_HEADER_LIST_SIZE)
        );
        assert_eq!(
            settings.max_concurrent_streams(),
            Some(DEFAULT_MAX_CONCURRENT_STREAMS)
        );
    }

    #[tokio::test]
    async fn test_server_handshake_uses_caller_options() {
        let mut options = H2Options::default();
        options.max_header_list_size(1234);
        options.max_concurrent_streams(42);

        let settings = advertised_settings(Some(options)).await;

        assert_eq!(settings.max_header_list_size(), Some(1234));
        assert_eq!(settings.max_concurrent_streams(), Some(42));
    }

    #[tokio::test]
    async fn test_zero_idle_timeout_waits_for_active_session() {
        let active = Arc::new(ActiveSessions::new());
        let guard = active.start_session();
        let timeout_active = active.clone();
        let timeout = tokio::spawn(async move {
            wait_for_idle_timeout(&timeout_active, Duration::ZERO).await;
        });

        tokio::task::yield_now().await;
        assert!(
            !timeout.is_finished(),
            "zero timeout must not spin or expire while a session is active"
        );

        drop(guard);
        pingora_timeout::timeout(Duration::from_secs(1), timeout)
            .await
            .expect("zero timeout did not expire after the session finished")
            .expect("timeout task panicked");
    }

    #[tokio::test]
    async fn test_server_handshake_rejects_oversized_header_list_by_default() {
        let (client, server) = duplex(256 * 1024);

        let client = tokio::spawn(async move {
            let (h2, connection) = h2::client::handshake(client).await.unwrap();
            tokio::spawn(async move {
                let _ = connection.await;
            });

            let mut request = Request::builder()
                .method(Method::GET)
                .uri("https://www.example.com/")
                .body(())
                .unwrap();
            for _ in 0..2000 {
                request
                    .headers_mut()
                    .append("a", HeaderValue::from_static(""));
            }

            let (response, _) = h2
                .ready()
                .await
                .unwrap()
                .send_request(request, true)
                .unwrap();
            assert_eq!(
                response.await.unwrap().status(),
                http::StatusCode::REQUEST_HEADER_FIELDS_TOO_LARGE
            );
        });

        let server = tokio::spawn(async move {
            let mut connection = handshake(Box::new(server), None).await.unwrap();
            let digest = Arc::new(Digest::default());
            let accepted = timeout(
                Duration::from_secs(1),
                HttpSession::from_h2_conn(&mut connection, digest),
            )
            .await;
            assert!(
                !matches!(accepted, Ok(Ok(Some(_)))),
                "oversized request reached the application"
            );
        });

        client.await.unwrap();
        server.await.unwrap();
    }

    #[cfg(feature = "patched_http1")]
    #[tokio::test]
    async fn test_server_rejects_forbidden_byte_in_request_target() {
        // Control bytes (CR/LF) may be accepted in the request path depending
        // on URI parsing. Ensure such a request target is rejected on ingest as
        // defense-in-depth, since these bytes are not permitted in a URI.
        let (client, server) = duplex(65536);

        let client = tokio::spawn(async move {
            let (h2, connection) = h2::client::handshake(client).await.unwrap();
            tokio::spawn(async move {
                let _ = connection.await;
            });

            let mut h2 = h2.ready().await.unwrap();

            let request = Request::builder()
                .method(Method::GET)
                .uri("https://www.example.com/a\r\nX-Injected: 1")
                .body(())
                .unwrap();
            // Ensure CR/LF survived URI parsing, otherwise the test is a no-op.
            assert!(request.uri().path().contains('\n'));

            let (response, _) = h2.send_request(request, true).unwrap();
            // The stream must be rejected (reset), not answered.
            assert!(response.await.is_err());
        });

        let server = tokio::spawn(async move {
            let mut connection = handshake(Box::new(server), None).await.unwrap();
            let digest = Arc::new(Digest::default());
            let accepted = timeout(
                Duration::from_secs(1),
                HttpSession::from_h2_conn(&mut connection, digest),
            )
            .await
            .expect("from_h2_conn hung: the offending stream was not rejected")
            .expect("from_h2_conn returned an error");
            // The offending stream is reset during acceptance, so `from_h2_conn`
            // yields `Rejected` rather than a session built from the forbidden
            // request target. Sibling streams and the connection are unaffected.
            assert!(
                matches!(accepted, Some(H2Accept::Rejected)),
                "request with forbidden byte in target was not rejected"
            );
        });

        client.await.unwrap();
        server.await.unwrap();
    }

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

        while let Some(accepted) = HttpSession::from_h2_conn(&mut connection, digest.clone())
            .await
            .unwrap()
        {
            let H2Accept::Session(mut http) = accepted else {
                continue;
            };
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
                http.write_body(server_body.into(), false).await.unwrap();
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

    #[tokio::test]
    async fn test_req_conflicting_content_length_rejected() {
        let (client, server) = duplex(65536);

        let client_task = tokio::spawn(async move {
            let (h2, connection) = h2::client::handshake(client).await.unwrap();
            tokio::spawn(async move {
                let _ = connection.await;
            });
            let mut h2 = h2.ready().await.unwrap();

            // Conflicting duplicate Content-Length values: unrecoverable framing.
            let request = Request::builder()
                .method(Method::GET)
                .uri("https://www.example.com/")
                .header("content-length", "5")
                .header("content-length", "6")
                .body(())
                .unwrap();

            let (response, mut req_body) = h2.send_request(request, false).unwrap();
            // Send a body matching the first Content-Length value so the h2
            // codec accepts the stream and Pingora's duplicate-CL validation is
            // what rejects the request.
            req_body.reserve_capacity(5);
            req_body.send_data("abcde".into(), true).unwrap();

            // The server must reject the stream with PROTOCOL_ERROR rather than
            // surface the request to the application.
            let err = response.await.unwrap_err();
            assert_eq!(err.reason(), Some(h2::Reason::PROTOCOL_ERROR));
            // Dropping `h2` lets the connection close so the server loop exits.
        });

        let mut connection = handshake(Box::new(server), None).await.unwrap();
        let digest = Arc::new(Digest::default());

        // The malformed stream is reset during acceptance, so it is reported as
        // rejected rather than surfaced to the application as a session.
        let accepted = HttpSession::from_h2_conn(&mut connection, digest.clone())
            .await
            .unwrap();
        assert!(
            matches!(accepted, Some(H2Accept::Rejected)),
            "malformed request must not surface as a session"
        );

        let done = timeout(
            Duration::from_secs(1),
            HttpSession::from_h2_conn(&mut connection, digest),
        )
        .await
        .expect("from_h2_conn hung after rejecting malformed request")
        .expect("from_h2_conn returned an error after rejecting malformed request");
        assert!(done.is_none(), "connection should close after rejection");

        client_task.await.unwrap();
    }

    #[tokio::test]
    async fn test_req_malformed_stream_budget_exhausted() {
        let (client, server) = duplex(65536);

        let client_task = tokio::spawn(async move {
            let (h2, connection) = h2::client::handshake(client).await.unwrap();
            tokio::spawn(async move {
                let _ = connection.await;
            });
            let mut h2 = h2.ready().await.unwrap();

            let request = Request::builder()
                .method(Method::GET)
                .uri("https://www.example.com/")
                .header("content-length", "5")
                .header("content-length", "6")
                .body(())
                .unwrap();

            let (response, mut req_body) = h2.send_request(request, false).unwrap();
            req_body.reserve_capacity(5);
            req_body.send_data("abcde".into(), true).unwrap();
            let err = response.await.unwrap_err();
            if let Some(reason) = err.reason() {
                assert_eq!(reason, h2::Reason::PROTOCOL_ERROR);
            }
        });

        let mut connection = handshake(Box::new(server), None).await.unwrap();
        let digest = Arc::new(Digest::default());
        let mut malformed_streams = MAX_MALFORMED_STREAMS_PER_CONN - 1;

        let err = match HttpSession::from_h2_conn_with_malformed_budget(
            &mut connection,
            digest,
            &mut malformed_streams,
        )
        .await
        {
            Ok(Some(_)) => panic!("malformed request must not surface as a session"),
            Ok(None) => panic!("connection ended before malformed budget was exhausted"),
            Err(err) => err,
        };

        assert_eq!(err.etype(), &ErrorType::H2Error);
        assert_eq!(malformed_streams, MAX_MALFORMED_STREAMS_PER_CONN);

        drop(connection);
        client_task.await.unwrap();
    }

    #[tokio::test]
    async fn test_req_content_length_eq_0_and_no_header_eos() {
        let (client, server) = duplex(65536);

        let server_body = "test server body";

        let mut handles = vec![];

        handles.push(tokio::spawn(async move {
            let (h2, connection) = h2::client::handshake(client).await.unwrap();
            tokio::spawn(async move {
                connection.await.unwrap();
            });

            let mut h2 = h2.ready().await.unwrap();

            let request = Request::builder()
                .method(Method::POST)
                .uri("https://www.example.com/")
                .header("content-length", "0") // explicitly set
                .body(())
                .unwrap();

            let (response, mut req_body) = h2.send_request(request, false).unwrap(); // no EOS

            let (head, mut body) = response.await.unwrap().into_parts();

            assert_eq!(head.status, 200);
            let data = body.data().await.unwrap().unwrap();
            assert_eq!(data, server_body);

            req_body.send_data("".into(), true).unwrap(); // set EOS after read the resp body
        }));

        let mut connection = handshake(Box::new(server), None).await.unwrap();
        let digest = Arc::new(Digest::default());

        while let Some(accepted) = HttpSession::from_h2_conn(&mut connection, digest.clone())
            .await
            .unwrap()
        {
            let H2Accept::Session(mut http) = accepted else {
                continue;
            };
            handles.push(tokio::spawn(async move {
                let req = http.req_header();
                assert_eq!(req.method, Method::POST);
                assert_eq!(req.uri, "https://www.example.com/");

                // 1. Check body related methods
                http.enable_retry_buffering();
                assert!(http.is_body_empty());
                assert!(http.is_body_done());
                let retry_body = http.get_retry_buffer();
                assert!(retry_body.is_none());

                // 2. Send response
                let response_header = Box::new(ResponseHeader::build(200, None).unwrap());
                assert!(http
                    .write_response_header(response_header.clone(), false)
                    .is_ok());

                http.write_body(server_body.into(), false).await.unwrap();
                assert_eq!(http.body_bytes_sent(), 16);

                // 3. Waiting for the reset from the client
                assert!(http.read_body_or_idle(http.is_body_done()).await.is_err());
            }));
        }

        for handle in handles {
            // ensure no panics
            assert!(handle.await.is_ok());
        }
    }

    #[tokio::test]
    async fn test_req_header_no_eos_empty_data_with_eos() {
        let (client, server) = duplex(65536);

        let server_body = "test server body";

        let mut handles = vec![];

        handles.push(tokio::spawn(async move {
            let (h2, connection) = h2::client::handshake(client).await.unwrap();
            tokio::spawn(async move {
                connection.await.unwrap();
            });

            let mut h2 = h2.ready().await.unwrap();

            let request = Request::builder()
                .method(Method::POST)
                .uri("https://www.example.com/")
                .body(())
                .unwrap();

            let (response, mut req_body) = h2.send_request(request, false).unwrap(); // no EOS

            let (head, mut body) = response.await.unwrap().into_parts();

            assert_eq!(head.status, 200);
            let data = body.data().await.unwrap().unwrap();
            assert_eq!(data, server_body);

            req_body.send_data("".into(), true).unwrap(); // set EOS after read the resp body

            // Drain the response to EOS before dropping the stream. Newer h2
            // sends RST_STREAM(CANCEL) when a still-open recv stream is dropped,
            // which would race with the server reading the request EOS and turn
            // the server-side read into a stream-reset error.
            while let Some(chunk) = body.data().await {
                let chunk = chunk.expect("response body error");
                body.flow_control()
                    .release_capacity(chunk.len())
                    .expect("release capacity");
            }
        }));

        let mut connection = handshake(Box::new(server), None).await.unwrap();
        let digest = Arc::new(Digest::default());

        while let Some(accepted) = HttpSession::from_h2_conn(&mut connection, digest.clone())
            .await
            .unwrap()
        {
            let H2Accept::Session(mut http) = accepted else {
                continue;
            };
            handles.push(tokio::spawn(async move {
                let req = http.req_header();
                assert_eq!(req.method, Method::POST);
                assert_eq!(req.uri, "https://www.example.com/");

                // 1. Check body related methods
                http.enable_retry_buffering();
                assert!(!http.is_body_empty());
                assert!(!http.is_body_done());
                let retry_body = http.get_retry_buffer();
                assert!(retry_body.is_none());

                // 2. Send response
                let response_header = Box::new(ResponseHeader::build(200, None).unwrap());
                assert!(http
                    .write_response_header(response_header.clone(), false)
                    .is_ok());

                http.write_body(server_body.into(), false).await.unwrap();
                assert_eq!(http.body_bytes_sent(), 16);

                // 3. Read the empty DATA frame carrying the request EOS.
                http.read_body_or_idle(http.is_body_done()).await.unwrap();

                // 4. Finish the response so the client can drain it to EOS and
                //    close the stream cleanly instead of cancelling it.
                http.finish().unwrap();
            }));
        }

        for handle in handles {
            // ensure no panics
            assert!(handle.await.is_ok());
        }
    }
}
