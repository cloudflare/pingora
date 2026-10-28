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

use crate::listeners::{SharedTlsAcceptCallbacks, TlsAcceptCallbacks};
use crate::offload::OffloadRuntime;
use crate::protocols::tls::{server::handshake, server::handshake_with_callback, TlsStream};
use crate::server::configuration::ServerConf;
use log::debug;
use pingora_error::ErrorType::{InternalError, InvalidCert};
use pingora_error::{Error, OrErr, Result};
use pingora_rustls::load_certs_and_key_files;
use pingora_rustls::ClientCertVerifier;
use pingora_rustls::ResolvesServerCert;
use pingora_rustls::ServerConfig;
use pingora_rustls::{version, TlsAcceptor as RusTlsAcceptor};

use crate::protocols::{ALPN, IO};

/// The TLS settings of a listening endpoint
pub struct TlsSettings {
    alpn_protocols: Option<Vec<Vec<u8>>>,
    cert_path: String,
    key_path: String,
    cert_resolver: Option<Arc<dyn ResolvesServerCert>>,
    client_cert_verifier: Option<Arc<dyn ClientCertVerifier>>,
    callbacks: Option<TlsAcceptCallbacks>,
    offload_threadpool: Option<(usize, usize)>,
}

pub struct Acceptor {
    pub acceptor: RusTlsAcceptor,
    callbacks: Option<SharedTlsAcceptCallbacks>,
    offload: Option<OffloadRuntime>,
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

        let mut config = if let Some(resolver) = self.cert_resolver {
            builder.with_cert_resolver(resolver)
        } else {
            assert!(
                !self.cert_path.is_empty() && !self.key_path.is_empty(),
                "Either set_cert_resolver() or both set_certificate_chain_file() and \
                 set_private_key_file() must be called before build()."
            );

            let Ok(Some((certs, key))) = load_certs_and_key_files(&self.cert_path, &self.key_path)
            else {
                panic!(
                    "Failed to load provided certificates \"{}\" or key \"{}\".",
                    self.cert_path, self.key_path
                )
            };

            builder
                .with_single_cert(certs, key)
                .explain_err(InternalError, |e| {
                    format!("Failed to create server listener config: {e}")
                })
                .unwrap()
        };

        if let Some(alpn_protocols) = self.alpn_protocols {
            config.alpn_protocols = alpn_protocols;
        }

