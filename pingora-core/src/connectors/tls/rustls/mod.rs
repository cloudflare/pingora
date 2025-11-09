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

use log::debug;
use pingora_error::{
    Error,
    ErrorType::{ConnectTimedout, InvalidCert},
    OrErr, Result,
};
use pingora_rustls::{
    load_ca_file_into_store, load_certs_and_key_files, load_platform_certs_incl_env_into_store,
    version, CertificateDer, CertificateError, ClientConfig as RusTlsClientConfig,
    DigitallySignedStruct, KeyLogFile, PrivateKeyDer, RootCertStore, RusTlsError, ServerName,
    SignatureScheme, TlsConnector as RusTlsConnector, UnixTime, WebPkiServerVerifier,
};

// Uses custom certificate verification from rustls's 'danger' module.
use pingora_rustls::{
    HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier as RusTlsServerCertVerifier,
};

use crate::protocols::tls::{client::handshake, TlsStream};
use crate::{connectors::ConnectorOptions, listeners::ALPN, protocols::IO, upstreams::peer::Peer};

use super::replace_leftmost_underscore;

#[derive(Clone)]
pub struct Connector {
    pub ctx: Arc<TlsConnector>,
}

impl Connector {
    /// Create a new connector based on the optional configurations. If no
    /// configurations are provided, no customized certificates or keys will be
    /// used
    pub fn new(config_opt: Option<ConnectorOptions>) -> Self {
        TlsConnector::build_connector(config_opt).unwrap()
    }
}

pub struct TlsConnector {
    config: Arc<RusTlsClientConfig>,
    ca_certs: Arc<RootCertStore>,
}

impl TlsConnector {
    pub(crate) fn build_connector(options: Option<ConnectorOptions>) -> Result<Connector>
    where
        Self: Sized,
    {
        // NOTE: Rustls only supports TLS 1.2 & 1.3

        // TODO: currently using Rustls defaults
        // - support SSLKEYLOGFILE
        // - set supported ciphers/algorithms/curves
        // - add options for CRL/OCSP validation

        let (ca_certs, certs_key) = {
            let mut ca_certs = RootCertStore::empty();
            let mut certs_key = None;

            if let Some(conf) = options.as_ref() {
                if let Some(ca_file_path) = conf.ca_file.as_ref() {
                    load_ca_file_into_store(ca_file_path, &mut ca_certs)?;
                } else {
                    load_platform_certs_incl_env_into_store(&mut ca_certs)?;
                }
                if let Some((cert, key)) = conf.cert_key_file.as_ref() {
                    certs_key = load_certs_and_key_files(cert, key)?;
                }
            } else {
                load_platform_certs_incl_env_into_store(&mut ca_certs)?;
            }

            (ca_certs, certs_key)
        };

        // TODO: WebPkiServerVerifier for CRL/OCSP validation
        let builder =
            RusTlsClientConfig::builder_with_protocol_versions(&[&version::TLS12, &version::TLS13])
                .with_root_certificates(ca_certs.clone());

        let mut config = match certs_key {
            Some((certs, key)) => {
                match builder.with_client_auth_cert(certs.clone(), key.clone_key()) {
                    Ok(config) => config,
                    Err(err) => {
                        // TODO: is there a viable alternative to the panic?
                        // falling back to no client auth... does not seem to be reasonable.
                        panic!("Failed to configure client auth cert/key. Error: {}", err);
                    }
                }
            }
            None => builder.with_no_client_auth(),
        };

        // Enable SSLKEYLOGFILE support for debugging TLS traffic
        if let Some(options) = options.as_ref() {
            if options.debug_ssl_keylog {
                config.key_log = Arc::new(KeyLogFile::new());
            }
        }

        Ok(Connector {
            ctx: Arc::new(TlsConnector {
                config: Arc::new(config),
                ca_certs: Arc::new(ca_certs),
            }),
        })
    }
}

