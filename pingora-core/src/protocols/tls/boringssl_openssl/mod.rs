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

//! BoringSSL & OpenSSL TLS specific implementation

use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use tokio::io::{self, AsyncRead, AsyncWrite, ReadBuf};

use pingora_error::ErrorType::TLSHandshakeFailure;
use pingora_error::{OrErr, Result};

use crate::protocols::tls::boringssl_openssl::stream::InnerStream;
use crate::protocols::tls::SslDigest;
use crate::protocols::{Ssl, UniqueID, ALPN};
use crate::tls::hash::MessageDigest;
use crate::tls::ssl;
use crate::tls::ssl::SslRef;
use crate::utils::tls::boringssl_openssl::{get_x509_organization, get_x509_serial};

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
    /// The caller needs to perform [`Self::connect()`] or [`Self::accept()`] to perform TLS
    /// handshake after.
    pub fn new(ssl: ssl::Ssl, stream: T) -> Result<Self> {
        let tls = InnerStream::new(ssl, stream)
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
        Self::clear_error(&self);
        Pin::new(&mut self.tls.0).poll_read(cx, buf)
    }
}

impl<T: AsyncRead + AsyncWrite + Unpin> TlsStream<T> {
    #[inline]
    fn clear_error(&self) {
        InnerStream::<T>::clear_error()
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
        Self::clear_error(&self);
        Pin::new(&mut self.tls.0).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<io::Result<()>> {
        Self::clear_error(&self);
        Pin::new(&mut self.tls.0).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<io::Result<()>> {
        Self::clear_error(&self);
        Pin::new(&mut self.tls.0).poll_shutdown(cx)
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[std::io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        Self::clear_error(&self);
        Pin::new(&mut self.tls.0).poll_write_vectored(cx, bufs)
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
        self.tls.0.get_ref().id()
    }
}

impl<T> Ssl for TlsStream<T> {
    fn get_ssl(&self) -> Option<&ssl::SslRef> {
        Some(self.tls.0.ssl())
    }

    fn get_ssl_digest(&self) -> Option<Arc<SslDigest>> {
        self.ssl_digest()
    }

    fn selected_alpn_proto(&self) -> Option<ALPN> {
        let ssl = self.tls.0.ssl();
        ALPN::from_wire_selected(ssl.selected_alpn_protocol()?)
    }
}

impl SslDigest {
    pub fn from_ssl(ssl: &SslRef) -> Self {
        let cipher = match ssl.current_cipher() {
            Some(c) => c.name(),
            None => "",
        };

        let (cert_digest, org, sn) = match ssl.peer_certificate() {
            Some(cert) => {
                let cert_digest = match cert.digest(MessageDigest::sha256()) {
                    Ok(c) => c.as_ref().to_vec(),
                    Err(_) => Vec::new(),
                };
                (
                    cert_digest,
                    get_x509_organization(&cert),
                    get_x509_serial(&cert).ok(),
                )
            }
            None => (Vec::new(), None, None),
        };

        SslDigest {
            cipher,
            version: ssl.version_str(),
            organization: org,
            serial_number: sn,
            cert_digest,
        }
    }
}
