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

use async_trait::async_trait;
use log::warn;
use std::pin::Pin;
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncWrite};

use pingora_error::{Error, ErrorType::*, OrErr, Result};

use crate::listeners::ALPN;
use crate::protocols::digest::{GetSocketDigest, SocketDigest, TimingDigest};
use crate::protocols::raw_connect::ProxyDigest;
use crate::protocols::tls::InnerTlsStream;
use crate::protocols::tls::SslDigest;
use crate::protocols::{GetProxyDigest, GetTimingDigest};
use crate::tls::error::ErrorStack;
use crate::tls::ext;
use crate::tls::tokio_ssl;
use crate::tls::tokio_ssl::SslStream;
use crate::tls::{ssl, ssl::SslRef, ssl_sys::X509_V_ERR_INVALID_CALL};

#[derive(Debug)]
pub struct InnerStream<T>(pub(crate) tokio_ssl::SslStream<T>);

impl<T: AsyncRead + AsyncWrite + Unpin> InnerStream<T> {
    /// Create a new TLS connection from the given `stream`
    ///
    /// The caller needs to perform [`Self::connect()`] or [`Self::accept()`] to perform TLS
    /// handshake after.
    pub(crate) fn new(ssl: ssl::Ssl, stream: T) -> Result<Self> {
        let ssl = SslStream::new(ssl, stream)
            .explain_err(TLSHandshakeFailure, |e| format!("tls.rs stream error: {e}"))?;

        Ok(InnerStream(ssl))
    }

    #[inline]
    pub(crate) fn clear_error() {
        let errs = ErrorStack::get();
        if !errs.errors().is_empty() {
            warn!("Clearing dirty TLS error stack: {}", errs);
        }
    }
}

#[async_trait]
impl<T: AsyncRead + AsyncWrite + Unpin + Send> InnerTlsStream for InnerStream<T> {
    /// Connect to the remote TLS server as a client
    async fn connect(&mut self) -> Result<()> {
        Self::clear_error();
        match Pin::new(&mut self.0).connect().await {
            Ok(_) => Ok(()),
            Err(err) => self.transform_ssl_error(err),
        }
    }

    /// Finish the TLS handshake from client as a server
    async fn accept(&mut self) -> Result<()> {
        Self::clear_error();
        match Pin::new(&mut self.0).accept().await {
            Ok(_) => Ok(()),
            Err(err) => self.transform_ssl_error(err),
        }
    }

    fn digest(&mut self) -> Option<Arc<SslDigest>> {
        Some(Arc::new(SslDigest::from_ssl(self.0.ssl())))
    }

    fn selected_alpn_proto(&mut self) -> Option<ALPN> {
        let ssl = self.0.ssl();
        ALPN::from_wire_selected(ssl.selected_alpn_protocol()?)
    }
}

impl<T: AsyncRead + AsyncWrite + Unpin> InnerStream<T> {
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

                    match verify_result(self.0.ssl()) {
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

impl<S> GetSocketDigest for InnerStream<S>
where
    S: GetSocketDigest,
{
    fn get_socket_digest(&self) -> Option<Arc<SocketDigest>> {
        self.0.get_ref().get_socket_digest()
    }
    fn set_socket_digest(&mut self, socket_digest: SocketDigest) {
        self.0.get_mut().set_socket_digest(socket_digest)
    }
}

impl<S> GetTimingDigest for InnerStream<S>
where
    S: GetTimingDigest,
{
    fn get_timing_digest(&self) -> Vec<Option<TimingDigest>> {
        self.0.get_ref().get_timing_digest()
    }
}

impl<S> GetProxyDigest for InnerStream<S>
where
    S: GetProxyDigest,
{
    fn get_proxy_digest(&self) -> Option<Arc<ProxyDigest>> {
        self.0.get_ref().get_proxy_digest()
    }
}
