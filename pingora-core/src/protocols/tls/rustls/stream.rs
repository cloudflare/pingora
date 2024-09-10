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

use crate::listeners::ALPN;
use crate::protocols::digest::{GetSocketDigest, SocketDigest, TimingDigest};
use crate::protocols::raw_connect::ProxyDigest;
use crate::protocols::tls::SslDigest;
use crate::protocols::{GetProxyDigest, GetTimingDigest, Ssl, UniqueID};
use crate::utils::tls::rustls::get_organization_serial;
use core::fmt;
use core::fmt::Formatter;
use pingora_error::ErrorType::{AcceptError, ConnectError, InternalError, TLSHandshakeFailure};
use pingora_error::{Error, ImmutStr, OrErr, Result};
use pingora_rustls::{hash_certificate, Accept, Connect, ServerName, TlsConnector};
use pingora_rustls::{TlsAcceptor, TlsStream as RusTlsStream};
use std::fmt::Debug;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::SystemTime;
use tokio::io;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use x509_parser::nom::AsBytes;

/// The TLS connection
pub struct TlsStream<T> {
    pub(crate) stream: Option<RusTlsStream<T>>,
    connect: Option<Connect<T>>,
    accept: Option<Accept<T>>,
    pub(super) digest: Option<Arc<SslDigest>>,
    pub(super) timing: TimingDigest,
}

impl<T: Debug> Debug for TlsStream<T> {
    fn fmt(&self, fmt: &mut Formatter<'_>) -> fmt::Result {
        fmt.debug_struct("InnerStream")
            .field("stream", &self.stream)
            .field("connect", {
                if self.connect.is_some() {
                    &"Some(Connect<T>)"
                } else {
                    &"None"
                }
            })
            .field("accept", {
                if self.accept.is_some() {
                    &"Some(Accept<T>)"
                } else {
                    &"None"
                }
            })
            .finish()
    }
}

impl<T: AsyncRead + AsyncWrite + Unpin> TlsStream<T> {
    /// Create a new TLS connection from the given `stream`
    ///
    /// Using RustTLS the stream is only returned after the handshake.
    ///
    /// The caller needs to perform [`Self::accept()`] to perform the TLS handshake.
    pub(crate) async fn from_acceptor(acceptor: &TlsAcceptor, stream: T) -> Result<Self> {
        let accept = acceptor.accept(stream);

        Ok(TlsStream {
            accept: Some(accept),
            digest: None,
            connect: None,
            stream: None,
            timing: Default::default(),
        })
    }

    /// Create a new TLS connection from the given `stream`
    ///
    /// Using RustTLS the stream is only returned after the handshake.
    ///
    /// The caller needs to perform [`Self::connect()`] to perform the TLS handshake.
    pub async fn from_connector(connector: &TlsConnector, domain: &str, stream: T) -> Result<Self> {
        let server = ServerName::try_from(domain)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))
            .explain_err(InternalError, |e| {
                format!("failed to parse domain: {}, error: {}", domain, e)
            })?
            .to_owned();

        let connect = connector.connect(server.to_owned(), stream);

        Ok(TlsStream {
            accept: None,
            digest: None,
            connect: Some(connect),
            stream: None,
            timing: Default::default(),
        })
    }
}

impl<T: AsyncRead + AsyncWrite + Unpin + Send> TlsStream<T> {
    /// Connect to the remote TLS server as a client
    pub(crate) async fn connect(&mut self) -> Result<()> {
        let connect = &mut self.connect;

        if let Some(ref mut connect) = connect {
            let stream = connect
                .await
                .explain_err(TLSHandshakeFailure, |e| format!("tls connect error: {e}"))?;
            self.stream = Some(RusTlsStream::Client(stream));
            self.connect = None;

            self.timing.established_ts = SystemTime::now();
            self.digest = self.digest();
            Ok(())
        } else {
            Err(Error::explain(
                ConnectError,
                ImmutStr::from("TLS connect not available to perform handshake."),
            ))
        }
    }

    /// Finish the TLS handshake from client as a server
    /// no-op implementation within Rustls, handshake is performed during creation of stream.
    pub(crate) async fn accept(&mut self) -> Result<()> {
        if let Some(ref mut accept) = &mut self.accept {
            let stream = accept
                .await
                .explain_err(TLSHandshakeFailure, |e| format!("tls connect error: {e}"))?;
            self.stream = Some(RusTlsStream::Server(stream));
            self.connect = None;

            self.timing.established_ts = SystemTime::now();
            self.digest = self.digest();
            Ok(())
        } else {
            Err(Error::explain(
                AcceptError,
                ImmutStr::from("TLS accept not available to perform handshake."),
            ))
        }
    }

