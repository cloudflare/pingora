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

//! HTTP/3 client session and connection

use crate::connectors::http::v3::ConnectionRef;
use crate::protocols::http::v3::nohash::StreamIdHashMap;
use crate::protocols::http::v3::{
    data_finished_event, event_to_response_headers, headervec_to_headermap,
    housekeeping_drop_sessions, request_headers_to_event, ConnectionIo,
    H3_SESSION_EVENTS_CHANNEL_SIZE,
};
use crate::protocols::l4::socket::SocketAddr;
use crate::protocols::{Digest, UniqueID, UniqueIDType};
use bytes::Bytes;
use http::HeaderMap;
use log::{debug, trace, warn};
use parking_lot::Mutex;
use pingora_error::ErrorType::{H3Error, InternalError, InvalidHTTPHeader, ReadError, WriteError};
use pingora_error::{Error, ErrorType, OrErr, Result};
use pingora_http::{RequestHeader, ResponseHeader};
use quiche::h3::{Event, Header, NameValue};
use quiche::ConnectionId;
use std::collections::VecDeque;
use std::fmt::Debug;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc::{Receiver, Sender};
use tokio::sync::{mpsc, watch};

/// HTTP/3 client session
///
/// The [`Http3Session`] is built around [`pingora_http`] structs and converts to
/// [`quiche::h3::Event`] where needed.
pub struct Http3Session {
    conn: ConnectionRef,

    // stream id is assigned after the request has been sent
    // quiche internally creates the underlying quic stream during quiche::h3::send_request()
    stream_id: Option<u64>,
    // HTTP3 event channel for this stream_id
    event_rx: Option<Receiver<Event>>,

    /// The read timeout, which will be applied when reading the header, body and trailers.
    /// The timeout is reset on every read attempt. This is not a timeout on the overall duration
    /// of the response.
    pub read_timeout: Option<Duration>,

    // sent request
    request_header_written: Option<Box<RequestHeader>>,
    // received response
    response_header: Option<ResponseHeader>,

    // sent body bytes
    body_sent: usize,
    // sending body is finished (Quic stream FIN flag sent)
    send_ended: bool,

    // body bytes read
    body_read: usize,
    // continue reading without waiting for new event
    read_continue: bool,
    // reading body is finished (Quic stream FIN flag received)
    read_ended: bool,
}

impl Http3Session {
    fn conn_io(&self) -> &ConnectionIo {
        self.conn.conn_io()
    }
}

impl Drop for Http3Session {
    fn drop(&mut self) {
        // TODO: clarify if a RESET_STREAM should be sent
        // drop session in case initialized
        if let Some(stream_id) = self.stream_id {
            self.conn.drop_session(stream_id);
            debug!(
                "connection {:?} dropping session with stream id {}",
                self.conn.conn_id(),
                stream_id
            )
        }
        // always decrease counter
        self.conn.release_stream();
    }
}

impl Http3Session {
    pub(crate) fn new(conn: ConnectionRef) -> Self {
        Self {
            conn,
            stream_id: None,
            event_rx: None,

            read_timeout: None,
            request_header_written: None,
            response_header: None,
            body_sent: 0,
            send_ended: false,
            body_read: 0,
            read_continue: false,
            read_ended: false,
        }
    }

    /// Write the request header to the server
    pub fn write_request_header(&mut self, req: Box<RequestHeader>) -> Result<()> {
        if self.request_header_written.is_some() {
            // cannot send again
            warn!("request not sent as session already sent a request");
            return Ok(());
        }

        let headers = request_headers_to_event(&req)?;
        self.send_request(&headers, false)?;

        self.request_header_written = Some(req);
        Ok(())
    }

    fn send_request<T: NameValue + Debug>(&mut self, headers: &[T], fin: bool) -> Result<u64> {
        // sending the request creates the underlying quic stream & according stream id
        // it is not possible to check the stream capacity before sending the request
        let stream_id = {
            let mut qconn = self.conn_io().quic.lock();
            let mut hconn = self.conn_io().http3.lock();

            hconn
                .send_request(&mut qconn, headers, fin)
                .explain_err(WriteError, |_| "failed to send http3 request headers")?
        };

        let (tx, rx) = mpsc::channel::<Event>(H3_SESSION_EVENTS_CHANNEL_SIZE);
        self.stream_id = Some(stream_id);
        self.event_rx = Some(rx);

        self.conn.add_session(stream_id, tx);
        Ok(stream_id)
    }

