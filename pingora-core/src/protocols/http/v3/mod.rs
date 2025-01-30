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

//! HTTP/3 implementation

use crate::protocols::http::v3::nohash::StreamIdHashMap;
use crate::protocols::l4::quic::{
    connector, handle_connection_errors, listener, MAX_IPV6_QUIC_DATAGRAM_SIZE,
};
use bytes::{BufMut, Bytes, BytesMut};
use http::uri::{Authority, Scheme};
use http::{HeaderMap, HeaderName, HeaderValue, Request, Uri, Version};
use log::{debug, error, trace, warn};
use parking_lot::Mutex;
use pingora_error::ErrorType::{H3Error, InvalidHTTPHeader, ReadError, WriteError};
use pingora_error::{Error, ErrorType, OrErr, Result};
use pingora_http::{RequestHeader, ResponseHeader};
use quiche::h3::{Event, Header, NameValue};
use quiche::{h3, ConnectionId, Shutdown};
use std::cmp;
use std::collections::VecDeque;
use std::fmt::Debug;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc::{Receiver, Sender};
use tokio::sync::Notify;

const H3_SESSION_EVENTS_CHANNEL_SIZE: usize = 256;
const H3_SESSION_DROP_DEQUE_INITIAL_CAPACITY: usize = 2048;

const MAX_PER_INVOCATION_READ_BODY_BYTES: usize = MAX_IPV6_QUIC_DATAGRAM_SIZE * 64;

pub mod client;
pub(crate) mod nohash;
pub mod server;

/// ConnectionIo useable for HTTP 3 interactions
/// unifies actions that are used in server & client
#[derive(Clone)]
pub(crate) struct ConnectionIo {
    /// the QUIC/HTTP 3 connection id
    id: ConnectionId<'static>,

    /// the underlying Quic connection
    quic: Arc<Mutex<quiche::Connection>>,
    /// the actual HTTP 3 connection
    http3: Arc<Mutex<quiche::h3::Connection>>,

    /// receive notification on Quic recv
    ///
    /// e.g. used to continue the Http3Poll loop and to check stream capacity
    /// as it only increases after MaxData or MaxStreamData frame was received
    rx_notify: Arc<Notify>,
    /// trigger Quic send, continues [`crate::protocols::l4::quic::ConnectionTx`] write loop
    tx_notify: Arc<Notify>,
}

impl From<(&connector::EstablishedState, h3::Connection)> for ConnectionIo {
    fn from((state, h3conn): (&connector::EstablishedState, h3::Connection)) -> Self {
        Self {
            id: state.connection_id.clone(),
            quic: state.connection.clone(),
            http3: Arc::new(Mutex::new(h3conn)),
            rx_notify: state.rx_notify.clone(),
            tx_notify: state.tx_notify.clone(),
        }
    }
}

impl From<(&listener::EstablishedState, h3::Connection)> for ConnectionIo {
    fn from((state, h3conn): (&listener::EstablishedState, h3::Connection)) -> Self {
        Self {
            id: state.connection_id.clone(),
            quic: state.connection.clone(),
            http3: Arc::new(Mutex::new(h3conn)),
            rx_notify: state.rx_notify.clone(),
            tx_notify: state.tx_notify.clone(),
        }
    }
}

