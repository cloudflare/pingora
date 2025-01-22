use std::cmp;
use std::collections::VecDeque;
use std::fmt::Debug;
use std::sync::Arc;
use crate::connectors::http::v3::{ConnectionIo, ConnectionRef};
use crate::protocols::l4::socket::SocketAddr;
use crate::protocols::{Digest, UniqueIDType};
use bytes::{BufMut, Bytes, BytesMut};
use h2::SendStream;
use http::HeaderMap;
use pingora_http::{RequestHeader, ResponseHeader};
use std::time::Duration;
use log::{debug, error, trace, warn};
use parking_lot::Mutex;
use quiche::{h3, ConnectionId};
use quiche::h3::{Event, Header, NameValue};
use tokio::sync::mpsc;
use tokio::sync::mpsc::{Receiver, Sender};
use pingora_error::{Error, ErrorType, OrErr, Result};
use pingora_error::ErrorType::{H3Error, InternalError, InvalidHTTPHeader, ReadError, WriteError};
use crate::protocols::http::v3::{data_finished_event, event_to_response_headers, header_size, headervec_to_headermap, request_headers_to_event, stream_capacity, H3_SESSION_EVENTS_CHANNEL_SIZE};
use crate::protocols::http::v3::nohash::StreamIdHashMap;
use crate::protocols::l4::quic::MAX_IPV6_QUIC_DATAGRAM_SIZE;

pub struct Http3Session {
    conn_io: ConnectionIo,
    stream_id: Option<u64>,

    /// The read timeout, which will be applied to both reading the header and the body.
    /// The timeout is reset on every read. This is not a timeout on the overall duration of the
    /// response.
    // FIXME: race with timeout if present
    pub read_timeout: Option<Duration>,

    // HTTP3 event channel for this stream_id
    event_rx: Option<Receiver<Event>>,

    // sent request
    request_header_written: Option<Box<RequestHeader>>,
    // received response
    response_header: Option<ResponseHeader>,

    // sent body bytes
    body_sent: usize,
    // send is finished (Quic finished frame sent)
    send_ended: bool,

    // body bytes read
    body_read: usize,
    // read is finished (Quic finished frame received)
    read_ended: bool,

    // remove session from active sessions
    drop_sessions: Arc<Mutex<VecDeque<u64>>>,
    // add session to active sessions
    add_sessions: Arc<Mutex<VecDeque<(u64, Sender<Event>)>>>
}

impl Drop for Http3Session {
    fn drop(&mut self) {
        if let Some(stream_id) = self.stream_id {
            {
                let mut sessions = self.drop_sessions.lock();
                sessions.push_back(stream_id);
            }
            debug!("connection {:?} dropping session with stream id {}",
            self.conn_io.conn_id, stream_id)
        }
    }
}

impl Http3Session {
    pub(crate) fn new(conn_io: ConnectionIo,
                      add_sessions: Arc<Mutex<VecDeque<(u64, Sender<Event>)>>>,
                      drop_sessions: Arc<Mutex<VecDeque<u64>>>)
        -> Result<Self> {
        Ok(Self {
            conn_io,
            stream_id: None,
            read_timeout: None,
            event_rx: None,
            request_header_written: None,
            response_header: None,
            body_sent: 0,
            send_ended: false,
            body_read: 0,
            read_ended: false,
            add_sessions,
            drop_sessions,
        })
    }

    /// Write the request header to the server
    pub async fn write_request_header(
        &mut self,
        req: Box<RequestHeader>
    ) -> Result<()> {
        if self.request_header_written.is_some() {
            // cannot send again
            warn!("request not sent as session already sent a request");
            return Ok(());
        }

        let headers = request_headers_to_event(&req)?;
        let stream_id = self.send_request(&headers, false).await?;
        error!("stream_id {}", stream_id);

        self.request_header_written = Some(req);
        Ok(())
    }

