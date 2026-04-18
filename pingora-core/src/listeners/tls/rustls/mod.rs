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

use std::sync::Arc;

use crate::listeners::TlsAcceptCallbacks;
use crate::protocols::tls::{
    server::handshake, server::handshake_with_callback, verify_cert_key_match, TlsStream,
};
use log::debug;
use pingora_error::{Error, ErrorType::InvalidCert, Result};
use pingora_rustls::load_certs_and_key_files;
use pingora_rustls::ClientCertVerifier;
use pingora_rustls::ServerConfig;
use pingora_rustls::{version, TlsAcceptor as RusTlsAcceptor};

use crate::protocols::{ALPN, IO};

/// The TLS settings of a listening endpoint
pub struct TlsSettings {
    alpn_protocols: Option<Vec<Vec<u8>>>,
    cert_path: String,
    key_path: String,
    client_cert_verifier: Option<Arc<dyn ClientCertVerifier>>,
    callbacks: Option<TlsAcceptCallbacks>,
}

pub struct Acceptor {
    pub acceptor: RusTlsAcceptor,
    callbacks: Option<TlsAcceptCallbacks>,
}

impl TlsSettings {
    /// Create a Rustls acceptor based on the current setting for certificates,
    /// keys, and protocols.
    ///
    /// _NOTE_ This function will panic if there is an error in loading
    /// certificate files or constructing the builder
    ///
    /// Todo: Return a result instead of panicking XD
    pub fn build(self) -> Acceptor {
        assert!(
            !self.cert_path.is_empty() && !self.key_path.is_empty(),
            "Certificate and key paths must be set before calling build(). \
             When using with_callbacks(), call set_certificate_chain_file() \
             and set_private_key_file() first."
        );

        let Ok(Some((certs, key))) = load_certs_and_key_files(&self.cert_path, &self.key_path)
        else {
            panic!(
                "Failed to load provided certificates \"{}\" or key \"{}\".",
                self.cert_path, self.key_path
            )
        };

        let builder =
            ServerConfig::builder_with_protocol_versions(&[&version::TLS12, &version::TLS13]);
        let provider = builder.crypto_provider().clone();
        let builder = if let Some(verifier) = self.client_cert_verifier {
            builder.with_client_cert_verifier(verifier)
        } else {
            builder.with_no_client_auth()
        };

        // Bypass with_single_cert() because its webpki policy validation rejects
        // certificates with unrecognized critical extensions; keep the key match check.
        let signing_key = provider
            .key_provider
            .load_private_key(key)
            .expect("Failed to load server private key");
        let leaf = certs
            .first()
            .expect("load_certs_and_key_files returned an empty cert chain");
        verify_cert_key_match(leaf, signing_key.as_ref())
            .expect("Server certificate and private key do not match");
        let certified_key = Arc::new(pingora_rustls::sign::CertifiedKey::new(certs, signing_key));

        use pingora_rustls::ResolvesServerCert;
        #[derive(Debug)]
        struct SingleCert(Arc<pingora_rustls::sign::CertifiedKey>);
        impl ResolvesServerCert for SingleCert {
            fn resolve(
                &self,
                _client_hello: pingora_rustls::ClientHello<'_>,
            ) -> Option<Arc<pingora_rustls::sign::CertifiedKey>> {
                Some(Arc::clone(&self.0))
            }
        }

        let mut config = builder.with_cert_resolver(Arc::new(SingleCert(certified_key)));

        if let Some(alpn_protocols) = self.alpn_protocols {
            config.alpn_protocols = alpn_protocols;
        }

        Acceptor {
            acceptor: RusTlsAcceptor::from(Arc::new(config)),
            callbacks: self.callbacks,
        }
    }

    /// Enable HTTP/2 support for this endpoint, which is default off.
    /// This effectively sets the ALPN to prefer HTTP/2 with HTTP/1.1 allowed
    pub fn enable_h2(&mut self) {
        self.set_alpn(ALPN::H2H1);
    }

    pub fn set_alpn(&mut self, alpn: ALPN) {
        self.alpn_protocols = Some(alpn.to_wire_protocols());
    }

    /// Configure mTLS by providing a rustls client certificate verifier.
    pub fn set_client_cert_verifier(&mut self, verifier: Arc<dyn ClientCertVerifier>) {
        self.client_cert_verifier = Some(verifier);
    }

    /// Set the path to the certificate chain file (PEM format).
    ///
    /// Returns an error if the file cannot be opened or contains no X.509
    /// certificate, matching the OpenSSL backend's behavior.
    pub fn set_certificate_chain_file(&mut self, path: &str) -> Result<()> {
        let path_str = path.to_string();
        let bytes = pingora_rustls::load_pem_file_ca(&path_str)?;
        if bytes.is_empty() {
            return Error::e_explain(InvalidCert, format!("No X.509 certificate found in {path}"));
        }
        self.cert_path = path_str;
        Ok(())
    }

    /// Set the path to the private key file (PEM format).
    ///
    /// Returns an error if the file cannot be opened or contains no private
    /// key, matching the OpenSSL backend's behavior.
    pub fn set_private_key_file(&mut self, path: &str) -> Result<()> {
        let path_str = path.to_string();
        let bytes = pingora_rustls::load_pem_file_private_key(&path_str)?;
        if bytes.is_empty() {
            return Error::e_explain(InvalidCert, format!("No private key found in {path}"));
        }
        self.key_path = path_str;
        Ok(())
    }

    pub fn intermediate(cert_path: &str, key_path: &str) -> Result<Self>
    where
        Self: Sized,
    {
        Ok(TlsSettings {
            alpn_protocols: None,
            cert_path: cert_path.to_string(),
            key_path: key_path.to_string(),
            client_cert_verifier: None,
            callbacks: None,
        })
    }

    /// Create a new [`TlsSettings`] with post-handshake callbacks.
    ///
    /// The provided callbacks will be invoked after TLS handshake completes.
    /// The [`TlsRef`](crate::protocols::tls::TlsRef) passed to the callback
    /// provides access to the peer certificate chain and negotiated cipher suite.
    ///
    /// Certificate and key files must be set separately via the builder methods
    /// on the returned `TlsSettings`.
    pub fn with_callbacks(callbacks: TlsAcceptCallbacks) -> Result<Self>
    where
        Self: Sized,
    {
        Ok(TlsSettings {
            alpn_protocols: None,
            cert_path: String::new(),
            key_path: String::new(),
            client_cert_verifier: None,
            callbacks: Some(callbacks),
        })
    }
}

impl Acceptor {
    pub async fn tls_handshake<S: IO>(&self, stream: S) -> Result<TlsStream<S>> {
        debug!("new tls session");
        // TODO: be able to offload this handshake in a thread pool
        if let Some(cb) = self.callbacks.as_ref() {
            handshake_with_callback(self, stream, cb).await
        } else {
            handshake(self, stream).await
        }
    }
}
