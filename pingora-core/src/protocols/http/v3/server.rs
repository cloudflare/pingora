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

use crate::protocols::http::date::get_cached_date;
use crate::protocols::http::v1::client::http_req_header_to_wire;
use crate::protocols::{Digest, SocketAddr, Stream};
use bytes::{BufMut, Bytes, BytesMut};
use http::uri::PathAndQuery;
use http::{header, HeaderMap, HeaderName};
use log::{debug, error, info, trace, warn};
use parking_lot::Mutex;
use pingora_error::{Error, ErrorType, OrErr, Result};
use pingora_http::{RequestHeader, ResponseHeader};
use std::cmp;
use std::collections::VecDeque;
use std::fmt::Debug;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use crate::protocols::http::body_buffer::FixedBuffer;
use crate::protocols::http::v3::nohash::StreamIdHashMap;
use crate::protocols::http::v3::{
    event_to_request_headers, header_size, headermap_to_headervec, response_headers_to_event,
};
use crate::protocols::http::HttpTask;
use crate::protocols::l4::quic::{Connection, MAX_IPV6_QUIC_DATAGRAM_SIZE};
pub use quiche::h3::Config as H3Options;
use quiche::h3::{Connection as QuicheH3Connection, Event, NameValue};
use quiche::{h3, Connection as QuicheConnection, ConnectionId, Shutdown};
use tokio::sync::mpsc::{Receiver, Sender};
use tokio::sync::{mpsc, Notify};

const H3_SESSION_EVENTS_CHANNEL_SIZE: usize = 256;
const H3_SESSION_DROP_DEQUE_INITIAL_CAPACITY: usize = 2048;
const BODY_BUF_LIMIT: usize = 1024 * 64;
const SHUTDOWN_GOAWAY_DRAIN_TIMEOUT: Duration = Duration::from_secs(60);

/// Perform HTTP/3 connection handshake with an established (QUIC) connection.
///
/// The optional `options` allow to adjust certain HTTP/3 parameters and settings.
/// See [`H3Options`] for more details.
pub async fn handshake(mut io: Stream, options: Option<&H3Options>) -> Result<H3Connection> {
    let Some(conn) = io.quic_connection_state() else {
        return Err(Error::explain(
            ErrorType::ConnectError,
            "H3 handshake only possible on Quic connections",
        ));
    };

    let (conn_id, qconn, drop_qconn, hconn, tx_notify, rx_notify) = match conn {
        Connection::Incoming(_) => {
            return Err(Error::explain(
                ErrorType::InternalError,
                "connection needs to be established, invalid state",
            ))
        }
        Connection::Established(state) => {
            let hconn = {
                let http3_config = if let Some(h3_options) = options {
                    h3_options
                } else {
                    &state.http3_config
                };

                let mut qconn = state.connection.lock();
                h3::Connection::with_transport(&mut qconn, http3_config)
                    .explain_err(ErrorType::ConnectError, |_| {
                        "failed to create H3 connection"
                    })?
            };
            state.tx_notify.notify_waiters();

            (
                state.connection_id.clone(),
                state.connection.clone(),
                state.drop_connection.clone(),
                hconn,
                state.tx_notify.clone(),
                state.rx_notify.clone(),
            )
        }
    };

    Ok(H3Connection {
        _l4stream: io,
        connection_id: conn_id,
        drop_quic_connection: drop_qconn,

        quic_connection: qconn,
        h3_connection: Arc::new(Mutex::new(hconn)),

        tx_notify,
        rx_notify,

        sessions: Default::default(),
        drop_sessions: Arc::new(Mutex::new(VecDeque::with_capacity(
            H3_SESSION_DROP_DEQUE_INITIAL_CAPACITY,
        ))),

        max_accepted_stream_id: 0,
        received_goaway: None,
    })
}

pub struct H3Connection {
    _l4stream: Stream, // ensure the stream will not be dropped until all sessions are
    connection_id: ConnectionId<'static>,
    drop_quic_connection: Arc<Mutex<VecDeque<ConnectionId<'static>>>>,

