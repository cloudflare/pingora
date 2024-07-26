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

//! BoringSSL & OpenSSL TLS server specific implementation

use std::pin::Pin;

use async_trait::async_trait;
use tokio::io::{AsyncRead, AsyncWrite};

use pingora_error::ErrorType::{TLSHandshakeFailure, TLSWantX509Lookup};
use pingora_error::{OrErr, Result};

use crate::listeners::tls::Acceptor;
use crate::protocols::tls::boringssl_openssl::TlsStream;
use crate::protocols::tls::server::{ResumableAccept, TlsAcceptCallbacks};
use crate::protocols::{Ssl, IO};
use crate::tls::ext;
use crate::tls::ext::ssl_from_acceptor;
use crate::tls::ssl::SslAcceptor;

#[async_trait]
impl<S: AsyncRead + AsyncWrite + Send + Unpin> ResumableAccept for TlsStream<S> {
    async fn start_accept(mut self: Pin<&mut Self>) -> Result<bool> {
        // safety: &mut self
        let ssl_mut = unsafe { ext::ssl_mut(self.get_ssl().unwrap()) };
        ext::suspend_when_need_ssl_cert(ssl_mut);
        let res = self.accept().await;

        match res {
            Ok(()) => Ok(true),
            Err(e) => {
                if e.etype == TLSWantX509Lookup {
                    Ok(false)
                } else {
                    Err(e)
                }
            }
        }
    }

    async fn resume_accept(mut self: Pin<&mut Self>) -> Result<()> {
        // safety: &mut ssl
        let ssl_mut = unsafe { ext::ssl_mut(self.get_ssl().unwrap()) };
        ext::unblock_ssl_cert(ssl_mut);
        self.accept().await
    }
}

fn prepare_tls_stream<S: IO>(acceptor: &Acceptor, io: S) -> Result<TlsStream<S>> {
    let ssl_acceptor = acceptor.inner().downcast_ref::<SslAcceptor>().unwrap();
    let ssl = ssl_from_acceptor(ssl_acceptor)
        .explain_err(TLSHandshakeFailure, |e| format!("ssl_acceptor error: {e}"))?;
    TlsStream::new(ssl, io).explain_err(TLSHandshakeFailure, |e| format!("tls stream error: {e}"))
}

/// Perform TLS handshake for the given connection with the given configuration
pub async fn handshake(
    acceptor: &Acceptor,
    io: Box<dyn IO + Send>,
) -> Result<TlsStream<Box<dyn IO + Send>>> {
    let mut stream = prepare_tls_stream(acceptor, io)?;
    stream
        .accept()
        .await
        .explain_err(TLSHandshakeFailure, |e| format!("TLS accept() failed: {e}"))?;
    Ok(stream)
}

/// Perform TLS handshake for the given connection with the given configuration and callbacks
pub async fn handshake_with_callback(
    acceptor: &Acceptor,
    io: Box<dyn IO + Send>,
    callbacks: &TlsAcceptCallbacks,
) -> pingora_error::Result<TlsStream<Box<dyn IO + Send>>> {
    let mut tls_stream = prepare_tls_stream(acceptor, io)?;
    let done = Pin::new(&mut tls_stream).start_accept().await?;
    if !done {
        // safety: we do hold a mut ref of tls_stream
        let ssl_mut = unsafe { ext::ssl_mut(tls_stream.0.ssl()) };
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

#[tokio::test]
async fn test_async_cert() {
    use crate::listeners::tls::TlsSettings;
    use crate::listeners::TlsAccept;
    use crate::protocols::tls::server::TlsAcceptCallbacks;
    use crate::tls::ssl;
    use crate::tls::ssl::SslRef;
    use tokio::io::AsyncReadExt;

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
        ssl.set_verify(ssl::SslVerifyMode::NONE); // we don't have a valid cert
        let mut stream = TlsStream::new(ssl, client).unwrap();
        Pin::new(&mut stream).connect().await.unwrap();
        let mut buf = [0; 1];
        let _ = stream.read(&mut buf).await;
    });

    let acceptor = TlsSettings::with_callbacks(cb).unwrap().build();
    acceptor.handshake(Box::new(server)).await.unwrap();
}
