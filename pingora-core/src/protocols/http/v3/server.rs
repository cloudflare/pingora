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

use std::cmp;
use std::fmt::Debug;
use crate::protocols::{Digest, SocketAddr, Stream};
use bytes::{BufMut, Bytes, BytesMut};
use http::uri::PathAndQuery;
use http::{header, HeaderMap, HeaderName};
use pingora_error::{Error, ErrorType, OrErr, Result};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, OnceLock};
use std::task::{Context, Poll};
use log::{debug, error, info, trace, warn};
use parking_lot::Mutex;
use crate::protocols::http::v1::client::http_req_header_to_wire;
use pingora_http::{RequestHeader, ResponseHeader};
use crate::protocols::http::date::get_cached_date;

use crate::protocols::http::HttpTask;
pub use quiche::h3::Config as H3Options;
use crate::protocols::l4::quic::{Connection, MAX_IPV6_QUIC_DATAGRAM_SIZE};
use quiche::{h3, Connection as QuicheConnection};
use quiche::h3::{Connection as QuicheH3Connection, Event, NameValue};
use tokio::sync::{mpsc, Notify};
use tokio::sync::mpsc::{Receiver, Sender};
use tokio::sync::mpsc::error::TrySendError;
use crate::protocols::http::body_buffer::FixedBuffer;
use crate::protocols::http::v3::{event_to_request_headers, response_headers_to_event};
use crate::protocols::http::v3::nohash::StreamIdHashMap;

static H3_OPTIONS: OnceLock<H3Options> = OnceLock::new();

const H3_SESSION_EVENTS_CHANNEL_SIZE : usize = 256;
const H3_SESSION_DROP_CHANNEL_SIZE : usize = 1024;

/// Perform HTTP/3 connection handshake with an established (QUIC) connection.
///
/// The optional `options` allow to adjust certain HTTP/3 parameters and settings.
/// See [`H3Options`] for more details.
pub async fn handshake(mut io: Stream, options: Option<&H3Options>) -> Result<H3Connection> {
    let options = options.unwrap_or(H3_OPTIONS.get_or_init(|| H3Options::new().unwrap()));

    let Some(conn) = io.quic_connection_state() else {
        return Err(Error::explain(
            ErrorType::ConnectError, "H3 handshake only possible on Quic connections"));
    };

    let (conn_id, qconn, hconn,
        tx_notify, rx_notify) = match conn {
        Connection::Incoming(_) => {
            return Err(Error::explain(
                ErrorType::InternalError,
                "connection needs to be established, invalid state"))
        }
        Connection::Established(state) => {
            let conn_id;
            let hconn = {
                let mut qconn = state.connection.lock();
                conn_id = qconn.trace_id().to_string();
                h3::Connection::with_transport(&mut qconn, &options).explain_err(
                   ErrorType::ConnectError, |_| "failed to create H3 connection")?
            };
            (conn_id, state.connection.clone(), hconn, state.tx_notify.clone(),  state.rx_notify.clone())
        }
    };

    let drop_sessions = mpsc::channel(H3_SESSION_DROP_CHANNEL_SIZE);
    Ok(H3Connection {
        _l4stream: io,
        id: conn_id.to_string(),
        quic_connection: qconn,
        h3_connection: Arc::new(Mutex::new(hconn)),
        tx_notify,
        rx_notify,
        sessions: Default::default(),
        drop_sessions,
    })
}

pub struct H3Connection {
    _l4stream: Stream, // ensure the stream will not be dropped until all sessions are
    id: String,
    quic_connection: Arc<Mutex<QuicheConnection>>,
    h3_connection: Arc<Mutex<QuicheH3Connection>>,

    tx_notify: Arc<Notify>,
    rx_notify: Arc<Notify>,

    sessions: StreamIdHashMap<Sender<Event>>,
    drop_sessions: (Sender<u64>, Receiver<u64>)
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
pub struct HttpSession {
    connection_id: String,
    stream_id: u64,
    quic_connection: Arc<Mutex<QuicheConnection>>,
    h3_connection: Arc<Mutex<QuicheH3Connection>>,

    // notify during drop to remove event_tx from active sessions
    drop_session: Sender<u64>,

    // trigger Quic send, continue ConnectionTx write loop
    tx_notify: Arc<Notify>,
    // receive notification on Quic recv, used to check stream capacity
    // as it only increases after MaxData or MaxStreamData frame was received
    rx_notify: Arc<Notify>,

    // HTTP3 event channel for this stream_id
    event_rx: Receiver<Event>,

    request_header: Option<RequestHeader>,
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
        match self.drop_session.try_send(self.stream_id) {
            Ok(()) => debug!("drop stream {}", self.stream_id),
            Err(e) => {
                let id = match e {
                    TrySendError::Full(id) => id,
                    TrySendError::Closed(id) => id
                };
                warn!("stream {} failed notify drop session", id)
            }
        }
    }
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
        'poll: loop {
            let poll = {
                let mut qconn = conn.quic_connection.lock();
                let mut hconn = conn.h3_connection.lock();
                // NOTE: poll() drives the entire Quic/HTTP3 connection
                hconn.poll(&mut qconn)
            };