pub async fn connect<T, P>(
    stream: T,
    peer: &P,
    alpn_override: Option<ALPN>,
    tls_ctx: &TlsConnector,
) -> Result<TlsStream<T>>
where
    T: IO,
    P: Peer + Send + Sync,
{
    let config = &tls_ctx.config;

    // TODO: setup CA/verify cert store from peer
    // peer.get_ca() returns None by default. It must be replaced by the
    // implementation of `peer`
    let key_pair = peer.get_client_cert_key();
    let mut updated_config_opt: Option<RusTlsClientConfig> = match key_pair {
        None => None,
        Some(key_arc) => {
            debug!("setting client cert and key");

            let mut cert_chain = vec![];
            debug!("adding leaf certificate to mTLS cert chain");
            cert_chain.push(key_arc.leaf());

            debug!("adding intermediate certificates to mTLS cert chain");
            key_arc
                .intermediates()
                .to_owned()
                .iter()
                .copied()
                .for_each(|i| cert_chain.push(i));

            let certs: Vec<CertificateDer> = cert_chain.into_iter().map(|c| c.into()).collect();
            let private_key: PrivateKeyDer =
                key_arc.key().as_slice().to_owned().try_into().unwrap();

            let builder = RusTlsClientConfig::builder_with_protocol_versions(&[
                &version::TLS12,
                &version::TLS13,
            ])
            .with_root_certificates(Arc::clone(&tls_ctx.ca_certs));
            debug!("added root ca certificates");

            let mut updated_config = builder.with_client_auth_cert(certs, private_key).or_err(
                InvalidCert,
                "Failed to use peer cert/key to update Rustls config",
            )?;
            // Preserve keylog setting from original config
            updated_config.key_log = Arc::clone(&config.key_log);
            Some(updated_config)
        }
    };

    if let Some(alpn) = alpn_override.as_ref().or(peer.get_alpn()) {
        let alpn_protocols = alpn.to_wire_protocols();
        if let Some(updated_config) = updated_config_opt.as_mut() {
            updated_config.alpn_protocols = alpn_protocols;
        } else {
            let mut updated_config = RusTlsClientConfig::clone(config);
            updated_config.alpn_protocols = alpn_protocols;
            updated_config_opt = Some(updated_config);
        }
    }

    let mut domain = peer.sni().to_string();

    if let Some(updated_config) = updated_config_opt.as_mut() {
        let verification_mode = if peer.sni().is_empty() {
            updated_config.enable_sni = false;
            /* NOTE: technically we can still verify who signs the cert but turn it off to be
            consistent with nginx's behavior */
            Some(VerificationMode::SkipAll) // disable verification if sni does not exist
        } else if !peer.verify_cert() {
            Some(VerificationMode::SkipAll)
        } else if !peer.verify_hostname() {
            Some(VerificationMode::SkipHostname)
        } else {
            // if sni had underscores in leftmost label replace and add
            if let Some(sni_s) = replace_leftmost_underscore(peer.sni()) {
                domain = sni_s;
            }
            None
            // to use the custom verifier for the full verify:
            // Some(VerificationMode::Full)
        };

        // Builds the custom_verifier when verification_mode is set.
        if let Some(mode) = verification_mode {
            let delegate = WebPkiServerVerifier::builder(Arc::clone(&tls_ctx.ca_certs))
                .build()
                .or_err(InvalidCert, "Failed to build WebPkiServerVerifier")?;

            let custom_verifier = Arc::new(CustomServerCertVerifier::new(delegate, mode));

            updated_config
                .dangerous()
                .set_certificate_verifier(custom_verifier);
        }
    }

    // TODO: curve setup from peer
    // - second key share from peer, currently only used in boringssl with PQ features

    // Patch config for dangerous verifier if needed, but only in test builds.
    #[cfg(test)]
    if !peer.verify_cert() || !peer.verify_hostname() {
        use crate::connectors::http::rustls_no_verify::apply_no_verify;
        if let Some(cfg) = updated_config_opt.as_mut() {
            apply_no_verify(cfg);
        } else {
            let mut tmp = RusTlsClientConfig::clone(config);
            apply_no_verify(&mut tmp);
            updated_config_opt = Some(tmp);
        }
    }

    let tls_conn = if let Some(cfg) = updated_config_opt {
        RusTlsConnector::from(Arc::new(cfg))
    } else {
        RusTlsConnector::from(Arc::clone(config))
    };

    let connect_future = handshake(&tls_conn, &domain, stream);

    match peer.connection_timeout() {
        Some(t) => match pingora_timeout::timeout(t, connect_future).await {
            Ok(res) => res,
            Err(_) => Error::e_explain(
                ConnectTimedout,
                format!("connecting to server {}, timeout {:?}", peer, t),
            ),
        },
        None => connect_future.await,
    }
}

#[allow(dead_code)]
#[derive(Debug)]
pub enum VerificationMode {
    SkipHostname,
    SkipAll,
    Full,
    // Note: "Full" Included for completeness, making this verifier self-contained
    // and explicit about all possible verification modes, not just exceptions.
}

#[derive(Debug)]
pub struct CustomServerCertVerifier {
    delegate: Arc<WebPkiServerVerifier>,
    verification_mode: VerificationMode,
}

impl CustomServerCertVerifier {
    pub fn new(delegate: Arc<WebPkiServerVerifier>, verification_mode: VerificationMode) -> Self {
        Self {
            delegate,
            verification_mode,
        }
    }
}

// CustomServerCertVerifier delegates TLS signature verification and allows 3 VerificationMode:
// Full: delegates all verification to the original WebPkiServerVerifier
// SkipHostname: same as "Full" but ignores "NotValidForName" certificate errors
// SkipAll: all certificate verification checks are skipped.
impl RusTlsServerCertVerifier for CustomServerCertVerifier {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, RusTlsError> {
        match self.verification_mode {
            VerificationMode::Full => self.delegate.verify_server_cert(
                _end_entity,
                _intermediates,
                _server_name,
                _ocsp,
                _now,
            ),
            VerificationMode::SkipHostname => {
                match self.delegate.verify_server_cert(
                    _end_entity,
                    _intermediates,
                    _server_name,
                    _ocsp,
                    _now,
                ) {
                    Ok(scv) => Ok(scv),
                    Err(RusTlsError::InvalidCertificate(cert_error)) => {
                        if let CertificateError::NotValidForNameContext { .. } = cert_error {
                            Ok(ServerCertVerified::assertion())
                        } else {
                            Err(RusTlsError::InvalidCertificate(cert_error))
                        }
                    }
                    Err(e) => Err(e),
                }
            }
            VerificationMode::SkipAll => Ok(ServerCertVerified::assertion()),
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, RusTlsError> {
        self.delegate.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, RusTlsError> {
        self.delegate.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.delegate.supported_verify_schemes()
    }
}