        Acceptor {
            acceptor: RusTlsAcceptor::from(Arc::new(config)),
            callbacks: self.callbacks.map(SharedTlsAcceptCallbacks::from),
            offload: self.offload_threadpool.map(|(shards, threads_per_shard)| {
                OffloadRuntime::new("downstream TLS offload", shards, threads_per_shard)
            }),
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

    /// Install a user-provided server certificate resolver.
    ///
    /// When set, any certificate/key paths are ignored at `build()`. Useful for
    /// dynamic SNI-based selection, where the cert cannot be chosen ahead of the
    /// handshake.
    pub fn set_cert_resolver(&mut self, resolver: Arc<dyn ResolvesServerCert>) {
        self.cert_resolver = Some(resolver);
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

    /// Offload server-side TLS handshakes for this endpoint to dedicated
    /// single-threaded runtime pools.
    ///
    /// `shards` partitions accepted connections by connection id, and
    /// `threads_per_shard` controls how many single-threaded runtimes are
    /// available per shard. Both values must be greater than zero.
    ///
    /// # Panics
    ///
    /// Panics when either `shards` or `threads_per_shard` is zero.
    #[track_caller]
    pub fn set_offload_threadpool(&mut self, shards: usize, threads_per_shard: usize) {
        assert!(shards != 0, "shards must be greater than zero");
        assert!(
            threads_per_shard != 0,
            "threads_per_shard must be greater than zero"
        );
        self.offload_threadpool = Some((shards, threads_per_shard));
    }

    /// Offload server-side TLS handshakes using the downstream TLS offload
    /// settings in [`ServerConf`], when both values are set and non-zero.
    ///
    /// This helper lets callers wire configuration files into per-listener
    /// [`TlsSettings`]. If either configuration value is unset or zero,
    /// this method leaves handshake offload disabled.
    pub fn set_offload_threadpool_from_server_conf(&mut self, server_conf: &ServerConf) {
        if let Some((shards, threads_per_shard)) = server_conf.downstream_tls_offload_threadpool() {
            self.set_offload_threadpool(shards, threads_per_shard);
        }
    }

    pub fn intermediate(cert_path: &str, key_path: &str) -> Result<Self>
    where
        Self: Sized,
    {
        Ok(TlsSettings {
            alpn_protocols: None,
            cert_path: cert_path.to_string(),
            key_path: key_path.to_string(),
            cert_resolver: None,
            client_cert_verifier: None,
            callbacks: None,
            offload_threadpool: None,
        })
    }

    /// Create a new [`TlsSettings`] with post-handshake callbacks.
    ///
    /// Before calling `build()`, supply either a cert/key pair via
    /// `set_certificate_chain_file` + `set_private_key_file`, or a custom
    /// resolver via `set_cert_resolver`.
    pub fn with_callbacks(callbacks: TlsAcceptCallbacks) -> Result<Self>
    where
        Self: Sized,
    {
        Ok(TlsSettings {
            alpn_protocols: None,
            cert_path: String::new(),
            key_path: String::new(),
            cert_resolver: None,
            client_cert_verifier: None,
            callbacks: Some(callbacks),
            offload_threadpool: None,
        })
    }
}

impl Acceptor {
    /// Build an `Acceptor` from a runtime-constructed rustls [`ServerConfig`],
    /// rather than from certificate/key files.
    ///
    /// This supports configurations whose key material does not live in files
    /// on disk, e.g. certificates fetched from a secrets manager. The
    /// resulting `Acceptor` accepts TLS connections via
    /// [`Self::tls_handshake`] exactly as one built by [`TlsSettings::build`]
    /// with no handshake offload configured.
    pub fn from_server_config(config: Arc<ServerConfig>) -> Self {
        Self {
            acceptor: RusTlsAcceptor::from(config),
            callbacks: None,
            offload: None,
        }
    }

    pub async fn tls_handshake<S: IO + 'static>(&self, stream: S) -> Result<TlsStream<S>> {
        debug!("new tls session");
        if let Some(offload) = self.offload.as_ref() {
            // Clone without offload to prevent recursive offloading on the worker runtime.
            let acceptor = Acceptor {
                acceptor: self.acceptor.clone(),
                callbacks: None,
                offload: None,
            };
            let callbacks = self.callbacks.clone();
            offload
                .spawn_abort_on_drop(stream.id() as u64, async move {
                    if let Some(cb) = callbacks.as_ref() {
                        handshake_with_callback(&acceptor, stream, cb.as_ref()).await
                    } else {
                        handshake(&acceptor, stream).await
                    }
                })
                .await
                .or_err(InternalError, "TLS offload runtime failure")?
        } else if let Some(cb) = self.callbacks.as_ref() {
            handshake_with_callback(self, stream, cb.as_ref()).await
        } else {
            handshake(self, stream).await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocols::l4::stream::Stream as L4Stream;
    use pingora_rustls::load_certs_and_key_files;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn test_from_server_config_handshake() {
        // Build a rustls ServerConfig by hand, as a server whose key material
        // arrives in memory (e.g. from a secrets manager) would. The fixture
        // files stand in for that material here.
        pingora_rustls::install_default_crypto_provider();
        let cert_path = format!("{}/tests/keys/server.crt", env!("CARGO_MANIFEST_DIR"));
        let key_path = format!("{}/tests/keys/key.pem", env!("CARGO_MANIFEST_DIR"));
        let (certs, key) = load_certs_and_key_files(&cert_path, &key_path)
            .unwrap()
            .unwrap();
        let config =
            ServerConfig::builder_with_protocol_versions(&[&version::TLS12, &version::TLS13])
                .with_no_client_auth()
                .with_single_cert(certs, key)
                .unwrap();
        let acceptor = Acceptor::from_server_config(Arc::new(config));

        // Accept a plain TCP connection and drive the TLS handshake directly
        // through the Acceptor, with no TlsSettings (and no cert/key files)
        // involved.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            let (tcp_stream, _) = listener.accept().await.unwrap();
            let stream: L4Stream = tcp_stream.into();
            let mut tls_stream = acceptor.tls_handshake(stream).await.unwrap();
            let mut buf = [0; 1024];
            let _ = tls_stream.read(&mut buf).await.unwrap();
            tls_stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\n\r\na")
                .await
                .unwrap();
            tls_stream.flush().await.unwrap();
        });

        let client = reqwest::Client::builder()
            .danger_accept_invalid_certs(true)
            .build()
            .unwrap();
        let res = client.get(format!("https://{addr}")).send().await.unwrap();
        assert_eq!(res.status(), reqwest::StatusCode::OK);
    }
}
