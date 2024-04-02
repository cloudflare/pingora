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

use log::debug;
use pingora_error::{Error, ErrorType::*, OrErr, Result};
use std::sync::{Arc, Once};

use super::ConnectorOptions;
use crate::protocols::ssl::client::handshake;
use crate::protocols::ssl::SslStream;
use crate::protocols::IO;
use crate::tls::ext::{
    add_host, clear_error_stack, ssl_add_chain_cert, ssl_set_groups_list,
    ssl_set_renegotiate_mode_freely, ssl_set_verify_cert_store, ssl_use_certificate,
    ssl_use_private_key, ssl_use_second_key_share,
};
#[cfg(feature = "boringssl")]
use crate::tls::ssl::SslCurve;
use crate::tls::ssl::{SslConnector, SslFiletype, SslMethod, SslVerifyMode, SslVersion};
use crate::tls::x509::store::X509StoreBuilder;
use crate::upstreams::peer::{Peer, ALPN};

const CIPHER_LIST: &str = "AES-128-GCM-SHA256\
    :AES-256-GCM-SHA384\
    :CHACHA20-POLY1305-SHA256\
    :ECDHE-ECDSA-AES128-GCM-SHA256\
    :ECDHE-ECDSA-AES256-GCM-SHA384\
    :ECDHE-RSA-AES128-GCM-SHA256\
    :ECDHE-RSA-AES256-GCM-SHA384\
    :ECDHE-RSA-AES128-SHA\
    :ECDHE-RSA-AES256-SHA384\
    :AES128-GCM-SHA256\
    :AES256-GCM-SHA384\
    :AES128-SHA\
    :AES256-SHA\
    :DES-CBC3-SHA";

/**
 * Enabled signature algorithms for signing/verification (ECDSA).
 * As of 4/10/2023, the only addition to boringssl's defaults is ECDSA_SECP521R1_SHA512.
 */
const SIGALG_LIST: &str = "ECDSA_SECP256R1_SHA256\
    :RSA_PSS_RSAE_SHA256\
    :RSA_PKCS1_SHA256\
    :ECDSA_SECP384R1_SHA384\
    :RSA_PSS_RSAE_SHA384\
    :RSA_PKCS1_SHA384\
    :RSA_PSS_RSAE_SHA512\
    :RSA_PKCS1_SHA512\
    :RSA_PKCS1_SHA1\
    :ECDSA_SECP521R1_SHA512";
/**
 * Enabled curves for ECDHE (signature key exchange).
 * As of 4/10/2023, the only addition to boringssl's defaults is SECP521R1.
 *
 * N.B. The ordering of these curves is important. The boringssl library will select the first one
 * as a guess when negotiating a handshake with a server using TLSv1.3. We should opt for curves
 * that are both computationally cheaper and more supported.
 */
#[cfg(feature = "boringssl")]
const BORINGSSL_CURVE_LIST: &[SslCurve] = &[
    SslCurve::X25519,
    SslCurve::SECP256R1,
    SslCurve::SECP384R1,
    SslCurve::SECP521R1,
];

static INIT_CA_ENV: Once = Once::new();
fn init_ssl_cert_env_vars() {
    // this sets env vars to pick up the root certs
    // it is universal across openssl and boringssl
    INIT_CA_ENV.call_once(openssl_probe::init_ssl_cert_env_vars);
}

#[derive(Clone)]
pub struct Connector {
    pub(crate) ctx: Arc<SslConnector>, // Arc to support clone
}

impl Connector {
    pub fn new(options: Option<ConnectorOptions>) -> Self {
        let mut builder = SslConnector::builder(SslMethod::tls()).unwrap();
        // TODO: make these conf
        // Set supported ciphers.
        builder.set_cipher_list(CIPHER_LIST).unwrap();
        // Set supported signature algorithms and ECDH (key exchange) curves.
        builder
            .set_sigalgs_list(&SIGALG_LIST.to_lowercase())
            .unwrap();
        #[cfg(feature = "boringssl")]
        builder.set_curves(BORINGSSL_CURVE_LIST).unwrap();
        builder
            .set_max_proto_version(Some(SslVersion::TLS1_3))
            .unwrap();
        builder
            .set_min_proto_version(Some(SslVersion::TLS1))
            .unwrap();
        if let Some(conf) = options.as_ref() {
            if let Some(ca_file_path) = conf.ca_file.as_ref() {
                builder.set_ca_file(ca_file_path).unwrap();
            } else {
                init_ssl_cert_env_vars();
                // load from default system wide trust location. (the name is misleading)
                builder.set_default_verify_paths().unwrap();
            }
            if let Some((cert, key)) = conf.cert_key_file.as_ref() {
                builder
                    .set_certificate_file(cert, SslFiletype::PEM)
                    .unwrap();

                builder.set_private_key_file(key, SslFiletype::PEM).unwrap();
            }
            if conf.debug_ssl_keylog {
                // write TLS keys to file specified by SSLKEYLOGFILE if it exists
                if let Some(keylog) = std::env::var_os("SSLKEYLOGFILE").and_then(|path| {
                    std::fs::OpenOptions::new()
                        .append(true)
                        .create(true)
                        .open(path)
                        .ok()
                }) {
                    use std::io::Write;
                    builder.set_keylog_callback(move |_, line| {
                        let _ = writeln!(&keylog, "{}", line);
                    });
                }
            }
        } else {
            init_ssl_cert_env_vars();
            builder.set_default_verify_paths().unwrap();
        }

        Connector {
            ctx: Arc::new(builder.build()),
        }
    }
}