    quic_connection: Arc<Mutex<QuicheConnection>>,
    h3_connection: Arc<Mutex<QuicheH3Connection>>,

    tx_notify: Arc<Notify>,
    rx_notify: Arc<Notify>,

    sessions: StreamIdHashMap<Sender<Event>>,
    drop_sessions: Arc<Mutex<VecDeque<u64>>>,

    max_accepted_stream_id: u64,
    received_goaway: Option<u64>,
}

impl Drop for H3Connection {
    fn drop(&mut self) {
        let mut drop_quic_connection = self.drop_quic_connection.lock();
        drop_quic_connection.push_back(self.connection_id.clone());
        debug!("drop connection {:?}", self.connection_id);
    }
}

impl H3Connection {
    pub async fn graceful_shutdown(&mut self) -> Result<()> {
        // send GOAWAY frame
        {
            let mut qconn = self.quic_connection.lock();
            let mut hconn = self.h3_connection.lock();

            debug!("H3 connection {:?} sending GoAway", self.connection_id);
            hconn
                .send_goaway(&mut qconn, self.max_accepted_stream_id)
                .explain_err(ErrorType::H3Error, |_| "failed to send graceful shutdown")?;
            self.tx_notify.notify_waiters();
        }

        let drain = async {
            while !self.sessions.is_empty() {
                self.rx_notify.notified().await
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
            let mut qconn = self.quic_connection.lock();
            qconn
                .close(false, 0x00, b"graceful shutdown")
                .explain_err(ErrorType::H3Error, |_| "failed to close quic connection")?;
            self.tx_notify.notify_waiters();
        }

        if is_timeout {
            Err(Error::explain(
                ErrorType::InternalError,
                "h3 session draining timed out with active sessions",
            ))
        } else {
            Ok(())
        }
    }

    async fn sessions_housekeeping(&mut self) {
        let mut drop_sessions = self.drop_sessions.lock();

        // housekeeping finished sessions
        while let Some(stream_id) = drop_sessions.pop_front() {
            match self.sessions.remove(&stream_id) {
                None => {
                    warn!(
                        "connection {:?} failed to remove stream {} from sessions",
                        self.connection_id, stream_id
                    )
                }
                Some(_) => {
                    debug!(
                        "connection {:?} stream {} removed from sessions",
                        self.connection_id, stream_id
                    );
                }
            };
        }
    }
}

/// HTTP/3 server session
pub struct HttpSession {
    pub(crate) connection_id: ConnectionId<'static>,
    pub(crate) stream_id: u64,
    quic_connection: Arc<Mutex<QuicheConnection>>,
    h3_connection: Arc<Mutex<QuicheH3Connection>>,

    // notify during drop to remove event_tx from active sessions
    drop_session: Arc<Mutex<VecDeque<u64>>>,

    // trigger Quic send, continue ConnectionTx write loop
    tx_notify: Arc<Notify>,
    // receive notification on Quic recv, used to check stream capacity
    // as it only increases after MaxData or MaxStreamData frame was received
    rx_notify: Arc<Notify>,

    // HTTP3 event channel for this stream_id
    event_rx: Receiver<Event>,

    request_header: RequestHeader,
    // required as separate field for has_body
    request_has_body: bool,
    read_ended: bool,
    body_read: usize,
    // buffered request body for retry logic
    body_retry_buffer: Option<FixedBuffer>,

    // Remember what has been written
    response_header_written: Option<Box<ResponseHeader>>,

    // How many (application, not wire) response body bytes have been sent so far.
    body_sent: usize,

    // track if the FIN STREAM frame was already sent
    // quiche::Connection::stream_send fin argument
    send_ended: bool,