impl ConnectionIo {
    pub(crate) fn conn_id(&self) -> &ConnectionId<'static> {
        &self.id
    }

    pub(crate) fn is_shutting_down(&self) -> bool {
        let qconn = self.quic.lock();
        qconn.is_draining()
    }

    pub(crate) fn more_streams_available(&self) -> bool {
        let qconn = self.quic.lock();
        qconn.is_established()
            && !qconn.is_closed()
            && !qconn.is_draining()
            && qconn.peer_streams_left_bidi() > 0
    }

    fn capacity(
        &self,
        stream_id: u64,
        required: usize,
    ) -> Pin<Box<dyn Future<Output = Result<usize>> + Send + '_>> {
        Box::pin(async move {
            let capacity;
            {
                let qconn = self.quic.lock();
                let conn_id = qconn.trace_id();
                capacity = qconn
                    .stream_capacity(stream_id)
                    .explain_err(WriteError, |e| {
                        format!(
                            "H3 connection {} failed to acquire capacity for stream {} error {:?}",
                            conn_id, stream_id, e
                        )
                    })?;
            }

            if capacity >= required {
                Ok(capacity)
            } else {
                self.tx_notify.notify_waiters();
                self.rx_notify.notified().await;
                self.capacity(stream_id, required).await
            }
        })
    }

    async fn send_body(&self, stream_id: u64, data: &[u8], end: bool) -> Result<usize> {
        let mut sent_len = 0;
        let mut fin = end;
        while sent_len < data.len() {
            let required = cmp::min(data.len() - sent_len, MAX_IPV6_QUIC_DATAGRAM_SIZE);
            let capacity = self.capacity(stream_id, required).await?;

            let send = if capacity > data.len() - sent_len {
                &data[sent_len..data.len()]
            } else {
                &data[sent_len..sent_len + capacity]
            };

            fin = sent_len + send.len() == data.len() && end;
            match self.send_body_conn(stream_id, send, fin) {
                Ok(sent_size) => {
                    sent_len += sent_size;
                    // following capacity check will send in case stream is full
                }
                Err(e) => {
                    return Err(e)
                        .explain_err(WriteError, |_| "writing h3 request body to downstream")
                }
            }
        }
        debug_assert_eq!(fin, end);
        debug_assert_eq!(sent_len, data.len());

        if end {
            trace!(
                "connection {:?} sent FIN flag for stream {} body",
                self.conn_id(),
                stream_id
            );
            self.tx_notify.notify_waiters();
        }

        Ok(sent_len)
    }

    fn send_body_conn(&self, stream_id: u64, body: &[u8], fin: bool) -> Result<usize> {
        let mut qconn = self.quic.lock();
        let mut hconn = self.http3.lock();

        hconn
            .send_body(&mut qconn, stream_id, body, fin)
            .explain_err(WriteError, |e| {
                format!("failed to send http3 request body {:?}", e)
            })
    }

    fn finish_send(&self, stream_id: u64) -> Result<()> {
        // use an empty data frame to signal the end
        self.send_body_conn(stream_id, &[], true).explain_err(
            WriteError,
            |e| format! {"Writing h3 request body finished to downstream failed. {e}"},
        )?;
        self.tx_notify.notify_waiters();
        trace!(
            "connection {:?} sent FIN flag for stream {} finish",
            self.conn_id(),
            stream_id
        );
        Ok(())
    }

    fn read_body(&self, stream_id: u64) -> Result<(Bytes, bool)> {
        let mut buf = [0u8; MAX_IPV6_QUIC_DATAGRAM_SIZE];
        let mut data = BytesMut::new();

        let continue_read = loop {
            match self.read_body_conn(stream_id, &mut buf) {
                Ok(read) => {
                    data.put_slice(&buf[..read]);
                    // limit in memory buffer growth
                    if data.len() + buf.len() > MAX_PER_INVOCATION_READ_BODY_BYTES {
                        // required to decide if subsequent calls should wait for new poll events
                        break true;
                    }
                }
                Err(h3::Error::Done) => {
                    // poll for next Http3 event
                    // Event::Finished is only emitted after recv_body is Done
                    self.rx_notify.notify_waiters();
                    trace!(
                        "connection {:?} reading body for stream {} done",
                        self.conn_id(),
                        stream_id
                    );
                    break false;
                }
                Err(e) => {
                    return Err(Error::explain(
                        ReadError,
                        format!("reading body failed with {}", e),
                    ))
                }
            };
        };

        Ok((data.into(), continue_read))
    }

    fn read_body_conn(&self, stream_id: u64, out: &mut [u8]) -> quiche::h3::Result<usize> {
        let mut qconn = self.quic.lock();
        let mut hconn = self.http3.lock();

        debug!(
            "H3 connection {:?} stream {} receiving body",
            self.id, stream_id
        );
        hconn.recv_body(&mut qconn, stream_id, out)
    }

    fn shutdown(&self, stream_id: u64, read_ended: &mut bool, write_ended: &mut bool) {
        let mut qconn = self.quic.lock();
        if !*read_ended {
            // sent STOP_SENDING frame & stream_recv() will no longer return data
            match qconn.stream_shutdown(stream_id, Shutdown::Read, 2u64) {
                Ok(()) => {}
                Err(e) => warn!("h3 stream {} shutdown failed. {:?}", stream_id, e),
            }
            *read_ended = true;
        }
        if !*write_ended {
            // sent RESET_STREAM & stream_send() data will be ignored
            match qconn.stream_shutdown(stream_id, Shutdown::Write, 2u64) {
                Ok(()) => {}
                Err(e) => warn!("h3 stream {} shutdown failed. {:?}", stream_id, e),
            }
            *write_ended = true;
        }

        self.tx_notify.notify_waiters()
    }

    async fn error_or_timeout_data_race<D, A>(
        &self,
        error: h3::Error,
        sessions: &mut StreamIdHashMap<Sender<Event>>,
        mut drop_sessions: D,
        mut add_sessions: A,
    ) -> Result<bool>
    where
        D: FnMut(&mut StreamIdHashMap<Sender<Event>>),
        A: FnMut(&mut StreamIdHashMap<Sender<Event>>),
    {
        // register before housekeeping to avoid notify misses in high-load scenarios
        let data_future = self.rx_notify.notified();

        match error {
            h3::Error::Done => {
                debug!("H3 connection {:?} no events available", self.conn_id());
                // TODO: in case PriorityUpdate was triggered call take_priority_update() here

                add_sessions(sessions);
                drop_sessions(sessions);

                let timeout;
                {
                    let qconn = self.quic.lock();
                    let is_closed =
                        qconn.is_closed() || !(qconn.is_established() || qconn.is_in_early_data());
                    if is_closed {
                        if !sessions.is_empty() {
                            warn!(
                                "H3 connection {:?} closed with open {} sessions",
                                self.conn_id(),
                                sessions.len()
                            );
                        } else {
                            debug!("H3 connection {:?} closed", self.conn_id());
                        }

                        // send close in case it is a local error
                        self.tx_notify.notify_waiters();

                        return match handle_connection_errors(
                            self.conn_id(),
                            qconn.local_error(),
                            qconn.peer_error(),
                        ) {
                            Ok(()) => Ok(false), // signal connection close
                            Err(e) => Err(e),
                        };
                    }
                    timeout = qconn.timeout();
                }

                // race for new data on connection or timeout
                tokio::select! { /* biased, poll data first */
                    // to avoid timeout race wins in high load scenarios when data could be available
                    biased;
                    _data = data_future => { /* continue */ }
                    _timedout = async {
                        if let Some(timeout) = timeout {
                            debug!("connection {:?} timeout {:?}", self.conn_id(), timeout);
                            tokio::time::sleep(timeout).await
                        } else {
                            debug!("connection {:?} timeout not present", self.conn_id());
                            tokio::time::sleep(Duration::MAX).await
                        }
                    } => {
                        drop_sessions(sessions);
                        if !sessions.is_empty() {
                            warn!("connection {:?} timed out with {} open sessions",
                                self.conn_id(), sessions.len());
                        }
                        let mut qconn = self.quic.lock();
                        // closes connection
                        qconn.on_timeout();
                        if let Some(timeout) = timeout {
                            debug!("connection {:?} timed out {:?}", self.conn_id(), timeout);
                        }
                    }
                }
                Ok(true) // signal continue
            }
            _ => {
                // If an error occurs while processing data, the connection is closed with
                // the appropriate error code, using the transport’s close() method.

                // send the close() event
                self.tx_notify.notify_waiters();

                error!(
                    "H3 connection {:?} closed with error {:?}.",
                    self.conn_id(),
                    error
                );
                Err(error).explain_err(H3Error, |_| "failed to poll H3 connection for new events")
            }
        }
    }
}

