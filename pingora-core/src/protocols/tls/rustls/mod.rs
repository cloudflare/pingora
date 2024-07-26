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

//! Rustls TLS specific implementation

use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use pingora_error::ErrorType::{InternalError, TLSHandshakeFailure};
use pingora_error::{OrErr, Result};
use pingora_rustls::TlsStream as RusTlsStream;
use pingora_rustls::{hash_certificate, ServerName, TlsConnector};
use tokio::io::{self, AsyncRead, AsyncWrite, ReadBuf};
use x509_parser::nom::AsBytes;

use crate::utils::tls::rustls::get_organization_serial;

use crate::listeners::tls::Acceptor;
use crate::protocols::tls::rustls::stream::InnerStream;
use crate::protocols::tls::SslDigest;
use crate::protocols::{Ssl, UniqueID, ALPN};

use super::TlsStream;

pub mod client;
pub mod server;
pub(super) mod stream;

impl<T> TlsStream<T>
where
    T: AsyncRead + AsyncWrite + Unpin + Send,
{
    /// Create a new TLS connection from the given `stream`
    ///
    /// Using RustTLS the stream is only returned after the handshake.
    /// The caller does therefor not need to perform [`Self::connect()`].
    pub async fn from_connector(connector: &TlsConnector, domain: &str, stream: T) -> Result<Self> {
        let server = ServerName::try_from(domain)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))
            .explain_err(InternalError, |e| {
                format!("failed to parse domain: {}, error: {}", domain, e)
            })?
            .to_owned();

        let tls = InnerStream::from_connector(connector, server, stream)
            .await
            .explain_err(TLSHandshakeFailure, |e| format!("tls stream error: {e}"))?;

        Ok(TlsStream {
            tls,
            digest: None,
            timing: Default::default(),
        })
    }

    /// Create a new TLS connection from the given `stream`
    ///
    /// Using RustTLS the stream is only returned after the handshake.
    /// The caller does therefor not need to perform [`Self::accept()`].
    pub(crate) async fn from_acceptor(acceptor: &Acceptor, stream: T) -> Result<Self> {
        let tls = InnerStream::from_acceptor(acceptor, stream)
            .await
            .explain_err(TLSHandshakeFailure, |e| format!("tls stream error: {e}"))?;

        Ok(TlsStream {
            tls,
            digest: None,
            timing: Default::default(),
        })
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
        Pin::new(&mut self.tls.stream.as_mut().unwrap()).poll_read(cx, buf)
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
        Pin::new(&mut self.tls.stream.as_mut().unwrap()).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<io::Result<()>> {
        Pin::new(&mut self.tls.stream.as_mut().unwrap()).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<io::Result<()>> {
        Pin::new(&mut self.tls.stream.as_mut().unwrap()).poll_shutdown(cx)
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[std::io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.tls.stream.as_mut().unwrap()).poll_write_vectored(cx, bufs)
    }

    fn is_write_vectored(&self) -> bool {
        true
    }
}

impl<T> UniqueID for TlsStream<T>
where
    T: UniqueID,
{
    fn id(&self) -> i32 {
        self.tls.stream.as_ref().unwrap().get_ref().0.id()
    }
}

impl<T> Ssl for TlsStream<T> {
    fn get_ssl_digest(&self) -> Option<Arc<SslDigest>> {
        self.ssl_digest()
    }

    fn selected_alpn_proto(&self) -> Option<ALPN> {
        let st = self.tls.stream.as_ref();
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
