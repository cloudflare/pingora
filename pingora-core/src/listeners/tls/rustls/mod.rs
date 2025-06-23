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

use std::sync::Arc;

use crate::listeners::TlsAcceptCallbacks;
use crate::protocols::tls::{server::handshake, server::handshake_with_callback, TlsStream};
use crate::protocols::{ALPN, IO};

use log::debug;
use pingora_error::ErrorType::InternalError;
use pingora_error::{Error, OrErr, Result};
use pingora_rustls::{crypto_provider, load_certs_and_key_files, CertifiedKey};
use pingora_rustls::{version, TlsAcceptor as RusTlsAcceptor};
use pingora_rustls::{ResolvesServerCert, ResolvesServerCertUsingSni, ServerConfig};

/// The TLS settings of a listening endpoint
pub struct TlsSettings {
    alpn_protocols: Option<Vec<Vec<u8>>>,
    cert_and_key: CertAndKey,
}

pub enum CertAndKey {
    Single { cert_path: String, key_path: String },
    Bundle(Vec<BundleCert>),
    Custom(Arc<dyn ResolvesServerCert>),
}

pub struct BundleCert {
    pub sni: String,
    pub cert_path: String,
    pub key_path: String,
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
        let mut config = match &self.cert_and_key {
            CertAndKey::Single {
                cert_path,
                key_path,
            } => Self::build_single(cert_path, key_path),
            CertAndKey::Bundle(bundle) => Self::build_bundled(bundle),
            CertAndKey::Custom(resolver) => Self::build_custom(resolver.clone()),
        };

        if let Some(alpn_protocols) = self.alpn_protocols {
            config.alpn_protocols = alpn_protocols;
        }

        Acceptor {
            acceptor: RusTlsAcceptor::from(Arc::new(config)),
            callbacks: None,
        }
    }

    fn build_bundled(bundle: &[BundleCert]) -> ServerConfig {
        let crypto_provider = crypto_provider();

        let mut resolver = ResolvesServerCertUsingSni::new();
        for cert_key in bundle {
            let Ok(Some((certs, key))) =
                load_certs_and_key_files(&cert_key.cert_path, &cert_key.key_path)
            else {
                panic!(
                    "Failed to load provided certificates \"{}\" or key \"{}\" for SNI \"{}\".",
                    cert_key.cert_path, cert_key.key_path, cert_key.sni
                )
            };

            let Ok(ck) = CertifiedKey::from_der(certs, key, crypto_provider) else {
                panic!(
                    "Failed to build CertifiedKey from \"{}\" for SNI \"{}\".",
                    cert_key.key_path, cert_key.sni
                )
            };

            if let Err(err) = resolver.add(&cert_key.sni, ck) {
                panic!(
                    "SNI \"{}\" invalid for cert \"{}\" and key \"{}\": {:?}",
                    cert_key.sni, cert_key.cert_path, cert_key.key_path, err
                )
            }
        }

        // TODO - Add support for client auth & custom CA support
        ServerConfig::builder_with_protocol_versions(&[&version::TLS12, &version::TLS13])
            .with_no_client_auth()
            .with_cert_resolver(Arc::new(resolver))
    }

    fn build_single(cert_path: &str, key_path: &str) -> ServerConfig {
        let Ok(Some((certs, key))) = load_certs_and_key_files(cert_path, key_path) else {
            panic!(
                "Failed to load provided certificates \"{}\" or key \"{}\".",
                cert_path, key_path
            )
        };

        ServerConfig::builder_with_protocol_versions(&[&version::TLS12, &version::TLS13])
            .with_no_client_auth()
            .with_single_cert(certs, key)
            .explain_err(InternalError, |e| {
                format!("Failed to create server listener config: {e}")
            })
            .unwrap()
    }

    fn build_custom(resolver: Arc<dyn ResolvesServerCert>) -> ServerConfig {
        // TODO - Add support for client auth & custom CA support
        ServerConfig::builder_with_protocol_versions(&[&version::TLS12, &version::TLS13])
            .with_no_client_auth()
            .with_cert_resolver(resolver)
    }

    /// Enable HTTP/2 support for this endpoint, which is default off.
    /// This effectively sets the ALPN to prefer HTTP/2 with HTTP/1.1 allowed
    pub fn enable_h2(&mut self) {
        self.set_alpn(ALPN::H2H1);
    }

    fn set_alpn(&mut self, alpn: ALPN) {
        self.alpn_protocols = Some(alpn.to_wire_protocols());
    }

    pub fn intermediate(cert_path: &str, key_path: &str) -> Result<Self>
    where
        Self: Sized,
    {
        Ok(TlsSettings {
            alpn_protocols: None,
            cert_and_key: CertAndKey::Single {
                cert_path: cert_path.to_string(),
                key_path: key_path.to_string(),
            },
        })
    }

    pub fn intermediate_bundle(bundle: Vec<BundleCert>) -> Result<Self>
    where
        Self: Sized,
    {
        Ok(TlsSettings {
            alpn_protocols: None,
            cert_and_key: CertAndKey::Bundle(bundle),
        })
    }

    pub fn intermediate_custom(resolver: Arc<dyn ResolvesServerCert>) -> Result<Self>
    where
        Self: Sized,
    {
        Ok(TlsSettings {
            alpn_protocols: None,
            cert_and_key: CertAndKey::Custom(resolver),
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