fn housekeeping_drop_sessions(
    conn_id: &ConnectionId<'_>,
    sessions: &mut StreamIdHashMap<Sender<Event>>,
    drop_sessions: &Mutex<VecDeque<u64>>,
) {
    let mut drop_sessions = drop_sessions.lock();

    // housekeeping finished sessions
    while let Some(stream_id) = drop_sessions.pop_front() {
        match sessions.remove(&stream_id) {
            None => {
                warn!(
                    "connection {:?} failed to remove stream {} from sessions",
                    conn_id, stream_id
                )
            }
            Some(_) => {
                debug!(
                    "connection {:?} stream {} removed from sessions",
                    conn_id, stream_id
                );
            }
        };
    }
}

fn event_to_request_headers(list: &Vec<Header>) -> Result<RequestHeader> {
    let (mut parts, _) = Request::new(()).into_parts();
    let mut uri = Uri::builder();
    let mut headers = HeaderMap::new();

    for h in list {
        match h.name() {
            b":scheme" => uri = uri.scheme(h.value()),
            b":authority" => uri = uri.authority(h.value()),
            b":path" => uri = uri.path_and_query(h.value()),
            b":method" => match h.value().try_into() {
                Ok(v) => parts.method = v,
                Err(_) => warn!("Failed to parse method from input: {:?}", h.value()),
            },
            _ => match HeaderName::from_bytes(h.name()) {
                Ok(k) => match HeaderValue::from_bytes(h.value()) {
                    Ok(v) => {
                        headers.append(k, v);
                    }
                    Err(_) => warn!("Failed to parse header value from input: {:?}", h.value()),
                },
                Err(_) => warn!("Failed to parse header name input: {:?}", h.name()),
            },
        }
    }

    parts.version = Version::HTTP_3;
    parts.uri = uri.build().explain_err(ErrorType::H3Error, |_| {
        "failed to convert event parts to request uri"
    })?;
    parts.headers = headers;
    Ok(parts.into())
}

