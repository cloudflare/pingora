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

//! BoringSSL & OpenSSL TLS stream specific implementation

use crate::tls::hash::MessageDigest;
use log::warn;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::SystemTime;
use tokio::io;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

use crate::listeners::ALPN;
use crate::protocols::digest::{GetSocketDigest, SocketDigest, TimingDigest};
use crate::protocols::raw_connect::ProxyDigest;
use crate::protocols::tls::SslDigest;
use crate::protocols::{GetProxyDigest, GetTimingDigest, Ssl, UniqueID};
use crate::tls::error::ErrorStack;
use crate::tls::ext;
use crate::tls::tokio_ssl::SslStream;
use crate::tls::{ssl, ssl::SslRef, ssl_sys::X509_V_ERR_INVALID_CALL};
use pingora_error::{Error, ErrorType::*, OrErr, Result};

#[derive(Debug)]
pub struct TlsStream<T> {
    pub(crate) stream: SslStream<T>,
    pub(super) digest: Option<Arc<SslDigest>>,
    pub(super) timing: TimingDigest,
}

impl<T: AsyncRead + AsyncWrite + Unpin> TlsStream<T> {
    /// Create a new TLS connection from the given `stream`
    ///
    /// The caller needs to perform [`Self::connect()`] or [`Self::accept()`] to perform TLS
    /// handshake after.
    pub(crate) fn new(ssl: ssl::Ssl, stream: T) -> Result<Self> {
        let stream = SslStream::new(ssl, stream)
            .explain_err(TLSHandshakeFailure, |e| format!("tls.rs stream error: {e}"))?;

        Ok(TlsStream {
            stream,
            timing: Default::default(),
            digest: None,
        })
    }

    #[inline]
    pub(crate) fn clear_error() {
        let errs = ErrorStack::get();
        if !errs.errors().is_empty() {
            warn!("Clearing dirty TLS error stack: {}", errs);
        }
    }
}

impl<T: AsyncRead + AsyncWrite + Unpin + Send> TlsStream<T> {
    /// Connect to the remote TLS server as a client
    pub(crate) async fn connect(&mut self) -> Result<()> {
        Self::clear_error();
        match Pin::new(&mut self.stream).connect().await {
            Ok(_) => {
                self.timing.established_ts = SystemTime::now();
                self.digest = self.digest();
                Ok(())
            }
            Err(err) => self.transform_ssl_error(err),
        }
    }

    /// Finish the TLS handshake from client as a server
    pub(crate) async fn accept(&mut self) -> Result<()> {
        Self::clear_error();
        match Pin::new(&mut self.stream).accept().await {
            Ok(_) => {
                self.timing.established_ts = SystemTime::now();
                self.digest = self.digest();
                Ok(())
            }
            Err(err) => self.transform_ssl_error(err),
        }
    }

    pub(crate) fn digest(&mut self) -> Option<Arc<SslDigest>> {
        Some(Arc::new(SslDigest::from_ssl(self.stream.ssl())))
    }

    fn transform_ssl_error(&self, e: ssl::Error) -> Result<()> {
        let context = format!("ssl::ErrorCode: {:?}", e.code());
        if ext::is_suspended_for_cert(&e) {
            Error::e_explain(TLSWantX509Lookup, format!("ssl::ErrorCode: {:?}", e.code()))
        } else {
            match e.code() {
                ssl::ErrorCode::SSL => {
                    // Unify the return type of `verify_result` for openssl
                    #[cfg(not(feature = "boringssl"))]
                    fn verify_result(ssl: &SslRef) -> Result<(), i32> {
                        match ssl.verify_result().as_raw() {
                            crate::tls::ssl_sys::X509_V_OK => Ok(()),
                            e => Err(e),
                        }
                    }

                    // Unify the return type of `verify_result` for boringssl
                    #[cfg(feature = "boringssl")]
                    fn verify_result(ssl: &SslRef) -> Result<(), i32> {
                        ssl.verify_result().map_err(|e| e.as_raw())
                    }

                    match verify_result(self.stream.ssl()) {
                        Ok(()) => Error::e_explain(TLSHandshakeFailure, context),
                        // X509_V_ERR_INVALID_CALL in case verify result was never set
                        Err(X509_V_ERR_INVALID_CALL) => {
                            Error::e_explain(TLSHandshakeFailure, context)
                        }
                        _ => Error::e_explain(InvalidCert, context),
                    }
                }
                /* likely network error, but still mark as TLS error */
                _ => Error::e_explain(TLSHandshakeFailure, context),
            }
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
        Self::clear_error();
        Pin::new(&mut self.stream).poll_read(cx, buf)
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
        Self::clear_error();
        Pin::new(&mut self.stream).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<io::Result<()>> {
        Self::clear_error();
        Pin::new(&mut self.stream).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<io::Result<()>> {
        Self::clear_error();
        Pin::new(&mut self.stream).poll_shutdown(cx)
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[std::io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        Self::clear_error();
        Pin::new(&mut self.stream).poll_write_vectored(cx, bufs)
    }

    fn is_write_vectored(&self) -> bool {
        true
    }
}

impl<T> Ssl for TlsStream<T> {
    fn get_ssl(&self) -> Option<&ssl::SslRef> {
        Some(self.stream.ssl())
    }

    fn get_ssl_digest(&self) -> Option<Arc<SslDigest>> {
        self.digest.clone()
    }

    fn selected_alpn_proto(&self) -> Option<ALPN> {
        let ssl = self.stream.ssl();
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
                    crate::utils::tls::boringssl_openssl::get_x509_organization(&cert),
                    crate::utils::tls::boringssl_openssl::get_x509_serial(&cert).ok(),
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

impl<S> GetSocketDigest for TlsStream<S>
where
    S: GetSocketDigest,
{
    fn get_socket_digest(&self) -> Option<Arc<SocketDigest>> {
        self.stream.get_ref().get_socket_digest()
    }
    fn set_socket_digest(&mut self, socket_digest: SocketDigest) {
        self.stream.get_mut().set_socket_digest(socket_digest)
    }
}

impl<S> GetTimingDigest for TlsStream<S>
where
    S: GetTimingDigest,
{
    fn get_timing_digest(&self) -> Vec<Option<TimingDigest>> {
        self.stream.get_ref().get_timing_digest()
    }
}

impl<S> GetProxyDigest for TlsStream<S>
where
    S: GetProxyDigest,
{
    fn get_proxy_digest(&self) -> Option<Arc<ProxyDigest>> {
        self.stream.get_ref().get_proxy_digest()
    }
}

impl<T> UniqueID for TlsStream<T>
where
    T: UniqueID,
{
    fn id(&self) -> i32 {
        self.stream.get_ref().id()
    }
}