    /// Write a request body chunk
    pub async fn write_request_body(&mut self, data: Bytes, end: bool) -> Result<()> {
        if self.send_ended {
            // NOTE: within http3 content-length tracking is not available
            warn!("Cannot write request body after stream ended. Dropping the extra data.");
            return Ok(());
        } else if self.request_header_written.is_none() {
            return Err(Error::explain(
                H3Error,
                "trying to send the request body before request header being sent",
            ));
        };

        let sent_len = self
            .conn_io()
            .send_body(self.stream_id()?, &data, end)
            .await?;

        self.body_sent += sent_len;
        self.send_ended = self.send_ended || end;
        Ok(())
    }

    /// Signal that the request body has ended
    pub fn finish_request_body(&mut self) -> Result<()> {
        if self.send_ended {
            // already ended the stream
            return Ok(());
        }

        if self.request_header_written.is_some() {
            self.conn_io().finish_send(self.stream_id()?)?;
            self.send_ended = true;
        }
        // else: the response header is not sent, do nothing now.

        Ok(())
    }

    /// Read the response header
    pub async fn read_response_header(&mut self) -> Result<()> {
        if self.response_header.is_some() {
            // already received
            return Ok(());
        };

        let read_timeout = self.read_timeout;
        tokio::select! { /* biased, poll data first */
            // to avoid timeout race wins in high load scenarios when data could be available
            biased;
            res = headers_event(self.stream_id()?, self.event_rx()?) => {
                let (headers, _) = res?;
                let map = event_to_response_headers(&headers)?;
                self.response_header = Some(map);
            },
            _timedout = timeout(read_timeout) => {
                return Err(Error::explain(ErrorType::ReadTimedout, "reading response headers timed out"))
            }
        }

        Ok(())
    }

    pub(crate) fn stream_id(&self) -> Result<u64> {
        let Some(stream_id) = self.stream_id else {
            return Err(Error::explain(H3Error, "stream id not present"));
        };
        Ok(stream_id)
    }

    fn event_rx(&mut self) -> Result<&mut Receiver<Event>> {
        let Some(ref mut event_rx) = &mut self.event_rx else {
            return Err(Error::explain(H3Error, "event rx not present"));
        };
        Ok(event_rx)
    }

    /// Read the response body
    ///
    /// `None` means, no more body to read
    pub async fn read_response_body(&mut self) -> Result<Option<Bytes>> {
        if self.read_ended {
            return Ok(None);
        }

        let read_timeout = self.read_timeout;
        tokio::select! { /* biased, poll data first */
            // to avoid timeout race wins in high load scenarios when data could be available
            biased;
            res = async {
                if !self.read_continue {
                    data_finished_event(self.stream_id()?, self.event_rx()?).await
                } else {
                    Ok(false)
                }
            } => {
                let finished = res?;
                if finished {
                    trace!("finished event received");
                    self.read_ended = true;
                    return Ok(None)
                }
            },
            _timedout = timeout(read_timeout) => {
                return Err(Error::explain(ErrorType::ReadTimedout, "reading response body timed out"))
            }
        }

        let (data, continue_read) = self.conn_io().read_body(self.stream_id()?)?;
        self.body_read += data.len();
        self.read_continue = continue_read;

        trace!("read response body len={:?}", data.len());
        Ok(Some(data))
    }

    /// Whether the response has ended
    pub fn response_finished(&self) -> bool {
        self.read_ended
    }

    /// Check whether stream finished with error.
    /// Like `response_finished`, but also attempts to poll the h3 stream for errors that may have
    /// caused the stream to terminate, and returns them as `H3Error`s.
    pub fn check_response_end_or_error(&mut self) -> Result<bool> {
        todo!("within h2 this is used in pingora-proxy")
    }