fn response_headers_to_event(resp: &ResponseHeader) -> Vec<Header> {
    let mut qheaders: Vec<Header> = Vec::with_capacity(resp.headers.len() + 1);
    qheaders.push(Header::new(b":status", resp.status.as_str().as_bytes()));

    for (k, v) in &resp.headers {
        qheaders.push(Header::new(k.as_str().as_bytes(), v.as_bytes()))
    }
    qheaders
}

fn request_headers_to_event(req: &RequestHeader) -> Result<Vec<Header>> {
    let mut qheaders: Vec<Header> = Vec::with_capacity(req.headers.len() + 4);
    // only encrypted traffic supported in HTTP3
    qheaders.push(Header::new(
        b":scheme".as_slice(),
        Scheme::HTTPS.to_string().as_bytes(),
    ));

    // use authority when present
    let authority = if let Some(authority) = req.uri.authority() {
        authority.clone()
    } else {
        // or use host header as authority
        let host = req.headers.get(http::header::HOST);
        let Some(host) = host else {
            return Error::e_explain(InvalidHTTPHeader, "no authority header for h3");
        };
        // validate
        Authority::try_from(host.as_bytes()).explain_err(InvalidHTTPHeader, |_| {
            format!("invalid authority from host {:?}", host)
        })?
    };
    qheaders.push(Header::new(
        b":authority".as_slice(),
        authority.as_str().as_bytes(),
    ));

    let Some(path) = req.uri.path_and_query() else {
        return Error::e_explain(InvalidHTTPHeader, "no path header for h3");
    };
    qheaders.push(Header::new(b":path".as_slice(), path.as_str().as_bytes()));
    qheaders.push(Header::new(
        b":method".as_slice(),
        req.method.as_str().as_bytes(),
    ));

    // copy all other request headers
    // the pseudo-headers starting with ":" need to be sent before regular headers
    for (k, v) in &req.headers {
        qheaders.push(Header::new(k.as_str().as_bytes(), v.as_bytes()))
    }
    Ok(qheaders)
}

fn event_to_response_headers(resp: &[Header]) -> Result<ResponseHeader> {
    // pseudo-headers have to be first, response only has a single valid pseudo header ":status"
    // which MUST be included as per RFC9114 Section 4.3.2
    let mut response = ResponseHeader::build(resp[0].value(), Some(resp.len() - 1))?;
    response.set_version(Version::HTTP_3);

    for h in &resp[1..] {
        let k = HeaderName::from_bytes(h.name()).explain_err(InvalidHTTPHeader, |_| {
            format!("failed to parse header name {:?}", h.name())
        })?;
        let v = HeaderValue::from_bytes(h.value()).explain_err(InvalidHTTPHeader, |_| {
            format!("failed to parse header value {:?}", h.value())
        })?;
        response.append_header(k, v)?;
    }

    Ok(response)
}

fn headermap_to_headervec(headers: &HeaderMap) -> Vec<Header> {
    headers
        .iter()
        .map(|(k, v)| Header::new(k.as_str().as_bytes(), v.as_bytes()))
        .collect()
}

fn headervec_to_headermap(headers: &Vec<Header>) -> Result<HeaderMap> {
    let mut map = HeaderMap::with_capacity(headers.len());
    for h in headers {
        if !h.name().is_empty() && h.name()[0] == b":".as_slice()[0] {
            let k = HeaderName::from_bytes(h.name()).explain_err(InvalidHTTPHeader, |_| {
                format!("failed to parse header name {:?}", h.name())
            })?;
            let v = HeaderValue::from_bytes(h.value()).explain_err(InvalidHTTPHeader, |_| {
                format!("failed to parse header value {:?}", h.value())
            })?;
            map.insert(k, v);
        }
    }
    Ok(map)
}

fn header_size<T: NameValue + Debug>(headers: &[T]) -> usize {
    headers
        .iter()
        .fold(0, |acc, h| acc + h.value().len() + h.name().len() + 32)
}

async fn data_finished_event(stream_id: u64, event_rx: &mut Receiver<Event>) -> Result<bool> {
    loop {
        match event_rx.recv().await {
            Some(ev) => {
                match ev {
                    Event::Finished => {
                        trace!("stream {} event {:?}", stream_id, ev);
                        return Ok(true);
                    }
                    Event::Headers { .. } => {
                        debug_assert!(false, "Headers or Finished event when Data requested");
                    }
                    Event::Data => {
                        trace!("stream {} event {:?}", stream_id, ev);
                        return Ok(false);
                    }
                    Event::Reset(error_code) => {
                        return Err(Error::explain(
                            H3Error,
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
                            .explain_err(H3Error, "failed to receive priority update field value")?;
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
                    ReadError,
                    format!(
                        "H3 session event channel disconnected fn {} stream {}",
                        "data_finished_event", stream_id
                    ),
                ))
            }
        }
    }
}