            match poll {
                Ok((stream_id, ev)) => {
                    if let Some(channel) = conn.sessions.get(&stream_id) {
                        debug!(
                            "H3 connection {} stream {} forward event={:?}",
                            conn.id, stream_id, ev
                        );
                        channel.send(ev).await
                            .explain_err(
                                ErrorType::WriteError,
                                |e| format!("failed to send on event channel with {}", e))?;
                    } else {
                        debug!(
                            "H3 connection {} stream {} received event {:?}",
                            conn.id, stream_id, &ev
                        );
                        match ev {
                            Event::Data
                            | Event::Finished
                            | Event::Reset(_)
                            | Event::PriorityUpdate => {
                                debug_assert!(false, "event type requires corresponding session")
                            }
                            Event::GoAway => {
                                info!("Received GoAway, dropping connection.");
                                return Ok(None)
                            },
                            Event::Headers { list, more_frames: stream_continues } => {
                                trace!(
                                    "H3 connection {} request headers={:?}, more_frames={:?}",
                                    conn.id,
                                    &list,
                                    &stream_continues
                                );

                                let (event_tx, event_rx) = mpsc::channel(H3_SESSION_EVENTS_CHANNEL_SIZE);
                                let session = HttpSession {
                                    connection_id: conn.id.clone(),
                                    stream_id,

                                    quic_connection: conn.quic_connection.clone(),
                                    h3_connection: conn.h3_connection.clone(),

                                    drop_session: conn.drop_sessions.0.clone(),

                                    tx_notify: conn.tx_notify.clone(),
                                    rx_notify: conn.rx_notify.clone(),
                                    event_rx,

                                    read_ended: !stream_continues,
                                    request_header: Some(event_to_request_headers(&list)),
                                    body_read: 0,
                                    body_retry_buffer: None,

                                    response_header_written: None,
                                    body_sent: 0,
                                    send_ended: false,

                                    digest
                                };

                                if let Some(_) = conn.sessions.insert(stream_id, event_tx) {
                                    debug_assert!(false, "H3 connection {} stream {} existing session is not allowed", conn.id, stream_id)
                                };
                                return Ok(Some(session));
                            }
                        }
                    }
                }
                Err(h3::Error::Done) => {
                    debug!("H3 connection {} no events available", conn.id);
                    // TODO: in case PriorityUpdate was triggered take_priority_update should be called here
                    let timeout;
                    {
                        let mut qconn = conn.quic_connection.lock();
                        if qconn.is_closed() ||
                            !(qconn.is_established() || qconn.is_in_early_data()) {
                            warn!("open sessions: {:?}", conn.sessions.keys());
                            return Ok(None)
                        }
                        timeout = qconn.timeout();
                    }

                    debug!("Quic connection {:?} is still active. Timeout: {:?}", conn.id, timeout);
                    if let Some(timeout) = timeout {
                        // race for new data on connection or timeout
                        tokio::select! {
                            _timeout = tokio::time::sleep(timeout) => {
                                let mut qconn = conn.quic_connection.lock();
                                qconn.on_timeout();
                            }
                            _data = conn.rx_notify.notified() => {}
                        }
                    };

                    debug!("H3 connection {} waiting for data", conn.id);
                    continue 'poll;
                }
                Err(err) => {
                    info!("Received error, dropping connection. {:?}", err);
                    return Err(err).explain_err(ErrorType::H3Error, |e| {
                        format!("While accepting new downstream requests. Error: {e}")
                    })
                }
            }

