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

//! TLS server specific implementation

use super::SslStream;
use crate::protocols::{Shutdown, IO};
use crate::tls::ext;
use crate::tls::ext::ssl_from_acceptor;
use crate::tls::ssl;
use crate::tls::ssl::{SslAcceptor, SslRef};

use async_trait::async_trait;
use log::warn;
use pingora_error::{ErrorType::*, OrErr, Result};
use std::pin::Pin;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};

/// Prepare a TLS stream for handshake
pub fn prepare_tls_stream<S: IO>(ssl_acceptor: &SslAcceptor, io: S) -> Result<SslStream<S>> {
    let ssl = ssl_from_acceptor(ssl_acceptor)
        .explain_err(TLSHandshakeFailure, |e| format!("ssl_acceptor error: {e}"))?;
    SslStream::new(ssl, io).explain_err(TLSHandshakeFailure, |e| format!("ssl stream error: {e}"))
}

/// Perform TLS handshake for the given connection with the given configuration
pub async fn handshake<S: IO>(ssl_acceptor: &SslAcceptor, io: S) -> Result<SslStream<S>> {
    let mut stream = prepare_tls_stream(ssl_acceptor, io)?;
    stream
        .accept()
        .await
        .explain_err(TLSHandshakeFailure, |e| format!("TLS accept() failed: {e}"))?;
    Ok(stream)
}

/// Perform TLS handshake for the given connection with the given configuration and callbacks
pub async fn handshake_with_callback<S: IO>(
    ssl_acceptor: &SslAcceptor,
    io: S,
    callbacks: &TlsAcceptCallbacks,
) -> Result<SslStream<S>> {
    let mut tls_stream = prepare_tls_stream(ssl_acceptor, io)?;
    let done = Pin::new(&mut tls_stream)
        .start_accept()
        .await
        .explain_err(TLSHandshakeFailure, |e| format!("TLS accept() failed: {e}"))?;
    if !done {
        // safety: we do hold a mut ref of tls_stream
        let ssl_mut = unsafe { ext::ssl_mut(tls_stream.ssl()) };
        callbacks.certificate_callback(ssl_mut).await;
        Pin::new(&mut tls_stream)
            .resume_accept()
            .await
            .explain_err(TLSHandshakeFailure, |e| format!("TLS accept() failed: {e}"))?;
        Ok(tls_stream)
    } else {
        Ok(tls_stream)
    }
}

/// The APIs to customize things like certificate during TLS server side handshake
#[async_trait]
pub trait TlsAccept {
    // TODO: return error?
    /// This function is called in the middle of a TLS handshake. Structs who implement this function
    /// should provide tls certificate and key to the [SslRef] via [ext::ssl_use_certificate] and [ext::ssl_use_private_key].
    async fn certificate_callback(&self, _ssl: &mut SslRef) -> () {
        // does nothing by default
    }
}

pub type TlsAcceptCallbacks = Box<dyn TlsAccept + Send + Sync>;

#[async_trait]
impl<S> Shutdown for SslStream<S>
where
    S: AsyncRead + AsyncWrite + Sync + Unpin + Send,
{
    async fn shutdown(&mut self) {
        match <Self as AsyncWriteExt>::shutdown(self).await {
            Ok(()) => {}
            Err(e) => {
                warn!("TLS shutdown failed, {e}");
            }
        }
    }
}

/// Resumable TLS server side handshake.
#[async_trait]
pub trait ResumableAccept {
    /// Start a resumable TLS accept handshake.
    ///
    /// * `Ok(true)` when the handshake is finished
    /// * `Ok(false)`` when the handshake is paused midway
    ///
    /// For now, the accept will only pause when a certificate is needed.
    async fn start_accept(self: Pin<&mut Self>) -> Result<bool, ssl::Error>;

    /// Continue the TLS handshake
    ///
    /// This function should be called after the certificate is provided.
    async fn resume_accept(self: Pin<&mut Self>) -> Result<(), ssl::Error>;
}

#[async_trait]
impl<S: AsyncRead + AsyncWrite + Send + Unpin> ResumableAccept for SslStream<S> {
    async fn start_accept(mut self: Pin<&mut Self>) -> Result<bool, ssl::Error> {
        // safety: &mut self
        let ssl_mut = unsafe { ext::ssl_mut(self.ssl()) };
        ext::suspend_when_need_ssl_cert(ssl_mut);
        let res = self.accept().await;

        match res {
            Ok(()) => Ok(true),
            Err(e) => {
                if ext::is_suspended_for_cert(&e) {
                    Ok(false)
                } else {
                    Err(e)
                }
            }
        }
    }

    async fn resume_accept(mut self: Pin<&mut Self>) -> Result<(), ssl::Error> {
        // safety: &mut ssl
        let ssl_mut = unsafe { ext::ssl_mut(self.ssl()) };
        ext::unblock_ssl_cert(ssl_mut);
        self.accept().await
    }
}

#[tokio::test]
async fn test_async_cert() {
    use tokio::io::AsyncReadExt;
    let acceptor = ssl::SslAcceptor::mozilla_intermediate_v5(ssl::SslMethod::tls())
        .unwrap()
        .build();

    struct Callback;
    #[async_trait]
    impl TlsAccept for Callback {
        async fn certificate_callback(&self, ssl: &mut SslRef) -> () {
            assert_eq!(
                ssl.servername(ssl::NameType::HOST_NAME).unwrap(),
                "pingora.org"
            );
            let cert = format!("{}/tests/keys/server.crt", env!("CARGO_MANIFEST_DIR"));
            let key = format!("{}/tests/keys/key.pem", env!("CARGO_MANIFEST_DIR"));

            let cert_bytes = std::fs::read(cert).unwrap();
            let cert = crate::tls::x509::X509::from_pem(&cert_bytes).unwrap();

            let key_bytes = std::fs::read(key).unwrap();
            let key = crate::tls::pkey::PKey::private_key_from_pem(&key_bytes).unwrap();
            ext::ssl_use_certificate(ssl, &cert).unwrap();
            ext::ssl_use_private_key(ssl, &key).unwrap();
        }
    }

    let cb: TlsAcceptCallbacks = Box::new(Callback);

    let (client, server) = tokio::io::duplex(1024);

    tokio::spawn(async move {
        let ssl_context = ssl::SslContext::builder(ssl::SslMethod::tls())
            .unwrap()
            .build();
        let mut ssl = ssl::Ssl::new(&ssl_context).unwrap();
        ssl.set_hostname("pingora.org").unwrap();
        ssl.set_verify(ssl::SslVerifyMode::NONE); // we don have a valid cert
        let mut stream = SslStream::new(ssl, client).unwrap();
        Pin::new(&mut stream).connect().await.unwrap();
        let mut buf = [0; 1];
        let _ = stream.read(&mut buf).await;
    });

    handshake_with_callback(&acceptor, server, &cb)
        .await
        .unwrap();
}
