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

use std::sync::Arc;

use crate::listeners::TlsAcceptCallbacks;
use crate::protocols::tls::{server::handshake, server::handshake_with_callback, TlsStream};
use log::debug;
use pingora_error::ErrorType::InternalError;
use pingora_error::{Error, OrErr, Result};
use pingora_rustls::{load_certs_and_key_files, RootCertStore, WebPkiClientVerifier};
use pingora_rustls::ServerConfig;
use pingora_rustls::{version, TlsAcceptor as RusTlsAcceptor};

use crate::protocols::{ALPN, IO};

use pingora_rustls::{load_ca_file_into_store, load_crls};

/// Enhanced TLS settings with client authentication support
pub struct TlsSettings {
    alpn_protocols: Option<Vec<Vec<u8>>>,
    cert_path: String,
    key_path: String,
    // New fields for client authentication
    client_auth_mode: ClientAuthMode,
}

/// Client authentication configuration
#[derive(Clone, Debug)]
pub enum ClientAuthMode {
    /// No client authentication required
    NoClientAuth,
    /// Optional client authentication
    Optional { ca_path: String, crls: Vec<String> },
    /// Mandatory client authentication
    Required { ca_path: String, crls: Vec<String> },
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
        let Ok(Some((certs, key))) = load_certs_and_key_files(&self.cert_path, &self.key_path)
        else {
            panic!(
                "Failed to load provided certificates \"{}\" or key \"{}\".",
                self.cert_path, self.key_path
            )
        };

        let client_verifier = match &self.client_auth_mode {
            ClientAuthMode::NoClientAuth => WebPkiClientVerifier::no_client_auth(),
            ClientAuthMode::Optional { ca_path, crls }
            | ClientAuthMode::Required { ca_path, crls } => {
                let mut roots = RootCertStore::empty();
                 load_ca_file_into_store(&ca_path, &mut roots)
                    .explain_err(InternalError, |e| format!("Failed to load CA file: {e}"))
                    .unwrap();

                let crls = load_crls(&crls)
                    .explain_err(InternalError, |e| format!("Failed to load CRLs: {e}"))
                    .unwrap();

                let builder = WebPkiClientVerifier::builder(Arc::new(roots)).with_crls(crls);

                if matches!(self.client_auth_mode, ClientAuthMode::Optional { .. }) {
                    builder.allow_unauthenticated()
                } else {
                    builder
                }
                .build()
                .explain_err(InternalError, |e| {
                    format!("Failed to create client verifier: {e}")
                })
                .unwrap()
            }
        };

        let mut config =
            ServerConfig::builder_with_protocol_versions(&[&version::TLS12, &version::TLS13])
                .with_client_cert_verifier(client_verifier)
                .with_single_cert(certs, key)
                .explain_err(InternalError, |e| {
                    format!("Failed to create server listener config: {e}")
                })
                .unwrap();

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

    fn set_alpn(&mut self, alpn: ALPN) {
        self.alpn_protocols = Some(alpn.to_wire_protocols());
    }

    /// Create new TLS settings with intermediate configuration
    pub fn intermediate(cert_path: &str, key_path: &str) -> Result<Self> {
        Ok(TlsSettings {
            alpn_protocols: None,
            cert_path: cert_path.to_string(),
            key_path: key_path.to_string(),
            client_auth_mode: ClientAuthMode::NoClientAuth,
        })
    }

    /// Enable client certificate authentication
    pub fn with_client_auth(mut self, mode: ClientAuthMode) -> Self {
        self.client_auth_mode = mode;
        self
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
mod test {
    use super::*;
    use std::fs::File;
    use std::io::Write;
    use tempfile::TempDir;

    #[test]
    fn test_client_auth_config() -> Result<()> {
        let temp_dir = TempDir::new().unwrap();

        // Create test certificates
        let cert_path = temp_dir.path().join("cert.pem");
        let key_path = temp_dir.path().join("key.pem");
        let ca_path = temp_dir.path().join("ca.pem");

        // Generate test certificates here...
        // For brevity, we'll just create empty files
        File::create(&cert_path).unwrap().write_all(b"").unwrap();
        File::create(&key_path).unwrap().write_all(b"").unwrap();
        File::create(&ca_path).unwrap().write_all(b"").unwrap();

        let settings =
            TlsSettings::intermediate(cert_path.to_str().unwrap(), key_path.to_str().unwrap())?
                .with_client_auth(ClientAuthMode::Required {
                    ca_path: ca_path.to_str().unwrap().to_string(),
                    crls: vec![],
                });

        assert!(matches!(
            settings.client_auth_mode,
            ClientAuthMode::Required { .. }
        ));
        Ok(())
    }
}
