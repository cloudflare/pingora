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

use crate::protocols::digest::TimingDigest;
use crate::protocols::tls::{SslDigest, ALPN};
use crate::protocols::{Peek, Ssl, UniqueID, UniqueIDType};
use crate::tls::{self, ssl, tokio_ssl::SslStream as InnerSsl};
use crate::utils::tls::{get_organization, get_serial};
use log::warn;
use pingora_error::{ErrorType::*, OrErr, Result};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::SystemTime;
use tokio::io::{self, AsyncRead, AsyncWrite, ReadBuf};

#[cfg(feature = "boringssl")]
use pingora_boringssl as ssl_lib;

#[cfg(feature = "openssl")]
use pingora_openssl as ssl_lib;

use ssl_lib::{hash::MessageDigest, ssl::SslRef};

/// The TLS connection
#[derive(Debug)]
pub struct SslStream<T> {
    ssl: InnerSsl<T>,
    digest: Option<Arc<SslDigest>>,
    pub(super) timing: TimingDigest,
}

impl<T> SslStream<T>
where
    T: AsyncRead + AsyncWrite + std::marker::Unpin,
{
    /// Create a new TLS connection from the given `stream`
    ///
    /// The caller needs to perform [`Self::connect()`] or [`Self::accept()`] to perform TLS
    /// handshake after.
    pub fn new(ssl: ssl::Ssl, stream: T) -> Result<Self> {
        let ssl = InnerSsl::new(ssl, stream)
            .explain_err(TLSHandshakeFailure, |e| format!("ssl stream error: {e}"))?;

        Ok(SslStream {
            ssl,
            digest: None,
            timing: Default::default(),
        })
    }

    /// Connect to the remote TLS server as a client
    pub async fn connect(&mut self) -> Result<(), ssl::Error> {
        Self::clear_error();
        Pin::new(&mut self.ssl).connect().await?;
        self.timing.established_ts = SystemTime::now();
        self.digest = Some(Arc::new(SslDigest::from_ssl(self.ssl())));
        Ok(())
    }

    /// Finish the TLS handshake from client as a server
    pub async fn accept(&mut self) -> Result<(), ssl::Error> {
        Self::clear_error();
        Pin::new(&mut self.ssl).accept().await?;
        self.timing.established_ts = SystemTime::now();
        self.digest = Some(Arc::new(SslDigest::from_ssl(self.ssl())));
        Ok(())
    }

    #[inline]
    fn clear_error() {
        let errs = tls::error::ErrorStack::get();
        if !errs.errors().is_empty() {
            warn!("Clearing dirty TLS error stack: {}", errs);
        }
    }
}

impl<T> SslStream<T> {
    pub fn ssl_digest(&self) -> Option<Arc<SslDigest>> {
        self.digest.clone()
    }
}

use std::ops::{Deref, DerefMut};

impl<T> Deref for SslStream<T> {
    type Target = InnerSsl<T>;

    fn deref(&self) -> &Self::Target {
        &self.ssl
    }
}

impl<T> DerefMut for SslStream<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.ssl
    }
}

impl<T> AsyncRead for SslStream<T>
where
    T: AsyncRead + AsyncWrite + Unpin,
{
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Self::clear_error();
        Pin::new(&mut self.ssl).poll_read(cx, buf)
    }
}

impl<T> AsyncWrite for SslStream<T>
where
    T: AsyncRead + AsyncWrite + Unpin,
{
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Self::clear_error();
        Pin::new(&mut self.ssl).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<io::Result<()>> {
        Self::clear_error();
        Pin::new(&mut self.ssl).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<io::Result<()>> {
        Self::clear_error();
        Pin::new(&mut self.ssl).poll_shutdown(cx)
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[std::io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        Self::clear_error();
        Pin::new(&mut self.ssl).poll_write_vectored(cx, bufs)
    }

    fn is_write_vectored(&self) -> bool {
        true
    }
}

impl<T> UniqueID for SslStream<T>
where
    T: UniqueID,
{
    fn id(&self) -> UniqueIDType {
        self.ssl.get_ref().id()
    }
}

impl<T> Ssl for SslStream<T> {
    fn get_ssl(&self) -> Option<&ssl::SslRef> {
        Some(self.ssl())
    }

    fn get_ssl_digest(&self) -> Option<Arc<SslDigest>> {
        self.ssl_digest()
    }

    /// Return selected ALPN if any
    fn selected_alpn_proto(&self) -> Option<ALPN> {
        let ssl = self.get_ssl()?;
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
                (cert_digest, get_organization(&cert), get_serial(&cert).ok())
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

// TODO: implement Peek if needed
impl<T> Peek for SslStream<T> {}
