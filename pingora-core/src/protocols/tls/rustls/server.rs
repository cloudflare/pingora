// Copyright 2026 Cloudflare, Inc.
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

//! Rustls TLS server specific implementation

use crate::listeners::TlsAcceptCallbacks;
use crate::protocols::tls::rustls::TlsStream;
use crate::protocols::IO;
use crate::{listeners::tls::Acceptor, protocols::Shutdown};
use async_trait::async_trait;
use log::warn;
use pingora_error::{ErrorType::*, OrErr, Result};
use std::pin::Pin;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};

impl<S: AsyncRead + AsyncWrite + Send + Unpin> TlsStream<S> {
    async fn start_accept(mut self: Pin<&mut Self>) -> Result<bool> {
        // TODO: suspend cert callback
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
        // TODO: unblock cert callback
        self.accept().await
    }
}

async fn prepare_tls_stream<S: IO>(acceptor: &Acceptor, io: S) -> Result<TlsStream<S>> {
    TlsStream::from_acceptor(acceptor, io)
        .await
        .explain_err(TLSHandshakeFailure, |e| format!("tls stream error: {e}"))
}

/// Perform TLS handshake for the given connection with the given configuration
pub async fn handshake<S: IO>(acceptor: &Acceptor, io: S) -> Result<TlsStream<S>> {
    let mut stream = prepare_tls_stream(acceptor, io).await?;
    stream
        .accept()
        .await
        .explain_err(TLSHandshakeFailure, |e| format!("TLS accept() failed: {e}"))?;
    Ok(stream)
}

/// Perform TLS handshake for the given connection with the given configuration and callbacks
pub async fn handshake_with_callback<S: IO>(
    acceptor: &Acceptor,
    io: S,
    callbacks: &TlsAcceptCallbacks,
) -> Result<TlsStream<S>> {
    let mut tls_stream = prepare_tls_stream(acceptor, io).await?;
    let done = Pin::new(&mut tls_stream).start_accept().await?;
    if !done {
        // NOTE: certificate_callback is not invoked for rustls. Dynamic cert selection
        // should use a custom ResolvesServerCert instead.
        warn!("certificate_callback is not supported with the rustls backend; use ResolvesServerCert for dynamic cert selection");
        Pin::new(&mut tls_stream)
            .resume_accept()
            .await
            .explain_err(TLSHandshakeFailure, |e| format!("TLS accept() failed: {e}"))?;
    }
    {
        // Build TlsRef with connection state for the callback
        let tls_ref = tls_stream.build_tls_ref();
        if let Some(extension) = callbacks.handshake_complete_callback(&tls_ref).await {
            if let Some(digest_mut) = tls_stream.ssl_digest_mut() {
                digest_mut.extension.set(extension);
            }
        }
    }
    Ok(tls_stream)
}

#[async_trait]
impl<S> Shutdown for TlsStream<S>
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

#[cfg(test)]
mod tests {
    use crate::listeners::tls::TlsSettings;
    use crate::listeners::TlsAccept;
    use crate::protocols::tls::TlsRef;
    use async_trait::async_trait;
    use pingora_rustls::{
        ClientConfig, HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier, ServerName,
    };
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, DuplexStream};

    #[derive(Debug)]
    struct NoVerify;

    impl ServerCertVerifier for NoVerify {
        fn verify_server_cert(
            &self,
            _: &rustls::pki_types::CertificateDer<'_>,
            _: &[rustls::pki_types::CertificateDer<'_>],
            _: &ServerName<'_>,
            _: &[u8],
            _: pingora_rustls::UnixTime,
        ) -> Result<ServerCertVerified, rustls::Error> {
            Ok(ServerCertVerified::assertion())
        }

        fn verify_tls12_signature(
            &self,
            _: &[u8],
            _: &rustls::pki_types::CertificateDer<'_>,
            _: &rustls::DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, rustls::Error> {
            Ok(HandshakeSignatureValid::assertion())
        }

        fn verify_tls13_signature(
            &self,
            _: &[u8],
            _: &rustls::pki_types::CertificateDer<'_>,
            _: &rustls::DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, rustls::Error> {
            Ok(HandshakeSignatureValid::assertion())
        }

        fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
            rustls::crypto::aws_lc_rs::default_provider()
                .signature_verification_algorithms
                .supported_schemes()
        }
    }

    async fn client_task(client: DuplexStream) {
        let config = ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(NoVerify))
            .with_no_client_auth();
        let connector = pingora_rustls::TlsConnector::from(Arc::new(config));
        let server_name = ServerName::try_from("openrusty.org").unwrap();
        let mut stream = connector.connect(server_name, client).await.unwrap();
        let mut buf = [0u8; 1];
        let _ = stream.read(&mut buf).await;
    }

    #[tokio::test]
    async fn test_handshake_complete_callback() {
        struct CipherName(String);
        struct Callback;

        #[async_trait]
        impl TlsAccept for Callback {
            async fn handshake_complete_callback(
                &self,
                tls: &TlsRef,
            ) -> Option<Arc<dyn std::any::Any + Send + Sync>> {
                let name = tls.current_cipher_name()?.to_string();
                Some(Arc::new(CipherName(name)))
            }
        }

        let cert = format!("{}/tests/keys/server.crt", env!("CARGO_MANIFEST_DIR"));
        let key = format!("{}/tests/keys/key.pem", env!("CARGO_MANIFEST_DIR"));

        let mut settings = TlsSettings::with_callbacks(Box::new(Callback)).unwrap();
        settings.set_certificate_chain_file(&cert).unwrap();
        settings.set_private_key_file(&key).unwrap();
        let acceptor = settings.build();

        let (client, server) = tokio::io::duplex(4096);
        tokio::spawn(client_task(client));

        let stream = acceptor.tls_handshake(server).await.unwrap();
        let digest = stream.ssl_digest().unwrap();
        let cipher = digest.extension.get::<CipherName>().unwrap();
        assert!(!cipher.0.is_empty());
    }
}
