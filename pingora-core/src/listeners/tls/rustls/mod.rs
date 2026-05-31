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

use std::{collections::HashMap, sync::Arc};

use crate::listeners::TlsAcceptCallbacks;
use crate::protocols::tls::{server::handshake, server::handshake_with_callback, TlsStream};
use arc_swap::ArcSwap;
use log::debug;
use pingora_error::ErrorType::InternalError;
use pingora_error::{Error, OrErr, Result};
use pingora_rustls::load_certs_and_key_files;
use pingora_rustls::ClientCertVerifier;
use pingora_rustls::ServerConfig;
use pingora_rustls::{version, TlsAcceptor as RusTlsAcceptor};
use pingora_rustls::{CertifiedKey, ClientHello, ResolvesServerCert};

use crate::protocols::{ALPN, IO};

/// The TLS settings of a listening endpoint
pub struct TlsSettings {
    alpn_protocols: Option<Vec<Vec<u8>>>,
    cert_and_key: CertAndKey,
    client_cert_verifier: Option<Arc<dyn ClientCertVerifier>>,
}

enum CertAndKey {
    SingleStatic {
        cert_path: String,
        key_path: String,
    },
    MultipleDynamic {
        storage: Arc<ArcSwap<HashMap<String, Arc<CertifiedKey>>>>,
    },
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
        // rustls 0.23+ requires an explicit CryptoProvider.
        pingora_rustls::install_default_crypto_provider();

        let builder =
            ServerConfig::builder_with_protocol_versions(&[&version::TLS12, &version::TLS13]);
        let builder = if let Some(verifier) = self.client_cert_verifier {
            builder.with_client_cert_verifier(verifier)
        } else {
            builder.with_no_client_auth()
        };

        let mut config = match self.cert_and_key {
            CertAndKey::SingleStatic {
                cert_path,
                key_path,
            } => {
                let Ok(Some((certs, key))) = load_certs_and_key_files(&cert_path, &key_path) else {
                    panic!(
                        "Failed to load provided certificates \"{}\" or key \"{}\".",
                        cert_path, key_path
                    )
                };

                builder
                    .with_single_cert(certs, key)
                    .explain_err(InternalError, |e| {
                        format!("Failed to create server listener config: {e}")
                    })
                    .unwrap()
            }
            CertAndKey::MultipleDynamic { storage } => {
                builder.with_cert_resolver(Arc::new(MultipleDynamicResolvesServerCert { storage }))
            }
        };

        if let Some(alpn_protocols) = self.alpn_protocols {
            config.alpn_protocols = alpn_protocols;
        }

        Acceptor {
            acceptor: RusTlsAcceptor::from(Arc::new(config)),
            callbacks: None,
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

    pub fn intermediate(cert_path: &str, key_path: &str) -> Result<Self>
    where
        Self: Sized,
    {
        Ok(TlsSettings {
            alpn_protocols: None,
            cert_and_key: CertAndKey::SingleStatic {
                cert_path: cert_path.to_string(),
                key_path: key_path.to_string(),
            },
            client_cert_verifier: None,
        })
    }

    pub fn intermediate_with_multiple(
        storage: Arc<ArcSwap<HashMap<String, Arc<CertifiedKey>>>>,
    ) -> Result<Self>
    where
        Self: Sized,
    {
        Ok(TlsSettings {
            alpn_protocols: None,
            cert_and_key: CertAndKey::MultipleDynamic { storage },
            client_cert_verifier: None,
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

#[derive(Debug)]
struct MultipleDynamicResolvesServerCert {
    storage: Arc<ArcSwap<HashMap<String, Arc<CertifiedKey>>>>,
}

impl MultipleDynamicResolvesServerCert {
    fn resolve(&self, server_name: &str) -> Option<Arc<CertifiedKey>> {
        let storage = self.storage.load();

        if let Some(x) = storage.get(server_name) {
            return Some(Arc::clone(x));
        }

        for (name, x) in storage.iter() {
            if name.starts_with("*.")
                && server_name.ends_with(&name[1..])
                && !server_name.replace(&name[1..], "").contains('.')
            {
                return Some(Arc::clone(x));
            }
        }

        None
    }
}

impl ResolvesServerCert for MultipleDynamicResolvesServerCert {
    fn resolve(&self, client_hello: ClientHello) -> Option<Arc<CertifiedKey>> {
        let Some(server_name) = client_hello.server_name() else {
            return None;
        };

        self.resolve(server_name)
    }
}

#[cfg(test)]
mod tests {

    use super::*;

    #[test]
    fn test_multiple_dynamic_resolves_server_cert() -> Result<(), Box<dyn core::error::Error>> {
        use std::fs;

        use pingora_rustls::{CertificateDer, CryptoProvider, PemObject as _, PrivateKeyDer};

        pingora_rustls::install_default_crypto_provider();
        let crypto_provider = CryptoProvider::get_default().expect("Never");

        let cert_path = format!("{}/tests/keys/server.crt", env!("CARGO_MANIFEST_DIR"));
        let key_path = format!("{}/tests/keys/key.pem", env!("CARGO_MANIFEST_DIR"));

        let cert_str = fs::read_to_string(&cert_path)?;
        let key_str = fs::read_to_string(&key_path)?;

        let mut storage: HashMap<String, Arc<CertifiedKey>> = HashMap::new();

        storage.insert(
            "openrusty.org".into(),
            Arc::new({
                let mut certified_key = CertifiedKey::from_der(
                    vec![CertificateDer::from_pem_slice(cert_str.as_bytes()).unwrap()],
                    PrivateKeyDer::from_pem_slice(key_str.as_bytes()).unwrap(),
                    &crypto_provider,
                )?;
                certified_key.ocsp = Some("FakeDataA".as_bytes().into());
                certified_key
            }),
        );

        storage.insert(
            "*.openrusty.org".into(),
            Arc::new({
                let mut certified_key = CertifiedKey::from_der(
                    vec![CertificateDer::from_pem_slice(cert_str.as_bytes()).unwrap()],
                    PrivateKeyDer::from_pem_slice(key_str.as_bytes()).unwrap(),
                    &crypto_provider,
                )?;
                certified_key.ocsp = Some("FakeDataB".as_bytes().into());
                certified_key
            }),
        );

        let storage = Arc::new(ArcSwap::new(Arc::new(storage)));

        let resolves_server_cert = MultipleDynamicResolvesServerCert { storage };

        assert_eq!(
            resolves_server_cert
                .resolve("openrusty.org")
                .and_then(|x| x.ocsp.clone()),
            Some("FakeDataA".as_bytes().into())
        );

        assert_eq!(
            resolves_server_cert
                .resolve("foo.openrusty.org")
                .and_then(|x| x.ocsp.clone()),
            Some("FakeDataB".as_bytes().into())
        );
        assert_eq!(
            resolves_server_cert
                .resolve("bar.openrusty.org")
                .and_then(|x| x.ocsp.clone()),
            Some("FakeDataB".as_bytes().into())
        );
        assert!(resolves_server_cert
            .resolve("xxx.foo.openrusty.org")
            .is_none());

        assert!(resolves_server_cert.resolve("example.com").is_none());
        assert!(resolves_server_cert.resolve("localhost").is_none());

        Ok(())
    }
}