    /// Read the optional trailer headers
    /// in case pre-conditions are not met, the call returns None
    ///
    /// requires that the request sent contains the TE header including the "trailers" keyword
    /// for further details see RFC9110 Section 6.5.1
    ///
    /// additionally the response headers need to contain the `trailers` header
    pub async fn read_trailers(&mut self) -> Result<Option<HeaderMap>> {
        if !self.read_ended {
            warn!("trying to read trailers before body finished");
            return Ok(None);
        };

        // RFC9110 Section 6.5.1
        // The presence of the keyword "trailers" in the TE header field (Section 10.1.4) of
        // a request indicates that the client is willing to accept trailer fields, on behalf of
        // itself and any downstream clients.
        let mut client_accepts = false;
        if let Some(headers) = &self.request_header_written {
            if let Some(te_header) = headers.headers.get(http::header::TE) {
                let te = te_header
                    .to_str()
                    .explain_err(InvalidHTTPHeader, |_| "failed to parse TE header")?;

                client_accepts = te.contains("trailers")
            }
        };

        let mut response_has_trailers = false;
        if let Some(response) = &self.response_header {
            response_has_trailers = response.headers.get(http::header::TRAILER).is_some()
        };

        if !(client_accepts && response_has_trailers) {
            return Ok(None);
        }

        // as per RFC9114/Section 4.1 it is an optional SINGLE header frame
        // only possible when supported by the version of HTTP in use and enabled by an explicit
        // framing mechanism
        let read_timeout = self.read_timeout;
        let trailer_map = tokio::select! { /* biased, poll data first */
            // to avoid timeout race wins in high load scenarios when data could be available
            biased;
            res = headers_event(self.stream_id()?, self.event_rx()?) => {
                let (trailers, _) = res?;
                headervec_to_headermap(&trailers)?
            },
            _timedout = timeout(read_timeout) => {
                return Err(Error::explain(ErrorType::ReadTimedout, "reading response body timed out"))
            }
        };

        Ok(Some(trailer_map))
    }

    /// The request header if it is already sent
    pub fn request_header(&self) -> Option<&RequestHeader> {
        self.request_header_written.as_deref()
    }

    /// The response header if it is already read
    pub fn response_header(&self) -> Option<&ResponseHeader> {
        self.response_header.as_ref()
    }

    /// Give up the stream abruptly.
    ///
    /// This will send a `STOP_SENDING` and a `RESET_STREAM` for the Quic stream to the client.
    pub fn shutdown(&mut self) {
        let stream_id = match self.stream_id() {
            Ok(id) => id,
            Err(_) => {
                warn!("failed to shutdown session, no stream id present");
                return;
            }
        };
        let conn_io = self.conn_io().clone();
        conn_io.shutdown(stream_id, &mut self.read_ended, &mut self.send_ended);
    }

    /// Return the [`ConnectionRef`] of the Http3Session
    pub(crate) fn conn(&self) -> ConnectionRef {
        self.conn.clone()
    }

    /// Return the [`Digest`] of the connection
    ///
    /// For reused connection, the timing in the digest will reflect its initial handshakes
    /// The caller should check if the connection is reused to avoid misuse the timing field.
    pub fn digest(&self) -> Option<&Digest> {
        Some(self.conn.digest())
    }

    /// Return a mutable [`Digest`] reference for the connection
    ///
    /// Will return `None` if multiple H3 streams are open.
    pub fn digest_mut(&mut self) -> Option<&mut Digest> {
        self.conn.digest_mut()
    }

    /// Return the server (peer) address recorded in the connection digest.
    pub fn server_addr(&self) -> Option<&SocketAddr> {
        self.conn
            .digest()
            .socket_digest
            .as_ref()
            .map(|d| d.peer_addr())?
    }

    /// Return the client (local) address recorded in the connection digest.
    pub fn client_addr(&self) -> Option<&SocketAddr> {
        self.conn
            .digest()
            .socket_digest
            .as_ref()
            .map(|d| d.local_addr())?
    }

    /// the FD of the underlying connection
    pub fn fd(&self) -> UniqueIDType {
        self.conn.id()
    }
}

