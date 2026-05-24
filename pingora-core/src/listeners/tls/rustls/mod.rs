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
use crate::protocols::tls::{server::handshake, server::handshake_with_callback, TlsStream};
use log::debug;
use pingora_error::ErrorType::InternalError;
use pingora_error::{Error, OrErr, Result};
use pingora_rustls::load_certs_and_key_files;
use pingora_rustls::ClientCertVerifier;
use pingora_rustls::ServerConfig;
use pingora_rustls::{
    version, CertificateHashProvider, CryptoProvider, ResolvesServerCert, SupportedCipherSuite,
    SupportedKxGroup, SupportedProtocolVersion, TlsAcceptor as RusTlsAcceptor,
};

use crate::protocols::{ALPN, IO};

static TLS12_AND_13: [&SupportedProtocolVersion; 2] = [&version::TLS12, &version::TLS13];
static TLS13_ONLY: [&SupportedProtocolVersion; 1] = [&version::TLS13];

/// The TLS settings of a listening endpoint
pub struct TlsSettings {
    alpn_protocols: Option<Vec<Vec<u8>>>,
    protocol_versions: &'static [&'static SupportedProtocolVersion],
    crypto_provider: Option<CryptoProvider>,
    cert_path: String,
    key_path: String,
    client_cert_verifier: Option<Arc<dyn ClientCertVerifier>>,
    cert_resolver: Option<Arc<dyn ResolvesServerCert>>,
    require_fips: bool,
}

pub struct Acceptor {
    pub acceptor: RusTlsAcceptor,
    pub(crate) cert_digest_hash: Option<&'static CertificateHashProvider>,
    callbacks: Option<TlsAcceptCallbacks>,
}

impl TlsSettings {
    /// Create a Rustls acceptor based on the current setting for certificates,
    /// keys, and protocols.
    ///
    /// _NOTE_ This function will panic if there is an error in loading
    /// certificate files or constructing the builder
    ///
    /// Use [`Self::try_build`] to receive these failures as a [`Result`].
    pub fn build(self) -> Acceptor {
        self.try_build().unwrap()
    }

    /// Create a Rustls acceptor and return configuration failures as errors.
    pub fn try_build(self) -> Result<Acceptor> {
        let has_cert_resolver = self.cert_resolver.is_some();
        // rustls 0.23+ requires an explicit CryptoProvider.
        let builder = if let Some(crypto_provider) = self.crypto_provider {
            ServerConfig::builder_with_provider(Arc::new(crypto_provider))
                .with_protocol_versions(self.protocol_versions)
        } else {
            pingora_rustls::install_default_crypto_provider();
            Ok(ServerConfig::builder_with_protocol_versions(
                self.protocol_versions,
            ))
        }
        .explain_err(InternalError, |e| {
            format!("Failed to create server listener config builder: {e}")
        })?;

        let builder = if let Some(verifier) = self.client_cert_verifier {
            builder.with_client_cert_verifier(verifier)
        } else {
            builder.with_no_client_auth()
        };
        let mut config = if let Some(cert_resolver) = self.cert_resolver {
            builder.with_cert_resolver(cert_resolver)
        } else {
            let (certs, key) = load_certs_and_key_files(&self.cert_path, &self.key_path)?
                .ok_or_else(|| {
                    Error::explain(
                        InternalError,
                        format!(
                            "Failed to load provided certificates \"{}\" or key \"{}\".",
                            self.cert_path, self.key_path
                        ),
                    )
                })?;

            builder
                .with_single_cert(certs, key)
                .explain_err(InternalError, |e| {
                    format!("Failed to create server listener config: {e}")
                })?
        };

        if let Some(alpn_protocols) = self.alpn_protocols {
            config.alpn_protocols = alpn_protocols;
        }
        let cert_digest_hash =
            pingora_rustls::sha256_hash_provider(config.crypto_provider().as_ref());
        if self.require_fips && !config.fips() {
            return Error::e_explain(
                InternalError,
                format!(
                    "Rustls listener configuration does not report FIPS mode; cert_path=\"{}\" key_path=\"{}\" cert_resolver={}",
                    self.cert_path, self.key_path, has_cert_resolver
                ),
            );
        }
        if self.require_fips && cert_digest_hash.is_none() {
            return Error::e_explain(
                InternalError,
                "Rustls listener configuration does not provide a SHA-256 hash provider for certificate digests.",
            );
        }

        Ok(Acceptor {
            acceptor: RusTlsAcceptor::from(Arc::new(config)),
            cert_digest_hash,
            callbacks: None,
        })
    }