/*
    OpenSSL considers underscores in hostnames non-compliant.
    We replace the underscore in the leftmost label as we must support these
    hostnames for wildcard matches and we have not patched OpenSSL.

    https://github.com/openssl/openssl/issues/12566

    > The labels must follow the rules for ARPANET host names.  They must
    > start with a letter, end with a letter or digit, and have as interior
    > characters only letters, digits, and hyphen.  There are also some
    > restrictions on the length.  Labels must be 63 characters or less.
    - https://datatracker.ietf.org/doc/html/rfc1034#section-3.5
*/
fn replace_leftmost_underscore(sni: &str) -> Option<String> {
    // wildcard is only leftmost label
    if let Some((leftmost, rest)) = sni.split_once('.') {
        // if not a subdomain or leftmost does not contain underscore return
        if !rest.contains('.') || !leftmost.contains('_') {
            return None;
        }
        // we have a subdomain, replace underscores
        let leftmost = leftmost.replace('_', "-");
        return Some(format!("{leftmost}.{rest}"));
    }
    None
}

pub(crate) async fn connect<T, P>(
    stream: T,
    peer: &P,
    alpn_override: Option<ALPN>,
    tls_ctx: &SslConnector,
) -> Result<SslStream<T>>
where
    T: IO,
    P: Peer + Send + Sync,
{
    let mut ssl_conf = tls_ctx.configure().unwrap();

    ssl_set_renegotiate_mode_freely(&mut ssl_conf);

    // Set up CA/verify cert store
    // TODO: store X509Store in the peer directly
    if let Some(ca_list) = peer.get_ca() {
        let mut store_builder = X509StoreBuilder::new().unwrap();
        for ca in &***ca_list {
            store_builder.add_cert(ca.clone()).unwrap();
        }
        ssl_set_verify_cert_store(&mut ssl_conf, &store_builder.build())
            .or_err(InternalError, "failed to load cert store")?;
    }

    // Set up client cert/key
    if let Some(key_pair) = peer.get_client_cert_key() {
        debug!("setting client cert and key");
        ssl_use_certificate(&mut ssl_conf, key_pair.leaf())
            .or_err(InternalError, "invalid client cert")?;
        ssl_use_private_key(&mut ssl_conf, key_pair.key())
            .or_err(InternalError, "invalid client key")?;

        let intermediates = key_pair.intermediates();
        if !intermediates.is_empty() {
            debug!("adding intermediate certificates for mTLS chain");
            for int in intermediates {
                ssl_add_chain_cert(&mut ssl_conf, int)
                    .or_err(InternalError, "invalid intermediate client cert")?;
            }
        }
    }

    if let Some(curve) = peer.get_peer_options().and_then(|o| o.curves) {
        ssl_set_groups_list(&mut ssl_conf, curve).or_err(InternalError, "invalid curves")?;
    }

    // second_keyshare is default true
    if !peer.get_peer_options().map_or(true, |o| o.second_keyshare) {
        ssl_use_second_key_share(&mut ssl_conf, false);
    }

    // disable verification if sni does not exist
    // XXX: verify on empty string cause null string seg fault
    if peer.sni().is_empty() {
        ssl_conf.set_use_server_name_indication(false);
        /* NOTE: technically we can still verify who signs the cert but turn it off to be
        consistent with nginx's behavior */
        ssl_conf.set_verify(SslVerifyMode::NONE);
    } else if peer.verify_cert() {
        if peer.verify_hostname() {
            let verify_param = ssl_conf.param_mut();
            add_host(verify_param, peer.sni()).or_err(InternalError, "failed to add host")?;
            // if sni had underscores in leftmost label replace and add
            if let Some(sni_s) = replace_leftmost_underscore(peer.sni()) {
                add_host(verify_param, sni_s.as_ref()).unwrap();
            }
            if let Some(alt_cn) = peer.alternative_cn() {
                if !alt_cn.is_empty() {
                    add_host(verify_param, alt_cn).unwrap();
                    // if alt_cn had underscores in leftmost label replace and add
                    if let Some(alt_cn_s) = replace_leftmost_underscore(alt_cn) {
                        add_host(verify_param, alt_cn_s.as_ref()).unwrap();
                    }
                }
            }
        }
        ssl_conf.set_verify(SslVerifyMode::PEER);
    } else {
        ssl_conf.set_verify(SslVerifyMode::NONE);
    }

    /*
       We always set set_verify_hostname(false) here because:
        - verify case.)  otherwise ssl.connect calls X509_VERIFY_PARAM_set1_host
                         which overrides the names added by add_host. Verify is
                         essentially on as long as the names are added.
        - off case.)    the non verify hostname case should have it disabled
    */
    ssl_conf.set_verify_hostname(false);

    if let Some(alpn) = alpn_override.as_ref().or(peer.get_alpn()) {
        ssl_conf.set_alpn_protos(alpn.to_wire_preference()).unwrap();
    }

    clear_error_stack();
    let connect_future = handshake(ssl_conf, peer.sni(), stream);

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_replace_leftmost_underscore() {
        let none_cases = [
            "",
            "some",
            "some.com",
            "1.1.1.1:5050",
            "dog.dot.com",
            "dog.d_t.com",
            "dog.dot.c_m",
            "d_g.com",
            "_",
            "dog.c_m",
        ];

        for case in none_cases {
            assert!(replace_leftmost_underscore(case).is_none(), "{}", case);
        }

        assert_eq!(
            Some("bb-b.some.com".to_string()),
            replace_leftmost_underscore("bb_b.some.com")
        );
        assert_eq!(
            Some("a-a-a.some.com".to_string()),
            replace_leftmost_underscore("a_a_a.some.com")
        );
        assert_eq!(
            Some("-.some.com".to_string()),
            replace_leftmost_underscore("_.some.com")
        );
    }
}
