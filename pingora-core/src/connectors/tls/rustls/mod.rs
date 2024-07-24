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

//! Rustls TLS connector specific implementation

use std::any::Any;
use std::sync::Arc;

use log::debug;

use pingora_error::ErrorType::{ConnectTimedout, InvalidCert};
use pingora_error::{Error, OrErr, Result};
use pingora_rustls::version;
use pingora_rustls::CertificateDer;
use pingora_rustls::ClientConfig;
use pingora_rustls::ClientConfig as RusTlsClientConfig;
use pingora_rustls::PrivateKeyDer;
use pingora_rustls::RootCertStore;
use pingora_rustls::TlsConnector as RusTlsConnector;
use pingora_rustls::{
    load_ca_file_into_store, load_certs_key_file, load_platform_certs_incl_env_into_store,
};

use crate::connectors::ConnectorOptions;
use crate::listeners::ALPN;
use crate::protocols::tls::rustls::client::handshake;
use crate::protocols::tls::TlsStream;
use crate::protocols::IO;
use crate::upstreams::peer::Peer;

use super::{replace_leftmost_underscore, Connector, TlsConnectorContext};

pub(crate) struct TlsConnectorCtx {
    config: RusTlsClientConfig,
    ca_certs: RootCertStore,
}
impl TlsConnectorContext for TlsConnectorCtx {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn build_connector(options: Option<ConnectorOptions>) -> Connector
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
                    load_ca_file_into_store(ca_file_path, &mut ca_certs);
                } else {
                    load_platform_certs_incl_env_into_store(&mut ca_certs);
                }
                if let Some((cert, key)) = conf.cert_key_file.as_ref() {
                    certs_key = load_certs_key_file(&cert, &key);
                }
                // TODO: support SSLKEYLOGFILE
            } else {
                load_platform_certs_incl_env_into_store(&mut ca_certs);
            }

            (ca_certs, certs_key)
        };

        // TODO: WebPkiServerVerifier for CRL/OCSP validation
        let builder =
            ClientConfig::builder_with_protocol_versions(&vec![&version::TLS12, &version::TLS13])
                .with_root_certificates(ca_certs.clone());

        let config = match certs_key {
            Some((certs, key)) => {
                match builder.with_client_auth_cert(certs.clone(), key.clone_key()) {
                    Ok(config) => config,
                    Err(err) => {
                        // TODO: is there a viable alternative to the panic?
                        // falling back to no client auth... does not seem to be reasonable.
                        panic!(
                            "{}",
                            format!("Failed to configure client auth cert/key. Error: {}", err)
                        );
                    }
                }
            }
            None => builder.with_no_client_auth(),
        };

        Connector {
            ctx: Arc::new(TlsConnectorCtx { config, ca_certs }),
        }
    }
}

pub(super) async fn connect<T, P>(
    stream: T,
    peer: &P,
    alpn_override: Option<ALPN>,
    tls_ctx: &Arc<dyn TlsConnectorContext + Send + Sync>,
) -> Result<TlsStream<T>>
where
    T: IO,
    P: Peer + Send + Sync,
{
    let ctx = tls_ctx.as_any().downcast_ref::<TlsConnectorCtx>().unwrap();
    let mut config = ctx.config.clone();

    // TODO: setup CA/verify cert store from peer
    // looks like the fields are always None
    // peer.get_ca()

    let key_pair = peer.get_client_cert_key();
    let updated_config: Option<ClientConfig> = match key_pair {
        None => None,
        Some(key_arc) => {
            debug!("setting client cert and key");

            let mut cert_chain = vec![];
            debug!("adding leaf certificate to mTLS cert chain");
            cert_chain.push(key_arc.leaf().to_owned());

            debug!("adding intermediate certificates to mTLS cert chain");
            key_arc
                .intermediates()
                .to_owned()
                .iter()
                .map(|i| i.to_vec())
                .for_each(|i| cert_chain.push(i));

            let certs: Vec<CertificateDer> = cert_chain
                .into_iter()
                .map(|c| c.as_slice().to_owned().into())
                .collect();
            let private_key: PrivateKeyDer =
                key_arc.key().as_slice().to_owned().try_into().unwrap();

            let builder = ClientConfig::builder_with_protocol_versions(&vec![
                &version::TLS12,
                &version::TLS13,
            ])
            .with_root_certificates(ctx.ca_certs.clone());

            let updated_config = builder
                .with_client_auth_cert(certs, private_key)
                .explain_err(InvalidCert, |e| {
                    format!(
                        "Failed to use peer cert/key to update Rustls config: {:?}",
                        e
                    )
                })?;
            Some(updated_config)
        }
    };

    if let Some(alpn) = alpn_override.as_ref().or(peer.get_alpn()) {
        config.alpn_protocols = alpn.to_wire_protocols();
    }

    // TODO: curve setup from peer
    // - second key share from peer, currently only used in boringssl with PQ features

    let tls_conn = if let Some(cfg) = updated_config {
        RusTlsConnector::from(Arc::new(cfg))
    } else {
        RusTlsConnector::from(Arc::new(config))
    };

    // TODO: for consistent behaviour between TLS providers some additions are required
    // - allowing to disable verification
    // - the validation/replace logic would need adjustments to match the boringssl/openssl behaviour
    //   implementing a custom certificate_verifier could be used to achieve matching behaviour
    //let d_conf = config.dangerous();
    //d_conf.set_certificate_verifier(...);

    let mut domain = peer.sni().to_string();
    if peer.sni().is_empty() {
        // use ip in case SNI is not present
        // TODO: disable validation
        domain = peer.address().as_inet().unwrap().ip().to_string()
    }
    if peer.verify_cert() {
        if peer.verify_hostname() {
            // TODO: streamline logic with replacing first underscore within TLS implementations
            if let Some(sni_s) = replace_leftmost_underscore(peer.sni()) {
                domain = sni_s;
            }
            if let Some(alt_cn) = peer.alternative_cn() {
                if !alt_cn.is_empty() {
                    domain = alt_cn.to_string();
                    // TODO: streamline logic with replacing first underscore within TLS implementations
                    if let Some(alt_cn_s) = replace_leftmost_underscore(alt_cn) {
                        domain = alt_cn_s;
                    }
                }
            }
        }
    }

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