    /// Enable HTTP/2 support for this endpoint, which is default off.
    /// This effectively sets the ALPN to prefer HTTP/2 with HTTP/1.1 allowed
    pub fn enable_h2(&mut self) {
        self.set_alpn(ALPN::H2H1);
    }

    pub fn set_alpn(&mut self, alpn: ALPN) {
        self.alpn_protocols = Some(alpn.to_wire_protocols());
    }

    /// Configure ALPN protocols directly.
    pub fn set_alpn_protocols(&mut self, protocols: Vec<Vec<u8>>) {
        self.alpn_protocols = Some(protocols);
    }

    /// Require at least TLS 1.2 for this endpoint.
    pub fn set_min_protocol_tls12(&mut self) {
        self.protocol_versions = &TLS12_AND_13;
    }

    /// Require at least TLS 1.3 for this endpoint.
    pub fn set_min_protocol_tls13(&mut self) {
        self.protocol_versions = &TLS13_ONLY;
    }

    /// Set the rustls crypto provider for this endpoint.
    pub fn set_crypto_provider(&mut self, crypto_provider: CryptoProvider) {
        self.crypto_provider = Some(crypto_provider);
    }

    /// Set the supported cipher suites for this endpoint.
    pub fn set_cipher_suites(&mut self, cipher_suites: Vec<SupportedCipherSuite>) {
        self.crypto_provider
            .get_or_insert_with(pingora_rustls::ring_default_crypto_provider)
            .cipher_suites = cipher_suites;
    }

    /// Set the supported key exchange groups for this endpoint.
    pub fn set_kx_groups(&mut self, kx_groups: Vec<&'static dyn SupportedKxGroup>) {
        self.crypto_provider
            .get_or_insert_with(pingora_rustls::ring_default_crypto_provider)
            .kx_groups = kx_groups;
    }

    /// Configure mTLS by providing a rustls client certificate verifier.
    pub fn set_client_cert_verifier(&mut self, verifier: Arc<dyn ClientCertVerifier>) {
        self.client_cert_verifier = Some(verifier);
    }

    /// Require rustls to report FIPS mode for the generated server config.
    pub fn set_require_fips(&mut self, require_fips: bool) {
        self.require_fips = require_fips;
    }

    pub fn intermediate(cert_path: &str, key_path: &str) -> Result<Self>
    where
        Self: Sized,
    {
        Ok(TlsSettings {
            alpn_protocols: None,
            protocol_versions: &TLS12_AND_13,
            crypto_provider: None,
            cert_path: cert_path.to_string(),
            key_path: key_path.to_string(),
            client_cert_verifier: None,
            cert_resolver: None,
            require_fips: false,
        })
    }

    pub fn with_cert_resolver(cert_resolver: Arc<dyn ResolvesServerCert>) -> Result<Self>
    where
        Self: Sized,
    {
        Ok(TlsSettings {
            alpn_protocols: None,
            protocol_versions: &TLS12_AND_13,
            crypto_provider: None,
            cert_path: String::new(),
            key_path: String::new(),
            client_cert_verifier: None,
            cert_resolver: Some(cert_resolver),
            require_fips: false,
        })
    }

    pub fn with_callbacks() -> Result<Self>
    where
        Self: Sized,
    {
        // TODO: verify if/how callback in handshake can be done using Rustls
        Error::e_explain(
            InternalError,
            "Certificate callbacks are not supported with feature \"rustls\".",
        )
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn try_build_returns_error_when_required_fips_is_not_reported() {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let cert_path = format!("{manifest_dir}/tests/keys/server.crt");
        let key_path = format!("{manifest_dir}/tests/keys/key.pem");
        let mut settings = TlsSettings::intermediate(&cert_path, &key_path).unwrap();
        settings.set_crypto_provider(pingora_rustls::ring_default_crypto_provider());
        settings.set_require_fips(true);

        match settings.try_build() {
            Ok(_) => panic!("expected non-FIPS rustls listener config to return an error"),
            Err(error) => {
                assert_eq!(error.etype(), &InternalError);
                assert!(error
                    .context
                    .as_ref()
                    .map(|context| context.as_str())
                    .unwrap_or_default()
                    .contains("does not report FIPS mode"));
            }
        }
    }
}
