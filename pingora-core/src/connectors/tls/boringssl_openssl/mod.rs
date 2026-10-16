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

use log::debug;
use pingora_error::{Error, ErrorType::*, OrErr, Result};
use std::sync::{Arc, Once};

use crate::connectors::tls::replace_leftmost_underscore;
use crate::connectors::ConnectorOptions;
use crate::protocols::tls::client::handshake;
use crate::protocols::tls::SslStream;
use crate::protocols::IO;
use crate::tls::ext::{
    add_host, clear_error_stack, ssl_add_chain_cert, ssl_set_groups_list,
    ssl_set_renegotiate_mode_freely, ssl_set_verify_cert_store, ssl_use_certificate,
    ssl_use_private_key, ssl_use_second_key_share,
};
use crate::tls::ssl::{SslConnector, SslFiletype, SslMethod, SslVerifyMode, SslVersion};
use crate::tls::x509::store::X509StoreBuilder;
use crate::upstreams::peer::{Peer, ALPN};

pub type TlsConnector = SslConnector;

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
 *
 * N.B. BoringSSL 5.x uses case-sensitive parsing for signature algorithm names,
 * so these must be in lowercase wire format.
 */
const SIGALG_LIST: &str = "ecdsa_secp256r1_sha256\
    :rsa_pss_rsae_sha256\
    :rsa_pkcs1_sha256\
    :ecdsa_secp384r1_sha384\
    :rsa_pss_rsae_sha384\
    :rsa_pkcs1_sha384\
    :rsa_pss_rsae_sha512\
    :rsa_pkcs1_sha512\
    :rsa_pkcs1_sha1\
    :ecdsa_secp521r1_sha512";

/**
 * Enabled curves for ECDHE (signature key exchange).
 * As of 4/10/2023, the only addition to boringssl's defaults is SECP521R1.
 *
 * N.B. The ordering of these curves is important. The boringssl library will select the first one
 * as a guess when negotiating a handshake with a server using TLSv1.3. We should opt for curves
 * that are both computationally cheaper and more supported.
 */
#[cfg(feature = "boringssl")]
const BORINGSSL_CURVES_LIST: &str = "X25519:P-256:P-384:P-521";

