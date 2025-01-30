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

use crate::protocols::http::body_buffer::FixedBuffer;
use crate::protocols::http::date::get_cached_date;
use crate::protocols::http::v1::client::http_req_header_to_wire;
use crate::protocols::http::v3::nohash::StreamIdHashMap;
use crate::protocols::http::v3::{
    data_finished_event, event_to_request_headers, header_size, headermap_to_headervec,
    housekeeping_drop_sessions, response_headers_to_event, ConnectionIo,
    H3_SESSION_DROP_DEQUE_INITIAL_CAPACITY, H3_SESSION_EVENTS_CHANNEL_SIZE,
};
use crate::protocols::http::HttpTask;
use crate::protocols::l4::quic::Connection;
use crate::protocols::{Digest, SocketAddr, Stream};
use bytes::Bytes;
use http::uri::PathAndQuery;
use http::{header, HeaderMap, HeaderName};
use log::{debug, error, info, trace, warn};
use parking_lot::Mutex;
use pingora_error::ErrorType::{ConnectError, H3Error, InternalError, ReadError, WriteError};
use pingora_error::{Error, OrErr, Result};
use pingora_http::{RequestHeader, ResponseHeader};
pub use quiche::h3::Config as Http3Options;
use quiche::h3::{self, Connection as QuicheH3Connection, Event, NameValue};
use quiche::ConnectionId;
use std::collections::VecDeque;
use std::fmt::Debug;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc::{self, Receiver, Sender};

const BODY_BUF_LIMIT: usize = 1024 * 64;
const SHUTDOWN_GOAWAY_DRAIN_TIMEOUT: Duration = Duration::from_secs(60);

/// Perform HTTP/3 connection handshake with an established Quic connection.
///
/// The optional `options` allow to adjust certain HTTP/3 parameters and settings.
/// See [`Http3Options`] for more details.
pub(crate) async fn handshake(
    mut io: Stream,
    options: Option<&Http3Options>,
) -> Result<Http3Connection> {
    let Some(conn) = io.quic_connection_state() else {
        return Err(Error::explain(
            ConnectError,
            "H3 handshake only possible on Quic connections",
        ));
    };

    let (conn_io, drop_connections) = match conn {
        Connection::IncomingEstablished(e_state) => {
            let hconn = {
                let http3_config = if let Some(h3_options) = options {
                    h3_options
                } else {
                    &e_state.http3_config
                };

                let mut qconn = e_state.connection.lock();
                QuicheH3Connection::with_transport(&mut qconn, http3_config)
                    .explain_err(ConnectError, |_| "failed to create H3 connection")?
            };
            e_state.tx_notify.notify_waiters();

            (
                ConnectionIo::from((&*e_state, hconn)),
                e_state.drop_connection.clone(),
            )
        }
        _ => {
            return Err(Error::explain(
                InternalError,
                "connection needs to be established, invalid state",
            ))
        }
    };

    debug!("connection {:?} http3 handshake finished", conn_io.id);
    Ok(Http3Connection {
        _l4stream: io,

        conn_io,
        drop_connections,

        sessions: Default::default(),
        drop_sessions: Arc::new(Mutex::new(VecDeque::with_capacity(
            H3_SESSION_DROP_DEQUE_INITIAL_CAPACITY,
        ))),

        max_accepted_stream_id: 0,
        received_goaway: None,
    })
}

/// corresponds to a [`quiche::h3::Connection`] and links [`Http3Session`]s to Quic IO
pub(crate) struct Http3Connection {
    _l4stream: Stream, // ensure the stream will not be dropped until connection is closed

    conn_io: ConnectionIo,
    drop_connections: Arc<Mutex<VecDeque<ConnectionId<'static>>>>,

    sessions: StreamIdHashMap<Sender<Event>>,
    drop_sessions: Arc<Mutex<VecDeque<u64>>>,

    max_accepted_stream_id: u64,
    received_goaway: Option<u64>,
}

impl Drop for Http3Connection {
    fn drop(&mut self) {
        let mut drop_connections = self.drop_connections.lock();
        drop_connections.push_back(self.conn_id().clone());
        debug!("drop connection {:?}", self.conn_id());
    }
}

