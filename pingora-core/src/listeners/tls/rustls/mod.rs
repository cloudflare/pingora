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
            let rt = offload.get_runtime(stream.id() as u64);
            rt.spawn(async move {
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