    async fn send_request<T: NameValue + Debug>(&mut self, headers: &[T], fin: bool) -> Result<u64> {
        // sending the request creates the underlying quic stream & according stream id
        // it is not possible to check the stream capacity before sending the request
        let stream_id = {
            let mut qconn = self.conn_io.quic.lock();
            let mut hconn = self.conn_io.http3.lock();

            hconn.send_request(&mut qconn, headers, fin)
                .explain_err(WriteError, |_| "failed to send http3 request headers")?
        };

        let (tx, rx) = mpsc::channel::<Event>(H3_SESSION_EVENTS_CHANNEL_SIZE);
        self.stream_id = Some(stream_id);
        self.event_rx = Some(rx);

        {
            let mut add_sessions = self.add_sessions.lock();
            add_sessions.push_back((stream_id, tx))
        }

        Ok(stream_id)
    }

    // TODO: potentially refactor/unify with server side

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
        let Some(stream_id) = self.stream_id else {
            return Err(Error::explain(H3Error, "stream id not present"));
        };

        let mut sent_len = 0;
        let mut fin = end;
        while sent_len < data.len() {
            let required = cmp::min(data.len() - sent_len, MAX_IPV6_QUIC_DATAGRAM_SIZE);
            let capacity = stream_capacity(&self.conn_io.quic, stream_id, required,
                                           &self.conn_io.rx_notify, &self.conn_io.tx_notify).await?;

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
                    return Err(e).explain_err(WriteError, |_| {
                        "writing h3 request body to downstream"
                    })
                }
            }
        }
        debug_assert_eq!(fin, end);
        debug_assert_eq!(sent_len, data.len());
        if end {
            self.conn_io.tx_notify.notify_waiters();
        }

        self.body_sent += sent_len;
        self.send_ended = self.send_ended || end;
        Ok(())
    }


    // TODO: potentially refactor/unify with server side
    fn send_body(&self, body: &[u8], fin: bool) -> Result<usize> {
        let mut qconn = self.conn_io.quic.lock();
        let mut hconn = self.conn_io.http3.lock();

        hconn.send_body(&mut qconn, self.stream_id()?, body, fin)
            .explain_err(WriteError, |e| format!("failed to send http3 request body {:?}", e))
    }

    // TODO: potentially refactor/unify with server side
    /// Signal that the request body has ended
    pub fn finish_request_body(&mut self) -> Result<()> {
        if self.send_ended {
            // already ended the stream
            return Ok(());
        }

        if self.request_header_written.is_some() {
            // use an empty data frame to signal the end
            self.send_body(&[], true).explain_err(
                WriteError,
                |e| format! {"Writing h3 request body finished to downstream failed. {e}"},
            )?;
            self.conn_io.tx_notify.notify_waiters();
            self.send_ended = true;
        }
        // else: the response header is not sent, do nothing now.

        Ok(())
    }

    /// Read the response header
    pub async fn read_response_header(&mut self) -> Result<()> {
        if self.response_header.is_some() {
            // already received
            return Ok(())
        };

        let (headers, _) = headers_event(self.stream_id()?, self.event_rx()?).await?;
        let map = event_to_response_headers(&headers)?;

        self.response_header = Some(map);
        Ok(())
    }

    fn stream_id(&self) -> Result<u64> {
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

    // TODO: potentially refactor/unify with server side
    /// Read the response body
    ///
    /// `None` means, no more body to read
    pub async fn read_response_body(&mut self) -> Result<Option<Bytes>> {
        if self.read_ended {
            return Ok(None);
        }


        let read_timeout = self.read_timeout.clone();
        tokio::select! {
            res = data_finished_event(self.stream_id()?, self.event_rx()?) => {
                self.read_ended = true;
                res?
            },
            _timedout = async {
                if let Some(read_timeout) = read_timeout {
                    tokio::time::sleep(read_timeout)
                } else {
                    tokio::time::sleep(Duration::MAX)
                }
            } => {
                return Err(Error::explain(ErrorType::ReadTimedout, "reading body timed out"))
            }
        }

        let mut buf = [0u8; MAX_IPV6_QUIC_DATAGRAM_SIZE];
        let size = match self.recv_body(self.stream_id()?, &mut buf) {
            Ok(size) => size,
            Err(h3::Error::Done) => {
                trace!("recv_body done");
                return Ok(Some(BytesMut::with_capacity(0).into()));
            }
            Err(e) => {
                return Err(Error::explain(
                    ReadError,
                    format!("reading body failed with {}", e),
                ))
            }
        };

        let mut data = BytesMut::with_capacity(size);
        data.put_slice(&buf[..size]);
        let data: Bytes = data.into();

        self.body_read += size;

        trace!("ready body len={:?}", data.len());
        Ok(Some(data))
    }

    // TODO: potentially refactor/unify with server side
    // TODO: check if result type can be changed (requires Error::Done not being used)
    fn recv_body(&self, stream_id: u64, out: &mut [u8]) -> h3::Result<usize> {
        let mut qconn = self.conn_io.quic.lock();
        let mut hconn = self.conn_io.http3.lock();
        debug!(
            "H3 connection {:?} stream {} receiving body",
            qconn.trace_id(), stream_id
        );
        hconn.recv_body(&mut qconn, stream_id, out)
    }


    /// Whether the response has ended
    pub fn response_finished(&self) -> bool {
        self.read_ended
    }

    /// Check whether stream finished with error.
    /// Like `response_finished`, but also attempts to poll the h2 stream for errors that may have
    /// caused the stream to terminate, and returns them as `H2Error`s.
    pub fn check_response_end_or_error(&mut self) -> Result<bool> {
        todo!()
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
            return Ok(None)
        };

        // RFC9110 Section 6.5.1
        // The presence of the keyword "trailers" in the TE header field (Section 10.1.4) of
        // a request indicates that the client is willing to accept trailer fields,
        // on behalf of itself and any downstream clients.
        let mut client_accepts = false;
        if let Some(headers) = &self.request_header_written {
            if let Some(te_header) = headers.headers.get(http::header::TE) {
                let te = te_header.to_str()
                    .explain_err(InvalidHTTPHeader, |_| "failed to parse TE header")?;

                client_accepts = te.contains("trailers")
            }
        };

        let mut response_has_trailers = false;
        if let Some(response) = &self.response_header {
            response_has_trailers = response.headers.get(http::header::TRAILER).is_some()
        };

        if !(client_accepts && response_has_trailers) {
            return Ok(None)
        }

        // as per RFC9114/Section 4.1 it is an optional SINGLE header frame
        // only possible when supported by the version of HTTP in use and enabled by an explicit
        // framing mechanism
        let (trailers, _) = headers_event(self.stream_id()?, self.event_rx()?).await?;
        let trailer_map = headervec_to_headermap(&trailers)?;

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

    /// Give up the http session abruptly.
    pub fn shutdown(&mut self) {
        todo!()
    }

    /// Drop everything in this h2 stream. Return the connection ref.
    /// After this function the underlying h2 connection should already notify the closure of this
    /// stream so that another stream can be created if needed.
    pub(crate) fn conn(&self) -> ConnectionRef {
        todo!()
    }

    /// Whether ping timeout occurred. After a ping timeout, the h2 connection will be terminated.
    /// Ongoing h2 streams will receive an stream/connection error. The streams should check this
    /// flag to tell whether the error is triggered by the timeout.
    pub(crate) fn ping_timedout(&self) -> bool {
        todo!()
    }

    /// Return the [Digest] of the connection
    ///
    /// For reused connection, the timing in the digest will reflect its initial handshakes
    /// The caller should check if the connection is reused to avoid misuse the timing field.
    pub fn digest(&self) -> Option<&Digest> {
        todo!()
    }

    /// Return a mutable [Digest] reference for the connection
    ///
    /// Will return `None` if multiple H2 streams are open.
    pub fn digest_mut(&mut self) -> Option<&mut Digest> {
        todo!()
    }

    /// Return the server (peer) address recorded in the connection digest.
    pub fn server_addr(&self) -> Option<&SocketAddr> {
        todo!()
    }

    /// Return the client (local) address recorded in the connection digest.
    pub fn client_addr(&self) -> Option<&SocketAddr> {
        todo!()
    }

    /// the FD of the underlying connection
    pub fn fd(&self) -> UniqueIDType {
        todo!()
    }

    /// take the body sender to another task to perform duplex read and write
    pub fn take_request_body_writer(&mut self) -> Option<SendStream<Bytes>> {
        todo!()
    }
}