impl Http3Connection {
    fn conn_id(&self) -> &ConnectionId<'static> {
        &self.conn_io.id
    }

    pub async fn graceful_shutdown(&mut self) -> Result<()> {
        // send GOAWAY frame
        {
            let mut qconn = self.conn_io.quic.lock();
            let mut hconn = self.conn_io.http3.lock();

            debug!("H3 connection {:?} sending GoAway", self.conn_id());
            hconn
                .send_goaway(&mut qconn, self.max_accepted_stream_id)
                .explain_err(H3Error, |_| "failed to send graceful shutdown")?;
            self.conn_io.tx_notify.notify_waiters();
        }

        let drain = async {
            while !self.sessions.is_empty() {
                self.conn_io.rx_notify.notified().await
            }
        };

        // wait for open sessions to drain
        let mut is_timeout = false;
        tokio::select! {
            _successful_drain = drain => { debug!("h3 successfully drained active sessions") }
            _timeout = tokio::time::sleep(SHUTDOWN_GOAWAY_DRAIN_TIMEOUT) => { is_timeout = true }
        }

        // close quic connection
        {
            let mut qconn = self.conn_io.quic.lock();
            qconn
                .close(false, 0x00, b"graceful shutdown")
                .explain_err(H3Error, |_| "failed to close quic connection")?;
            self.conn_io.tx_notify.notify_waiters();
        }

        if is_timeout {
            Err(Error::explain(
                InternalError,
                "h3 session draining timed out with active sessions",
            ))
        } else {
            Ok(())
        }
    }
}

/// HTTP/3 server session
///
/// [`Http3Session`]s contain the converted [`quiche::h3::Event::Headers`] as
/// [`pingora_http::RequestHeader`]. The [`Http3Session`] is built around [`pingora_http`] structs
/// and converts to [`quiche::h3::Event`] where needed.
pub struct Http3Session {
    pub(crate) stream_id: u64,
    conn_io: ConnectionIo,

    // notify during drop to remove event_tx from active sessions
    drop_session: Arc<Mutex<VecDeque<u64>>>,

    // HTTP3 event channel for this stream_id
    event_rx: Receiver<Event>,

    request_header: RequestHeader,
    // required as separate field for has_body
    request_has_body: bool,
    // continue reading without waiting for new event
    read_continue: bool,
    // reading body is finished (Quic stream FIN flag received)
    read_ended: bool,
    body_read: usize,
    // buffered request body for retry logic
    body_retry_buffer: Option<FixedBuffer>,

    // Remember what has been written
    response_header_written: Option<Box<ResponseHeader>>,

    // How many (application, not wire) response body bytes have been sent so far.
    body_sent: usize,

    // sending body is finished (Quic stream FIN flag sent)
    send_ended: bool,

    // digest to record underlying connection info
    digest: Arc<Digest>,
}

impl Drop for Http3Session {
    fn drop(&mut self) {
        let mut drop_sessions = self.drop_session.lock();
        drop_sessions.push_back(self.stream_id);
        debug!(
            "H3 connection {:?} drop stream {}",
            self.conn_id(),
            self.stream_id
        );
    }
}

