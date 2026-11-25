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

use crate::listeners::TlsAccept;
use crate::protocols::tls::rustls::TlsStream;
use crate::protocols::{Ssl, IO};
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
    callbacks: &(dyn TlsAccept + Send + Sync),
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
    let extension = match tls_stream.get_ssl() {
        Some(tls_ref) => callbacks.handshake_complete_callback(tls_ref).await,
        None => None,
    };
    if let Some(extension) = extension {
        if let Some(digest_mut) = tls_stream.ssl_digest_mut() {
            digest_mut.extension.set(extension);
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
            rustls::crypto::ring::default_provider()
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

    #[derive(Debug)]
    struct NegotiatedTls {
        group: Option<rustls::NamedGroup>,
        cipher: String,
    }

    struct GroupCallback;

    #[async_trait]
    impl TlsAccept for GroupCallback {
        async fn handshake_complete_callback(
            &self,
            tls: &TlsRef,
        ) -> Option<Arc<dyn std::any::Any + Send + Sync>> {
            Some(Arc::new(NegotiatedTls {
                group: tls.negotiated_key_exchange_group(),
                cipher: tls.current_cipher_name()?.to_owned(),
            }))
        }
    }

    fn group_acceptor() -> crate::listeners::tls::Acceptor {
        let cert = format!("{}/tests/keys/server.crt", env!("CARGO_MANIFEST_DIR"));
        let key = format!("{}/tests/keys/key.pem", env!("CARGO_MANIFEST_DIR"));
        let mut settings = TlsSettings::with_callbacks(Box::new(GroupCallback)).unwrap();
        settings.set_certificate_chain_file(&cert).unwrap();
        settings.set_private_key_file(&key).unwrap();
        settings.build()
    }

    fn group_connector(
        group: &'static dyn rustls::crypto::SupportedKxGroup,
        version: &'static rustls::SupportedProtocolVersion,
    ) -> pingora_rustls::TlsConnector {
        let mut provider = rustls::crypto::ring::default_provider();
        provider.kx_groups = vec![group];
        let config = ClientConfig::builder_with_provider(Arc::new(provider))
            .with_protocol_versions(&[version])
            .unwrap()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(NoVerify))
            .with_no_client_auth();
        pingora_rustls::TlsConnector::from(Arc::new(config))
    }

    async fn group_handshake(
        acceptor: &crate::listeners::tls::Acceptor,
        connector: &pingora_rustls::TlsConnector,
    ) -> (
        super::TlsStream<DuplexStream>,
        super::TlsStream<DuplexStream>,
    ) {
        let (client, server) = tokio::io::duplex(4096);
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            let (client, server) = tokio::join!(
                crate::protocols::tls::rustls::client::handshake(
                    connector,
                    "openrusty.org",
                    client
                ),
                acceptor.tls_handshake(server),
            );
            (client.unwrap(), server.unwrap())
        })
        .await
        .expect("TLS handshake timed out")
    }

    #[tokio::test]
    async fn test_negotiated_group_digest_extension() {
        use crate::protocols::Ssl;
        use rustls::crypto::ring::kx_group::{SECP256R1, X25519};

        let acceptor = group_acceptor();
        let mut digests = Vec::new();
        for version in [&rustls::version::TLS12, &rustls::version::TLS13] {
            for group in [X25519, SECP256R1] {
                let connector = group_connector(group, version);
                let (client, server) = group_handshake(&acceptor, &connector).await;
                assert_eq!(
                    client.get_ssl().unwrap().negotiated_key_exchange_group(),
                    Some(group.name())
                );
                let digest = server.ssl_digest().unwrap();
                let metadata = digest.extension.get::<NegotiatedTls>().unwrap();
                assert_eq!(metadata.group, Some(group.name()));
                assert_eq!(metadata.cipher, digest.cipher);
                assert_eq!(client.ssl_digest().unwrap().version, digest.version);
                digests.push((digest, group.name()));
            }
        }
        // Retained digests must keep their own connection's metadata.
        for (digest, group) in digests {
            assert_eq!(
                digest.extension.get::<NegotiatedTls>().unwrap().group,
                Some(group)
            );
        }
    }

    #[tokio::test]
    async fn test_negotiated_group_tls12_resumption() {
        use crate::protocols::Ssl;

        let acceptor = group_acceptor();
        let connector = group_connector(
            rustls::crypto::ring::kx_group::X25519,
            &rustls::version::TLS12,
        );
        let (client, server) = group_handshake(&acceptor, &connector).await;
        let original = server.ssl_digest().unwrap();
        assert_eq!(
            original.extension.get::<NegotiatedTls>().unwrap().group,
            Some(rustls::NamedGroup::X25519)
        );
        drop((client, server));

        let (client, server) = group_handshake(&acceptor, &connector).await;
        assert_eq!(
            client.stream.as_ref().unwrap().get_ref().1.handshake_kind(),
            Some(rustls::HandshakeKind::Resumed)
        );
        assert_eq!(
            client.get_ssl().unwrap().negotiated_key_exchange_group(),
            None
        );
        let digest = server.ssl_digest().unwrap();
        let metadata = digest.extension.get::<NegotiatedTls>().unwrap();
        assert_eq!(metadata.group, None);
        assert_eq!(metadata.cipher, digest.cipher);
        assert_eq!(
            original.extension.get::<NegotiatedTls>().unwrap().group,
            Some(rustls::NamedGroup::X25519)
        );
    }
}