            while !conn.drop_sessions.1.is_empty() {
                if let Some(stream_id) = conn.drop_sessions.1.recv().await {
                    conn.sessions.remove(&stream_id);
                }
            }
        }
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
        self.data_finished_event().await?;
        if self.read_ended {
            return Ok(None)
        }

        let mut buf = [0u8; MAX_IPV6_QUIC_DATAGRAM_SIZE];
        let size = match self.recv_body(&mut buf) {
            Ok(size) => size,
            Err(h3::Error::Done) => {
                error!("recv_body: Done");
                return Ok(Some(BytesMut::with_capacity(0).into()))
            },
            Err(e) => return Err(Error::explain(
                ErrorType::ReadError, format!("reading body failed with {}", e)))
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


    fn recv_body(&self, out: &mut [u8]) -> h3::Result<(usize)> {
        let mut qconn = self.quic_connection.lock();
        let mut hconn = self.h3_connection.lock();
        debug!(
            "H3 connection {} stream {} receiving body",
            qconn.trace_id(),
            self.stream_id
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
            // TODO: error or warn?
            warn!("H3 session already ended");
            return Ok(());
        } else if self.response_header_written.as_ref().is_some() {
            warn!("response header is already sent, cannot send again");
            return Ok(());
        }

        /* TODO: check if should that be as well handled like that?
        if header.status.is_informational() {
            // ignore informational response 1xx header because send_response() can only be called once
            // https://github.com/hyperium/h2/issues/167
            debug!("ignoring informational headers");
            return Ok(());
        } */

        /* update headers */
        header.insert_header(header::DATE, get_cached_date())?;

        // TODO: check if this is correct for H3
        // remove other h1 hop headers that cannot be present in H3
        // https://httpwg.org/specs/rfc7540.html#n-connection-specific-header-fields
        header.remove_header(&header::TRANSFER_ENCODING);
        header.remove_header(&header::CONNECTION);
        header.remove_header(&header::UPGRADE);
        header.remove_header(&HeaderName::from_static("keep-alive"));
        header.remove_header(&HeaderName::from_static("proxy-connection"));

        let headers = response_headers_to_event(&header);
        let sent = self.send_response(headers.as_slice(), end).await;

        self.response_header_written = Some(header);
        self.send_ended = self.send_ended || end;
        Ok(())
    }

    async fn send_response<T: NameValue + Debug>(
        &self,
        headers: &[T],
        fin: bool,
    ) -> Result<()> {
        let headers_len = headers
            .iter()
            .fold(0, |acc, h| acc + h.value().len() + h.name().len() + 32);

        let capacity = self.stream_capacity(headers_len).await
            .explain_err(
                ErrorType::WriteError,
                |_| format!("H3 connection {} failed to acquire capacity for stream {}",
                    self.connection_id, self.stream_id))?;

        let mut qconn = self.quic_connection.lock();
        let mut hconn = self.h3_connection.lock();

        debug!(
            "H3 connection {} stream {} sending response headers={:?}, finished={}",
            qconn.trace_id(),
            self.stream_id,
            headers,
            fin
        );

        match hconn.send_response(&mut qconn, self.stream_id, headers, fin) {
            Ok(()) => {
                self.tx_notify.notify_one();
                Ok(())
            }
            Err(h3::Error::Done) => { Ok(()) },
            Err(e) => Err(e).explain_err(
                    ErrorType::WriteError,
                    |_| "H3 connection failed to write response"),
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
        while sent_len < data.len() {
            let required = cmp::min(data.len(), MAX_IPV6_QUIC_DATAGRAM_SIZE);
            let capacity = self.stream_capacity(required).await
                .explain_err(
                    ErrorType::WriteError,
                    |e| format!("Failed to acquire capacity on stream id {} with {}", self.stream_id, e))?;

            let send;
            if capacity > data.len() {
                send = &data[sent_len..data.len()];
            } else {
                send = &data[sent_len..capacity];
            }

            match self.send_body(send, end).await {
                Ok(sent_size) => {
                    sent_len += sent_size;
                    self.tx_notify.notify_one();
                },
                Err(e) => return Err(e).explain_err(
                        ErrorType::WriteError, |_| "writing h3 response body to downstream")
            }
        }

        self.body_sent += sent_len;
        self.send_ended = self.send_ended || end;
        Ok(())
    }

    async fn send_body(&self, body: &[u8], fin: bool) -> h3::Result<usize> {
        let mut qconn = self.quic_connection.lock();
        let mut hconn = self.h3_connection.lock();

        debug!("H3 connection {} stream {} sending response body with length={:?}, finished={}",
            self.connection_id, self.stream_id, body.len(), fin);

        hconn.send_body(&mut qconn, self.stream_id, body, fin)
    }

    async fn stream_capacity(&self, required: usize) -> quiche::Result<usize> {
        let capacity;
        {
            let qconn = self.quic_connection.lock();
            capacity = qconn.stream_capacity(self.stream_id)?;
        }

        if capacity >= required {
            Ok(capacity)
        } else {
            self.rx_notify.notified().await;
            Box::pin(self.stream_capacity(required)).await
        }
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
        if self.send_ended {
            // already ended the stream
            return Ok(());
        }

        // use an empty data frame to signal the end
        self.send_body(&[], true)
            .await
            .explain_err(
                ErrorType::WriteError,
                |e| format! {"Writing h3 response body to downstream failed. {e}"},
            )?;

        self.send_ended = true;
        // else: the response header is not sent, do nothing now.
        // When send_response_body is dropped, an RST_STREAM will be sent

        Ok(())
    }

    async fn data_finished_event(&mut self) -> Result<()> {
        loop {
            match self.event_rx.recv().await {
                Some(ev) => {
                    trace!("event {:?}", ev);
                    match ev {
                        Event::Finished => {
                            self.read_ended = true;
                            return Ok(())
                        }
                        Event::Headers { .. } => {
                            debug_assert!(false, "Headers or Finished event when Data requested");
                        },
                        Event::Data => {
                            return Ok(())
                        }
                        // TODO: handle events correctly
                        Event::Reset(_) |
                        Event::PriorityUpdate |
                        Event::GoAway => {
                            continue
                        },
                    }
                }
                None => return Err(Error::explain(
                    ErrorType::ReadError,
                    "H3 session event channel disconnected")),
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
        self.response_header_written.as_deref()
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