impl Http3Session {
    /// Create a new [`Http3Session`] from the QUIC connection.
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
    pub(crate) async fn from_h3_conn(
        conn: &mut Http3Connection,
        digest: Arc<Digest>,
    ) -> Result<Option<Self>> {
        'poll: loop {
            let poll = {
                let mut qconn = conn.conn_io.quic.lock();
                let mut hconn = conn.conn_io.http3.lock();
                // NOTE: poll() drives the entire Quic/HTTP3 connection
                hconn.poll(&mut qconn)
            };

            match poll {
                Ok((stream_id, ev)) => {
                    if let Some(goaway_id) = conn.received_goaway {
                        // do not accept new streams, continue processing existing streams
                        if stream_id >= goaway_id {
                            continue 'poll;
                        }
                    }

                    if let Some(channel) = conn.sessions.get(&stream_id) {
                        debug!(
                            "H3 connection {:?} stream {} forward event={:?}",
                            conn.conn_id(),
                            stream_id,
                            ev
                        );
                        channel.send(ev).await.explain_err(WriteError, |e| {
                            format!("failed to send on event channel with {}", e)
                        })?;
                    } else {
                        debug!(
                            "H3 connection {:?} stream {} received event {:?}",
                            conn.conn_id(),
                            stream_id,
                            &ev
                        );
                        match ev {
                            Event::Data
                            | Event::Finished
                            | Event::Reset(_)
                            | Event::PriorityUpdate => {
                                debug_assert!(false, "event type requires corresponding session")
                            }
                            Event::GoAway => {
                                info!("stream_id {} received GoAway", stream_id);
                                conn.received_goaway = Some(stream_id);

                                let mut qconn = conn.conn_io.quic.lock();
                                let mut hconn = conn.conn_io.http3.lock();
                                hconn
                                    .send_goaway(&mut qconn, conn.max_accepted_stream_id)
                                    .explain_err(InternalError, |_| "failed to send goaway")?;
                                conn.conn_io.tx_notify.notify_waiters();
                            }
                            Event::Headers { list, more_frames } => {
                                trace!(
                                    "H3 connection {:?} request headers={:?}, more_frames={:?}",
                                    conn.conn_id(),
                                    &list,
                                    &more_frames
                                );

                                let (event_tx, event_rx) =
                                    mpsc::channel(H3_SESSION_EVENTS_CHANNEL_SIZE);

                                let session = Http3Session {
                                    stream_id,
                                    conn_io: conn.conn_io.clone(),

                                    drop_session: conn.drop_sessions.clone(),

                                    event_rx,

                                    request_header: event_to_request_headers(&list)?,
                                    request_has_body: more_frames,
                                    read_continue: false,
                                    read_ended: !more_frames,
                                    body_read: 0,
                                    body_retry_buffer: None,

                                    response_header_written: None,
                                    body_sent: 0,
                                    send_ended: false,

                                    digest,
                                };

                                if conn.sessions.insert(stream_id, event_tx).is_some() {
                                    debug_assert!(
                                        false,
                                        "H3 connection {:?} stream {} existing \
                                    session is not allowed",
                                        conn.conn_id(),
                                        stream_id
                                    )
                                };

                                if session.stream_id > conn.max_accepted_stream_id {
                                    conn.max_accepted_stream_id = session.stream_id;
                                }
                                return Ok(Some(session));
                            }
                        }
                    }
                }
                Err(e) => {
                    let conn_id = conn.conn_id().clone();
                    let drop_sessions = &conn.drop_sessions.clone();

                    let fn_drop_sessions = |sessions: &mut StreamIdHashMap<Sender<Event>>| {
                        housekeeping_drop_sessions(&conn_id, sessions, drop_sessions)
                    };

                    conn.conn_io
                        .error_or_timeout_data_race(e, &mut conn.sessions, fn_drop_sessions, |_| {})
                        .await?;
                    continue 'poll;
                }
            }
        }
    }

    /// The request sent from the client
    pub fn req_header(&self) -> &RequestHeader {
        &self.request_header
    }

    /// A mutable reference to request sent from the client
    pub fn req_header_mut(&mut self) -> &mut RequestHeader {
        &mut self.request_header
    }

    /// Read request body bytes. `None` when there is no more body to read.
    pub async fn read_body_bytes(&mut self) -> Result<Option<Bytes>> {
        if self.read_ended {
            return Ok(None);
        }

        if !self.read_continue {
            let finished = data_finished_event(self.stream_id, &mut self.event_rx).await?;
            if finished {
                trace!("finished event received");
                self.read_ended = true;
                return Ok(None);
            }
        }

        let (data, continue_read) = self.conn_io.read_body(self.stream_id)?;
        self.body_read += data.len();
        self.read_continue = continue_read;

        if let Some(buffer) = &mut self.body_retry_buffer {
            buffer.write_to_buffer(&data);
        }

        trace!("read request body len={:?}", data.len());
        Ok(Some(data))
    }

    // the write_* don't have timeouts because the actual writing happens on the connection
    // not here.

    /// Write the response header to the client.
    /// the `end` flag marks the end of this session.
    /// If the `end` flag is set, no more header or body can be sent to the client.
    pub async fn write_response_header(
        &mut self,
        mut header: Box<ResponseHeader>,
        end: bool,
    ) -> Result<()> {
        if self.send_ended {
            warn!("H3 session already ended");
            return Ok(());
        } else if self.response_header_written.as_ref().is_some() {
            warn!("response header is already sent, cannot send again");
            return Ok(());
        }

        if header.status.is_informational() {
            // ignore informational response 1xx header
            // send_response() can only be called once in case end = true
            // https://github.com/hyperium/h2/issues/167
            debug!("ignoring informational headers");
            return Ok(());
        }

        /* update headers */
        header.insert_header(header::DATE, get_cached_date())?;

        // remove other h1 hop headers that cannot be present in H3
        // https://httpwg.org/specs/rfc7540.html#n-connection-specific-header-fields
        header.remove_header(&header::TRANSFER_ENCODING);
        header.remove_header(&header::CONNECTION);
        header.remove_header(&header::UPGRADE);
        header.remove_header(&HeaderName::from_static("keep-alive"));
        header.remove_header(&HeaderName::from_static("proxy-connection"));

        let headers = response_headers_to_event(&header);
        self.send_response(headers.as_slice(), end).await?;
        if end {
            self.conn_io.tx_notify.notify_waiters();
        }

        self.response_header_written = Some(header);
        self.send_ended = self.send_ended || end;
        Ok(())
    }

    async fn send_response<T: NameValue + Debug>(&self, headers: &[T], fin: bool) -> Result<()> {
        self.conn_io
            .capacity(self.stream_id, header_size(headers))
            .await?;

        let mut qconn = self.conn_io.quic.lock();
        let mut hconn = self.conn_io.http3.lock();

        debug!(
            "H3 connection {:?} stream {} sending response headers={:?}, finished={}",
            self.conn_id(),
            self.stream_id,
            headers,
            fin
        );

        match hconn.send_response(&mut qconn, self.stream_id, headers, fin) {
            Ok(()) => Ok(()),
            Err(h3::Error::Done) => Ok(()),
            Err(e) => Err(e).explain_err(WriteError, |_| "H3 connection failed to write response"),
        }
    }

    /// Write response body to the client. See [Self::write_response_header] for how to use `end`.
    pub async fn write_body(&mut self, data: Bytes, end: bool) -> Result<()> {
        if self.send_ended {
            // NOTE: within http3 content-length tracking is not available
            warn!("Cannot write body after stream ended. Dropping the extra data.");
            return Ok(());
        } else if self.response_header_written.is_none() {
            return Err(Error::explain(
                H3Error,
                "trying to send the body before header being sent",
            ));
        };

        let sent_len = self.conn_io.send_body(self.stream_id, &data, end).await?;

        self.body_sent += sent_len;
        self.send_ended = self.send_ended || end;
        Ok(())
    }

    /// Write response trailers to the client, this also closes the stream.
    pub async fn write_trailers(&mut self, trailers: HeaderMap) -> Result<()> {
        if self.send_ended {
            warn!("Tried to write trailers after end of stream, dropping them");
            return Ok(());
        } else if self.body_sent == 0 {
            return Err(Error::explain(
                H3Error,
                "Trying to send trailers before body is sent.",
            ));
        };

        let headers = headermap_to_headervec(&trailers);
        self.send_additional_headers(self.stream_id, headers.as_slice(), true, true)
            .await?;

        // sending trailers closes the stream
        self.send_ended = true;
        Ok(())
    }

    // NOTE: as of quiche/v0.22.0 only available in quiche.git/master
    async fn send_additional_headers<T: NameValue + Debug>(
        &self,
        stream_id: u64,
        headers: &[T],
        is_trailer: bool,
        fin: bool,
    ) -> Result<()> {
        self.conn_io
            .capacity(self.stream_id, header_size(headers))
            .await?;

        let mut qconn = self.conn_io.quic.lock();
        let mut hconn = self.conn_io.http3.lock();

        debug!(
            "H3 connection {:?} stream {} sending additional headers={:?}, is_trailer={:?} finished={}",
            self.conn_id(), self.stream_id, headers, is_trailer, fin
        );

        match hconn.send_additional_headers(&mut qconn, stream_id, headers, is_trailer, fin) {
            Ok(()) => {
                self.conn_io.tx_notify.notify_waiters();
                Ok(())
            }
            Err(e) => Err(e).explain_err(WriteError, |_| {
                "H3 connection failed to write h3 trailers to downstream"
            }),
        }
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
        if self.send_ended {
            // already ended the stream
            return Ok(());
        }

        if self.response_header_written.is_some() {
            self.conn_io.finish_send(self.stream_id)?;
            self.send_ended = true;
        }
        // else: the response header is not sent, do nothing now.

        Ok(())
    }

    async fn reset_event(&mut self) -> Result<u64> {
        loop {
            match self.event_rx.recv().await {
                Some(ev) => {
                    error!("reset stream {} event {:?}", self.stream_id, ev);
                    match ev {
                        Event::Data | Event::Finished | Event::GoAway | Event::PriorityUpdate => {
                            continue
                        }
                        Event::Headers { .. } => continue,
                        Event::Reset(error_code) => return Ok(error_code),
                    }
                }
                None => {
                    return Err(Error::explain(
                        ReadError,
                        format!(
                            "H3 session event channel disconnected fn {} stream {}",
                            "reset_event", self.stream_id
                        ),
                    ))
                }
            }
        }
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
                    self.write_trailers(*trailers).await?;
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
        self.response_header_written.as_deref()
    }

    /// Give up the stream abruptly.
    ///
    /// This will send a `STOP_SENDING` and a `RESET_STREAM` for the Quic stream to the client.
    pub fn shutdown(&mut self) {
        self.conn_io
            .shutdown(self.stream_id, &mut self.read_ended, &mut self.send_ended);
    }

    // This is a hack for pingora-proxy to create subrequests from h3 server session
    pub fn pseudo_raw_h1_request_header(&self) -> Bytes {
        let buf = http_req_header_to_wire(self.req_header()).unwrap(); // safe, None only when version unknown
        buf.freeze()
    }

    /// Whether there is no more body to read.
    pub fn is_body_done(&self) -> bool {
        self.is_body_empty() || self.read_ended
    }

    /// Whether there is any body to read.
    pub fn is_body_empty(&self) -> bool {
        self.request_has_body
            || self
                .request_header
                .headers
                .get(header::CONTENT_LENGTH)
                .map_or(false, |cl| cl.as_bytes() == b"0")
    }

    pub fn retry_buffer_truncated(&self) -> bool {
        self.body_retry_buffer
            .as_ref()
            .map_or_else(|| false, |r| r.is_truncated())
    }

    pub fn enable_retry_buffering(&mut self) {
        if self.body_retry_buffer.is_none() {
            self.body_retry_buffer = Some(FixedBuffer::new(BODY_BUF_LIMIT))
        }
    }

    pub fn get_retry_buffer(&self) -> Option<Bytes> {
        self.body_retry_buffer.as_ref().and_then(|b| {
            if b.is_truncated() {
                None
            } else {
                b.get_buffer()
            }
        })
    }

    /// This async fn will be pending forever until the client closes the stream/connection
    /// This function is used for watching client status so that the server is able to cancel
    /// its internal tasks as the client waiting for the tasks goes away
    pub async fn idle(&mut self) -> Result<()> {
        match self.reset_event().await {
            Ok(_error_code) => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Similar to `read_body_bytes()` but will be pending after `Ok(None)` is returned,
    /// until the client closes the connection
    pub async fn read_body_or_idle(&mut self, no_body_expected: bool) -> Result<Option<Bytes>> {
        if no_body_expected || self.is_body_done() {
            let reason = self.reset_event().await?;
            Error::e_explain(H3Error, format!("Client closed H3, reason: {reason}"))
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

    fn conn_id(&self) -> &ConnectionId<'_> {
        &self.conn_io.id
    }
}