static INIT_CA_ENV: Once = Once::new();
fn init_ssl_cert_env_vars() {
    // this sets env vars to pick up the root certs
    // it is universal across openssl and boringssl
    // safety: although impossible to prove safe we assume it's safe since the call is
    // wrapped in a call_once and it's unlikely other threads are reading these vars
    INIT_CA_ENV.call_once(|| unsafe { openssl_probe::init_openssl_env_vars() });
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
        builder.set_sigalgs_list(SIGALG_LIST).unwrap();
        #[cfg(feature = "boringssl")]
        builder.set_curves_list(BORINGSSL_CURVES_LIST).unwrap();
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
                builder.set_certificate_chain_file(cert).unwrap();

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

    if let Some(curve) = peer.get_peer_options().and_then(|o| o.curves.as_deref()) {
        ssl_set_groups_list(&mut ssl_conf, curve).or_err(InternalError, "invalid curves")?;
    }

    // second_keyshare is default true
    if !peer.get_peer_options().is_none_or(|o| o.second_keyshare) {
        ssl_use_second_key_share(&mut ssl_conf, false);
    }

    // disable verification if sni does not exist
    // XXX: verify on empty string cause null string seg fault
    let verify_mode = if peer.sni().is_empty() {
        ssl_conf.set_use_server_name_indication(false);
        /* NOTE: technically we can still verify who signs the cert but turn it off to be
        consistent with nginx's behavior */
        SslVerifyMode::NONE
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
        SslVerifyMode::PEER
    } else {
        SslVerifyMode::NONE
    };
    match peer.upstream_tls_verify_callback().cloned() {
        Some(callback) => {
            ssl_conf.set_verify_callback(verify_mode, move |ok, ctx| callback(ok, ctx))
        }
        None => ssl_conf.set_verify(verify_mode),
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

    let complete_hook = peer
        .get_peer_options()
        .and_then(|o| o.upstream_tls_handshake_complete_hook.clone());
    let connect_future = handshake(ssl_conf, peer.sni(), stream, complete_hook);

    let mut stream = match peer.connection_timeout() {
        Some(t) => match pingora_timeout::timeout(t, connect_future).await {
            Ok(res) => res,
            Err(_) => Error::e_explain(
                ConnectTimedout,
                format!("connecting to server {}, timeout {:?}", peer, t),
            ),
        },
        None => connect_future.await,
    }?;
    if peer
        .get_peer_options()
        .is_some_and(|o| o.tls_write_coalescing)
    {
        stream.enable_write_coalescing();
    }
    Ok(stream)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocols::tls::server::handshake as accept;
    use crate::protocols::tls::VerifyCallback;
    use crate::tls::pkey::PKey;
    use crate::tls::ssl::SslAcceptor;
    use crate::tls::x509::X509;
    use crate::upstreams::peer::{BasicPeer, HttpPeer};
    use std::sync::Mutex;
    use tokio::io::DuplexStream;

    // `verify-leaf.crt` is issued to `verify.test` by `verify-ca.crt`; both expire in 2126.
    const SNI: &str = "verify.test";

    fn fixture(name: &str) -> Vec<u8> {
        std::fs::read(format!("{}/tests/certs/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap()
    }

    fn peer(trust_ca: bool, callback: VerifyCallback) -> BasicPeer {
        let mut peer = BasicPeer::new("127.0.0.1:443");
        peer.sni = SNI.to_string();
        if trust_ca {
            let ca = X509::from_pem(&fixture("verify-ca.crt")).unwrap();
            peer.options.ca = Some(Arc::new(vec![ca].into_boxed_slice()));
        }
        peer.options.upstream_tls_verify_callback = Some(callback);
        peer
    }

    /// Returns a callback that records `(preverify_ok, depth, chain length)` for each call and
    /// returns `verdict(preverify_ok)`.
    fn recording(
        verdict: fn(bool) -> bool,
    ) -> (VerifyCallback, Arc<Mutex<Vec<(bool, u32, usize)>>>) {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let recorded = calls.clone();
        let callback: VerifyCallback = Arc::new(move |ok, ctx| {
            let chain_len = ctx.chain().map_or(0, |c| c.len());
            recorded
                .lock()
                .unwrap()
                .push((ok, ctx.error_depth(), chain_len));
            verdict(ok)
        });
        (callback, calls)
    }

    /// Runs a TLS handshake from `connect` against a server presenting the fixture leaf.
    async fn handshake_with_fixture_server(peer: &BasicPeer) -> Result<()> {
        let mut builder = SslAcceptor::mozilla_intermediate_v5(SslMethod::tls()).unwrap();
        builder
            .set_certificate(&X509::from_pem(&fixture("verify-leaf.crt")).unwrap())
            .unwrap();
        builder
            .set_private_key(&PKey::private_key_from_pem(&fixture("verify-leaf.key")).unwrap())
            .unwrap();
        let acceptor = builder.build();
        let (client, server) = tokio::io::duplex(16 * 1024);
        let server = tokio::spawn(async move {
            let _ = accept(&acceptor, server).await;
        });
        let connector = super::Connector::new(None);
        let result = connect(client, peer, None, &connector.ctx)
            .await
            .map(|_| ());
        server.abort();
        result
    }

    #[tokio::test]
    async fn verify_callback_observes_verified_chain() {
        let (callback, calls) = recording(|ok| ok);
        handshake_with_fixture_server(&peer(true, callback))
            .await
            .unwrap();

        // the CA, then the leaf, each with the whole chain built
        assert_eq!(*calls.lock().unwrap(), [(true, 1, 2), (true, 0, 2)]);
    }

    #[tokio::test]
    async fn verify_callback_can_reject_verified_chain() {
        let (callback, calls) = recording(|_| false);
        let err = handshake_with_fixture_server(&peer(true, callback))
            .await
            .unwrap_err();

        // verification stops at the rejected CA
        assert_eq!(*calls.lock().unwrap(), [(true, 1, 2)]);
        assert!(err.to_string().contains("TLS connect() failed"), "{err}");
    }

    #[tokio::test]
    async fn verify_callback_runs_when_verification_is_off() {
        let (callback, calls) = recording(|_| false);
        let mut peer = peer(false, callback);
        peer.options.verify_cert = false;
        // the CA is not trusted, and even rejecting cannot fail the handshake
        handshake_with_fixture_server(&peer).await.unwrap();

        // the leaf's issuer is missing
        assert_eq!(*calls.lock().unwrap(), [(false, 0, 1)]);
    }

    #[tokio::test]
    async fn verify_callback_sees_hostname_mismatch_once() {
        let (callback, calls) = recording(|ok| ok);
        let mut peer = peer(true, callback);
        peer.sni = "other.test".to_string();
        handshake_with_fixture_server(&peer).await.unwrap_err();

        // the hostname is checked before the chain's signatures, so the leaf fails without an
        // earlier successful call
        assert_eq!(*calls.lock().unwrap(), [(false, 0, 2)]);
    }

    #[tokio::test]
    async fn verify_callback_runs_without_sni() {
        let (callback, calls) = recording(|ok| ok);
        let mut peer = peer(true, callback);
        peer.sni.clear();
        handshake_with_fixture_server(&peer).await.unwrap();

        assert_eq!(*calls.lock().unwrap(), [(true, 1, 2), (true, 0, 2)]);
    }

    /// Connects to `peer` with the default connector, with a local TLS server on the other end
    /// of a duplex, and returns the client's stream.
    async fn connect_locally(peer: &HttpPeer) -> SslStream<DuplexStream> {
        let mut acceptor = SslAcceptor::mozilla_intermediate_v5(SslMethod::tls()).unwrap();
        let dir = env!("CARGO_MANIFEST_DIR");
        acceptor
            .set_certificate_chain_file(format!("{dir}/tests/keys/server.crt"))
            .unwrap();
        acceptor
            .set_private_key_file(format!("{dir}/tests/keys/key.pem"), SslFiletype::PEM)
            .unwrap();
        let acceptor = acceptor.build();
        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let server = tokio::spawn(async move { accept(&acceptor, server_io).await.unwrap() });

        let connector = Connector::new(None);
        let stream = connect(client_io, peer, None, &connector.ctx)
            .await
            .unwrap();
        server.await.unwrap();
        stream
    }

    #[tokio::test]
    async fn upstream_streams_coalesce_writes_only_when_the_peer_enables_it() {
        let mut peer = HttpPeer::new("127.0.0.1:443", true, "pingora.org".into());
        peer.options.verify_cert = false;
        assert!(!connect_locally(&peer).await.write_coalescing());

        peer.options.tls_write_coalescing = true;
        assert!(connect_locally(&peer).await.write_coalescing());
    }
}