async fn headers_event(
    stream_id: u64,
    event_rx: &mut Receiver<Event>,
) -> Result<(Vec<Header>, bool)> {
    loop {
        match event_rx.recv().await {
            Some(ev) => {
                trace!("stream {} event {:?}", stream_id, ev);
                match ev {
                    Event::Finished => {
                        debug_assert!(false, "Finished event when Headers requested");
                    }
                    Event::Headers { list, more_frames } => return Ok((list, more_frames)),
                    Event::Data => {
                        debug_assert!(false, "Data event when Headers requested");
                    }
                    Event::Reset(error_code) => {
                        return Err(Error::explain(
                            H3Error,
                            format!("stream was reset with error code {}", error_code),
                        ))
                    }
                    Event::PriorityUpdate => {
                        debug_assert!(false, "PriorityUpdate event when Headers requested");
                        warn!("received unhandled PriorityUpdate event");
                    }
                    Event::GoAway => {
                        debug_assert!(false, "PriorityUpdate event when Headers requested");
                        // RFC 9114 Section 5.2 & 7.2.6
                        warn!("received unhandled GoAway event");
                    }
                }
            }
            None => {
                return Err(Error::explain(
                    ReadError,
                    format!(
                        "H3 session event channel disconnected fn {} stream {}",
                        "headers_event", stream_id
                    ),
                ))
            }
        }
    }
}

pub(crate) struct Http3Poll {
    pub(crate) conn_io: ConnectionIo,
    pub(crate) sessions: StreamIdHashMap<Sender<Event>>,
    pub(crate) drop_sessions: Arc<Mutex<VecDeque<u64>>>,
    pub(crate) add_sessions: Arc<Mutex<VecDeque<(u64, Sender<Event>)>>>,
    pub(crate) idle_close: watch::Sender<bool>,
}

impl Http3Poll {
    pub(crate) async fn start(mut self) -> Result<()> {
        let mut notified = self.conn_io.rx_notify.notified();

        'poll: loop {
            let poll = {
                let mut qconn = self.conn_io.quic.lock();
                let mut hconn = self.conn_io.http3.lock();
                // NOTE: poll() drives the entire Quic/HTTP3 connection
                hconn.poll(&mut qconn)
            };

            let (stream_id, ev) = match poll {
                Ok((stream, ev)) => (stream, ev),
                Err(e) => {
                    housekeeping_drop_sessions(
                        &self.conn_id().clone(),
                        &mut self.sessions,
                        &self.drop_sessions,
                    );

                    let conn_alive = self
                        .conn_io
                        .error_or_timeout_data_race(e, notified, &self.sessions)
                        .await?;
                    match conn_alive {
                        Some(updated_notified) => {
                            notified = updated_notified;
                            continue 'poll;
                        }
                        None => {
                            self.idle_close.send_replace(true);
                            break 'poll Ok(());
                        }
                    }
                }
            };

            let session = if let Some(session) = self.sessions.get_mut(&stream_id) {
                session
            } else {
                let conn_id = self.conn_id().clone();
                housekeeping_add_sessions(&conn_id, &mut self.sessions, &self.add_sessions);
                let Some(session) = self.sessions.get_mut(&stream_id) else {
                    return Err(Error::explain(
                        InternalError,
                        format!(
                            "connection {:?} missing session channel for stream {}",
                            self.conn_io.conn_id(),
                            stream_id
                        ),
                    ));
                };
                session
            };

            session
                .send(ev)
                .await
                .explain_err(H3Error, |_| "failed to forward h3 event to session")?;
        }
    }

    fn conn_id(&self) -> &ConnectionId<'static> {
        &self.conn_io.id
    }
}

fn housekeeping_add_sessions(
    conn_id: &ConnectionId<'_>,
    sessions: &mut StreamIdHashMap<Sender<Event>>,
    add_sessions: &Mutex<VecDeque<(u64, Sender<Event>)>>,
) {
    let mut add_sessions = add_sessions.lock();
    while let Some((stream_id, sender)) = add_sessions.pop_front() {
        match sessions.insert(stream_id, sender) {
            Some(_) => {
                warn!(
                    "connection {:?} stream {} was already present in sessions",
                    conn_id, stream_id
                );
                debug_assert!(false, "session already present")
            }
            None => {
                debug!(
                    "connection {:?} added stream {} to sessions",
                    conn_id, stream_id
                )
            }
        }
    }
}

async fn timeout(timeout: Option<Duration>) {
    if let Some(timeout) = timeout {
        tokio::time::sleep(timeout).await;
    } else {
        tokio::time::sleep(Duration::MAX).await;
    }
}