    // digest to record underlying connection info
    digest: Arc<Digest>,
}

impl Drop for HttpSession {
    fn drop(&mut self) {
        let mut drop_sessions = self.drop_session.lock();
        drop_sessions.push_back(self.stream_id);
        debug!(
            "H3 connection {:?} drop stream {}",
            self.connection_id, self.stream_id
        );
    }
}

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
        'poll: loop {
            let poll = {
                let mut qconn = conn.quic_connection.lock();
                let mut hconn = conn.h3_connection.lock();
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
                            conn.connection_id, stream_id, ev
                        );
                        channel
                            .send(ev)
                            .await
                            .explain_err(ErrorType::WriteError, |e| {
                                format!("failed to send on event channel with {}", e)
                            })?;
                    } else {
                        debug!(
                            "H3 connection {:?} stream {} received event {:?}",
                            conn.connection_id, stream_id, &ev
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

                                let mut qconn = conn.quic_connection.lock();
                                let mut hconn = conn.h3_connection.lock();
                                hconn
                                    .send_goaway(&mut qconn, conn.max_accepted_stream_id)
                                    .explain_err(ErrorType::InternalError, |_| {
                                        "failed to send goaway"
                                    })?;
                                conn.tx_notify.notify_waiters();
                            }
                            Event::Headers { list, more_frames } => {
                                trace!(
                                    "H3 connection {:?} request headers={:?}, more_frames={:?}",
                                    conn.connection_id,
                                    &list,
                                    &more_frames
                                );

                                let (event_tx, event_rx) =
                                    mpsc::channel(H3_SESSION_EVENTS_CHANNEL_SIZE);

                                let session = HttpSession {
                                    connection_id: conn.connection_id.clone(),
                                    stream_id,

                                    quic_connection: conn.quic_connection.clone(),
                                    h3_connection: conn.h3_connection.clone(),

                                    drop_session: conn.drop_sessions.clone(),

                                    tx_notify: conn.tx_notify.clone(),
                                    rx_notify: conn.rx_notify.clone(),
                                    event_rx,

                                    request_header: event_to_request_headers(&list)?,
                                    request_has_body: more_frames,
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
                                        conn.connection_id, stream_id
                                    )
                                };

                                conn.max_accepted_stream_id = session.stream_id;
                                return Ok(Some(session));
                            }
                        }
                    }
                }
                Err(h3::Error::Done) => {
                    debug!("H3 connection {:?} no events available", conn.connection_id);
                    // TODO: in case PriorityUpdate was triggered call take_priority_update() here

                    conn.sessions_housekeeping().await;

                    let is_closed;
                    let timeout;
                    {
                        let qconn = conn.quic_connection.lock();
                        is_closed = qconn.is_closed()
                            || !(qconn.is_established() || qconn.is_in_early_data());
                        if is_closed {
                            if let Some(e) = qconn.peer_error() {
                                debug!(
                                    "connection {:?} peer error reason: {}",
                                    conn.connection_id,
                                    String::from_utf8_lossy(e.reason.as_slice()).to_string()
                                );
                            }
                            if let Some(e) = qconn.local_error() {
                                debug!(
                                    "connection {:?} local error reason: {}",
                                    conn.connection_id,
                                    String::from_utf8_lossy(e.reason.as_slice()).to_string()
                                );
                            }
                        }
                        timeout = qconn.timeout();
                    }

                    if is_closed {
                        if !conn.sessions.is_empty() {
                            warn!(
                                "H3 connection {:?} closed with open {} sessions",
                                conn.connection_id,
                                conn.sessions.len()
                            );
                        } else {
                            debug!("H3 connection {:?} closed", conn.connection_id);
                        }

                        conn.tx_notify.notify_waiters();
                        return Ok(None);
                    }

                    // race for new data on connection or timeout
                    tokio::select! {
                        _data = conn.rx_notify.notified() => {}
                        _timedout = async {
                            if let Some(timeout) = timeout {
                                debug!("connection {:?} timeout {:?}", conn.connection_id, timeout);
                                tokio::time::sleep(timeout).await
                            } else {
                                debug!("connection {:?} timeout not present", conn.connection_id);
                                tokio::time::sleep(Duration::MAX).await
                            }
                        } => {
                            conn.sessions_housekeeping().await;
                            if !conn.sessions.is_empty() {
                                warn!("connection {:?} timed out with {} open sessions",
                                    conn.connection_id, conn.sessions.len());
                            }
                            let mut qconn = conn.quic_connection.lock();
                            // closes connection
                            qconn.on_timeout();
                            if let Some(timeout) = timeout {
                                debug!("connection {:?} timed out {:?}", conn.connection_id, timeout);
                            }
                        }
                    }
                }
                Err(e) => {
                    // If an error occurs while processing data, the connection is closed with
                    // the appropriate error code, using the transport’s close() method.

                    // send the close() event
                    conn.tx_notify.notify_waiters();

                    error!("H3 connection closed with error {:?}.", e);
                    return Err(e).explain_err(ErrorType::H3Error, |_| {
                        "while accepting new downstream requests"
                    });
                }
            }
        }
    }

    /// The request sent from the client
    ///
    /// Different from its HTTP/1.X counterpart, this function never panics as the request is already
    /// read when established a new HTTP/3 stream.
    pub fn req_header(&self) -> &RequestHeader {
        &self.request_header
    }

    /// A mutable reference to request sent from the client
    ///
    /// Different from its HTTP/1.X counterpart, this function never panics as the request is already
    /// read when established a new HTTP/3 stream.
    pub fn req_header_mut(&mut self) -> &mut RequestHeader {
        &mut self.request_header
    }

    /// Read request body bytes. `None` when there is no more body to read.
    pub async fn read_body_bytes(&mut self) -> Result<Option<Bytes>> {
        self.data_finished_event().await?;
        if self.read_ended {
            return Ok(None);
        }

        let mut buf = [0u8; MAX_IPV6_QUIC_DATAGRAM_SIZE];
        let size = match self.recv_body(&mut buf) {
            Ok(size) => size,
            Err(h3::Error::Done) => {
                trace!("recv_body done");
                return Ok(Some(BytesMut::with_capacity(0).into()));
            }
            Err(e) => {
                return Err(Error::explain(
                    ErrorType::ReadError,
                    format!("reading body failed with {}", e),
                ))
            }
        };

        let mut data = BytesMut::with_capacity(size);
        data.put_slice(&buf[..size]);
        let data: Bytes = data.into();

        self.body_read += size;
        if let Some(buffer) = &mut self.body_retry_buffer {
            buffer.write_to_buffer(&data);
        }

        trace!("ready body len={:?}", data.len());
        Ok(Some(data))
    }

    fn recv_body(&self, out: &mut [u8]) -> h3::Result<usize> {
        let mut qconn = self.quic_connection.lock();
        let mut hconn = self.h3_connection.lock();
        debug!(
            "H3 connection {:?} stream {} receiving body",
            self.connection_id, self.stream_id
        );
        hconn.recv_body(&mut qconn, self.stream_id, out)
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
            self.tx_notify.notify_waiters();
        }

        self.response_header_written = Some(header);
        self.send_ended = self.send_ended || end;
        Ok(())
    }

    async fn send_response<T: NameValue + Debug>(&self, headers: &[T], fin: bool) -> Result<()> {
        self.stream_capacity(header_size(headers))
            .await
            .explain_err(ErrorType::WriteError, |_| {
                format!(
                    "H3 connection {:?} failed to acquire capacity for stream {}",
                    self.connection_id, self.stream_id
                )
            })?;

        let mut qconn = self.quic_connection.lock();
        let mut hconn = self.h3_connection.lock();

        debug!(
            "H3 connection {:?} stream {} sending response headers={:?}, finished={}",
            self.connection_id, self.stream_id, headers, fin
        );

        match hconn.send_response(&mut qconn, self.stream_id, headers, fin) {
            Ok(()) => Ok(()),
            Err(h3::Error::Done) => Ok(()),
            Err(e) => Err(e).explain_err(ErrorType::WriteError, |_| {
                "H3 connection failed to write response"
            }),
        }
    }

    /// Write response body to the client. See [Self::write_response_header] for how to use `end`.
    pub async fn write_body(&mut self, data: Bytes, end: bool) -> Result<()> {
        if self.send_ended {
            // NOTE: in h1, we also track to see if content-length matches the data
            // We have not tracked that in h3
            warn!("Cannot write body after stream ended. Dropping the extra data.");
            return Ok(());
        } else if self.response_header_written.is_none() {
            return Err(Error::explain(
                ErrorType::H3Error,
                "trying to send the body before header being sent",
            ));
        };

        let mut sent_len = 0;
        let mut fin = end;
        while sent_len < data.len() {
            let required = cmp::min(data.len() - sent_len, MAX_IPV6_QUIC_DATAGRAM_SIZE);
            let capacity =
                self.stream_capacity(required)
                    .await
                    .explain_err(ErrorType::WriteError, |e| {
                        format!(
                            "Failed to acquire capacity on stream id {} with {}",
                            self.stream_id, e
                        )
                    })?;

            let send = if capacity > data.len() - sent_len {
                &data[sent_len..data.len()]
            } else {
                &data[sent_len..sent_len + capacity]
            };

            fin = sent_len + send.len() == data.len() && end;
            match self.send_body(send, fin) {
                Ok(sent_size) => {
                    debug_assert_eq!(sent_size, send.len());
                    sent_len += sent_size;
                }
                Err(e) => {
                    return Err(e).explain_err(ErrorType::WriteError, |_| {
                        "writing h3 response body to downstream"
                    })
                }
            }
        }
        debug_assert_eq!(fin, end);
        debug_assert_eq!(sent_len, data.len());
        if end {
            self.tx_notify.notify_waiters();
        }

        self.body_sent += sent_len;
        self.send_ended = self.send_ended || end;
        Ok(())
    }

    fn send_body(&self, body: &[u8], fin: bool) -> h3::Result<usize> {
        let mut qconn = self.quic_connection.lock();
        let mut hconn = self.h3_connection.lock();

        debug!(
            "H3 connection {:?} stream {} sending response body with length={:?}, finished={}",
            self.connection_id,
            self.stream_id,
            body.len(),
            fin
        );

        hconn.send_body(&mut qconn, self.stream_id, body, fin)
    }

    fn stream_capacity(
        &self,
        required: usize,
    ) -> Pin<Box<dyn Future<Output = quiche::Result<usize>> + Send + '_>> {
        Box::pin(async move {
            let capacity;
            {
                let qconn = self.quic_connection.lock();
                capacity = qconn.stream_capacity(self.stream_id)?;
            }

            if capacity >= required {
                Ok(capacity)
            } else {
                self.tx_notify.notify_waiters();
                self.rx_notify.notified().await;
                self.stream_capacity(required).await
            }
        })
    }

    /// Write response trailers to the client, this also closes the stream.
    pub async fn write_trailers(&mut self, trailers: HeaderMap) -> Result<()> {
        if self.send_ended {
            warn!("Tried to write trailers after end of stream, dropping them");
            return Ok(());
        } else if self.body_sent == 0 {
            return Err(Error::explain(
                ErrorType::H3Error,
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
        self.stream_capacity(header_size(headers))
            .await
            .explain_err(ErrorType::WriteError, |_| {
                format!(
                    "H3 connection {:?} failed to acquire capacity for stream {}",
                    self.connection_id, self.stream_id
                )
            })?;

        let mut qconn = self.quic_connection.lock();
        let mut hconn = self.h3_connection.lock();

        debug!(
            "H3 connection {:?} stream {} sending additional headers={:?}, is_trailer={:?} finished={}",
            self.connection_id,
            self.stream_id,
            headers,
            is_trailer,
            fin
        );

        match hconn.send_additional_headers(&mut qconn, stream_id, headers, is_trailer, fin) {
            Ok(()) => {
                self.tx_notify.notify_waiters();
                Ok(())
            }
            Err(e) => Err(e).explain_err(ErrorType::WriteError, |_| {
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
            // use an empty data frame to signal the end
            self.send_body(&[], true).explain_err(
                ErrorType::WriteError,
                |e| format! {"Writing h3 response body to downstream failed. {e}"},
            )?;
            self.tx_notify.notify_waiters();
            self.send_ended = true;
        }
        // else: the response header is not sent, do nothing now.

        Ok(())
    }

    async fn data_finished_event(&mut self) -> Result<()> {
        loop {
            match self.event_rx.recv().await {
                Some(ev) => {
                    match ev {
                        Event::Finished => {
                            trace!("stream {} event {:?}", self.stream_id, ev);
                            self.read_ended = true;
                            return Ok(());
                        }
                        Event::Headers { .. } => {
                            debug_assert!(false, "Headers or Finished event when Data requested");
                        }
                        Event::Data => {
                            trace!("stream {} event {:?}", self.stream_id, ev);
                            return Ok(());
                        }
                        Event::Reset(error_code) => {
                            return Err(Error::explain(
                                ErrorType::H3Error,
                                format!("stream was reset with error code {}", error_code),
                            ))
                        }
                        Event::PriorityUpdate => {
                            // TODO: this step should be deferred until
                            // h3::Connection::poll() returns Error::Done
                            // see also h3::Connection::send_response_with_priority()

                            /*
                            // https://datatracker.ietf.org/doc/rfc9218/
                            let mut hconn = self.h3_connection.lock();
                            // field value has the same content as the header::Priority field
                            let field_value = hconn.take_last_priority_update(self.stream_id)
                                .explain_err(ErrorType::H3Error, "failed to receive priority update field value")?;
                            */
                            warn!("received unhandled priority update");
                            continue;
                        }
                        Event::GoAway => {
                            // RFC 9114 Section 5.2 & 7.2.6
                            warn!("received unhandled go-away");
                            continue;
                        }
                    }
                }
                None => {
                    return Err(Error::explain(
                        ErrorType::ReadError,
                        "H3 session event channel disconnected",
                    ))
                }
            }
        }
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
                        ErrorType::ReadError,
                        "H3 session event channel disconnected",
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
    /// This will send a `INTERNAL_ERROR` stream error to the client
    pub fn shutdown(&mut self) {
        if !self.read_ended {
            self.stream_shutdown(Shutdown::Read, 2u64);
            // sent STOP_SENDING frame & stream_recv() will no longer return data
            self.read_ended = true;
        }
        if !self.send_ended {
            self.stream_shutdown(Shutdown::Write, 2u64);
            // sent RESET_STREAM & stream_send() data will be ignored
            self.send_ended = true;
        }
    }

    fn stream_shutdown(&self, direction: Shutdown, error_code: u64) {
        let mut qconn = self.quic_connection.lock();
        match qconn.stream_shutdown(self.stream_id, direction, error_code) {
            Ok(()) => self.tx_notify.notify_waiters(),
            Err(e) => warn!("h3 stream {} shutdown failed. {:?}", self.stream_id, e),
        }
    }

    // This is a hack for pingora-proxy to create subrequests from h3 server session
    pub fn pseudo_raw_h1_request_header(&self) -> Bytes {
        let buf = http_req_header_to_wire(self.req_header()).unwrap(); // safe, None only when version unknown
        buf.freeze()
    }

    /// Whether there is no more body to read
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

    /// `async fn idle() -> Result<Reason, Error>;`
    /// This async fn will be pending forever until the client closes the stream/connection
    /// This function is used for watching client status so that the server is able to cancel
    /// its internal tasks as the client waiting for the tasks goes away
    pub async fn idle(&mut self) -> Result<()> {
        match self.reset_event().await {
            Ok(_error_code) => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Similar to `read_body_bytes()` but will be pending after Ok(None) is returned,
    /// until the client closes the connection
    pub async fn read_body_or_idle(&mut self, no_body_expected: bool) -> Result<Option<Bytes>> {
        if no_body_expected || self.is_body_done() {
            let reason = self.reset_event().await?;
            Error::e_explain(
                ErrorType::H3Error,
                format!("Client closed H3, reason: {reason}"),
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