    pub(crate) fn digest(&mut self) -> Option<Arc<SslDigest>> {
        Some(Arc::new(SslDigest::from_stream(&self.stream)))
    }
}

impl<S> GetSocketDigest for TlsStream<S>
where
    S: GetSocketDigest,
{
    fn get_socket_digest(&self) -> Option<Arc<SocketDigest>> {
        if let Some(stream) = self.stream.as_ref() {
            stream.get_ref().0.get_socket_digest()
        } else {
            None
        }
    }
    fn set_socket_digest(&mut self, socket_digest: SocketDigest) {
        self.stream
            .as_mut()
            .unwrap()
            .get_mut()
            .0
            .set_socket_digest(socket_digest)
    }
}

impl<S> GetTimingDigest for TlsStream<S>
where
    S: GetTimingDigest,
{
    fn get_timing_digest(&self) -> Vec<Option<TimingDigest>> {
        self.stream
            .as_ref()
            .unwrap()
            .get_ref()
            .0
            .get_timing_digest()
    }
}

impl<S> GetProxyDigest for TlsStream<S>
where
    S: GetProxyDigest,
{
    fn get_proxy_digest(&self) -> Option<Arc<ProxyDigest>> {
        if let Some(stream) = self.stream.as_ref() {
            stream.get_ref().0.get_proxy_digest()
        } else {
            None
        }
    }
}

impl<T> AsyncRead for TlsStream<T>
where
    T: AsyncRead + AsyncWrite + Unpin,
{
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream.as_mut().unwrap()).poll_read(cx, buf)
    }
}

impl<T> AsyncWrite for TlsStream<T>
where
    T: AsyncRead + AsyncWrite + Unpin,
{
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.stream.as_mut().unwrap()).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream.as_mut().unwrap()).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream.as_mut().unwrap()).poll_shutdown(cx)
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[std::io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.stream.as_mut().unwrap()).poll_write_vectored(cx, bufs)
    }

    fn is_write_vectored(&self) -> bool {
        true
    }
}

impl<T> Ssl for TlsStream<T> {
    fn get_ssl_digest(&self) -> Option<Arc<SslDigest>> {
        self.digest.clone()
    }

    fn selected_alpn_proto(&self) -> Option<ALPN> {
        let st = self.stream.as_ref();
        if let Some(stream) = st {
            let proto = stream.get_ref().1.alpn_protocol();
            match proto {
                None => None,
                Some(raw) => ALPN::from_wire_selected(raw),
            }
        } else {
            None
        }
    }
}

impl SslDigest {
    fn from_stream<T>(stream: &Option<RusTlsStream<T>>) -> Self {
        let stream = stream.as_ref().unwrap();
        let (_io, session) = stream.get_ref();
        let protocol = session.protocol_version();
        let cipher_suite = session.negotiated_cipher_suite();
        let peer_certificates = session.peer_certificates();

        let cipher = match cipher_suite {
            Some(suite) => match suite.suite().as_str() {
                Some(suite_str) => suite_str,
                None => "",
            },
            None => "",
        };

        let version = match protocol {
            Some(proto) => match proto.as_str() {
                Some(ver) => ver,
                None => "",
            },
            None => "",
        };

        let cert_digest = match peer_certificates {
            Some(certs) => match certs.first() {
                Some(cert) => hash_certificate(cert.clone()),
                None => vec![],
            },
            None => vec![],
        };

        let (organization, serial_number) = match peer_certificates {
            Some(certs) => match certs.first() {
                Some(cert) => {
                    let (organization, serial) = get_organization_serial(cert.as_bytes());
                    (organization, Some(serial))
                }
                None => (None, None),
            },
            None => (None, None),
        };

        SslDigest {
            cipher: &cipher,
            version: &version,
            organization,
            serial_number,
            cert_digest,
        }
    }
}

impl<T> UniqueID for TlsStream<T>
where
    T: UniqueID,
{
    fn id(&self) -> i32 {
        self.stream.as_ref().unwrap().get_ref().0.id()
    }
}