async fn headers_event(stream_id: u64, event_rx: &mut Receiver<Event>) -> Result<(Vec<Header>, bool)> {
    loop {
        match event_rx.recv().await {
            Some(ev) => {
                trace!("stream {} event {:?}", stream_id, ev);
                match ev {
                    Event::Finished => {
                        debug_assert!(false, "Finished event when Headers requested");
                    }
                    Event::Headers { list, more_frames } => {
                        return Ok((list, more_frames))
                    }
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
                    "H3 session event channel disconnected",
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
}

impl Http3Poll {
    pub(crate) async fn start(mut self) -> Result<()> {
//        let conn_id = self.conn_io.conn_id.clone();
        'poll: loop {
            let res = {
                let mut qconn = self.conn_io.quic.lock();
                let mut hconn = self.conn_io.http3.lock();
                hconn.poll(&mut qconn)
            };

            let (stream_id, ev) = match res {
                Ok((stream, ev)) => (stream, ev),
                Err(e) => match e {
                    h3::Error::Done => {
                        self.sessions_housekeeping()?;
                        self.conn_io.rx_notify.notified().await;
                        continue 'poll
                    }
                    _ => {
                        break 'poll Err(e).explain_err(
                            H3Error,  |_| format!("failed to poll h3 connection {:?}" , e))
                    }
                }
            };

            let session = if let Some(session) = self.sessions.get_mut(&stream_id) {
                session
            } else {
                self.add_sessions()?;
                let Some(session) = self.sessions.get_mut(&stream_id) else {
                    return Err(Error::explain(
                        InternalError,
                        format!("missing session channel for stream id {}", stream_id)))
                };
                session
            };

            session.send(ev).await
                .explain_err(H3Error, |_| "failed to forward h3 event to session")?
        }
    }

    fn sessions_housekeeping(&mut self) -> Result<()> {
        self.drop_sessions()?;
        self.add_sessions()
    }

    fn add_sessions(&mut self) -> Result<()>{
        let mut add_sessions = self.add_sessions.lock();
        while let Some((stream_id, sender)) = add_sessions.pop_front() {
            if let Some(_sender) = self.sessions.insert(stream_id, sender) {
                debug_assert!(false, "stream id {} existed", stream_id);
                return Err(Error::explain(
                    InternalError, format!("stream id {} was already present in sessions", stream_id)))
            } else {
                debug!("connection {:?} added stream id {} to sessions", self.conn_io.conn_id, stream_id)
            }
        }
        Ok(())
    }

    fn drop_sessions(&mut self) -> Result<()>{
        let mut drop_sessions = self.drop_sessions.lock();
        while let Some(stream_id) = drop_sessions.pop_front() {
            if let Some(_sender) = self.sessions.remove(&stream_id) {
                debug!("connection {:?} removed stream id {} from sessions", self.conn_io.conn_id, stream_id)
            } else {
                return Err(Error::explain(
                    InternalError, format!("failed to remove session with stream id {}", stream_id)))
            }
        }
        Ok(())
    }
}