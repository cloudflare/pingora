use crate::connectors::http::v2::ConnectionRef;
use crate::protocols::l4::socket::SocketAddr;
use crate::protocols::{Digest, UniqueIDType};
use bytes::Bytes;
use h2::client::SendRequest;
use h2::SendStream;
use http::HeaderMap;
use pingora_http::{RequestHeader, ResponseHeader};
use std::time::Duration;

// FIXME: implement client H3Session
pub struct Http3Session {
    pub read_timeout: Option<Duration>,
    send_req: SendRequest<Bytes>,
    conn: ConnectionRef,
}

impl Http3Session {
    pub(crate) fn new(send_req: SendRequest<Bytes>, conn: ConnectionRef) -> Self {
        Self {
            read_timeout: None,
            send_req,
            conn,
        }
    }

    /// Write the request header to the server
    pub fn write_request_header(
        &mut self,
        mut req: Box<RequestHeader>,
        end: bool,
    ) -> pingora_error::Result<()> {
        todo!()
    }

    /// Write a request body chunk
    pub fn write_request_body(&mut self, data: Bytes, end: bool) -> pingora_error::Result<()> {
        todo!()
    }

    /// Signal that the request body has ended
    pub fn finish_request_body(&mut self) -> pingora_error::Result<()> {
        todo!()
    }

    /// Read the response header
    pub async fn read_response_header(&mut self) -> pingora_error::Result<()> {
        todo!()
    }

    /// Read the response body
    ///
    /// `None` means, no more body to read
    pub async fn read_response_body(&mut self) -> pingora_error::Result<Option<Bytes>> {
        todo!()
    }

    /// Whether the response has ended
    pub fn response_finished(&self) -> bool {
        todo!()
    }

    /// Check whether stream finished with error.
    /// Like `response_finished`, but also attempts to poll the h2 stream for errors that may have
    /// caused the stream to terminate, and returns them as `H2Error`s.
    pub fn check_response_end_or_error(&mut self) -> pingora_error::Result<bool> {
        todo!()
    }

    /// Read the optional trailer headers
    pub async fn read_trailers(&mut self) -> pingora_error::Result<Option<HeaderMap>> {
        todo!()
    }

    /// The request header if it is already sent
    pub fn request_header(&self) -> Option<&RequestHeader> {
        todo!()
    }

    /// The response header if it is already read
    pub fn response_header(&self) -> Option<&ResponseHeader> {
        todo!()
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
