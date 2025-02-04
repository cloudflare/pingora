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

//! Quic TLS Connector

use crate::connectors::ConnectorOptions;
use crate::protocols::l4::quic::QuicHttp3Configs;
use pingora_boringssl::ssl::{SslContextBuilder, SslCurve, SslFiletype, SslMethod, SslVersion};
use std::ffi::OsStr;
use std::sync::Once;

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
    // safety: although impossible to prove safe we assume it's safe since the call is
    // wrapped in a call_once and it's unlikely other threads are reading these vars
    INIT_CA_ENV.call_once(|| unsafe { openssl_probe::init_openssl_env_vars() });
}

/// enables rustls & quic-boringssl usage
// at the cost of some duplication
#[derive(Clone)]
pub struct Connector {
    pub(crate) quic_http3: QuicHttp3Configs,
}

impl Connector {
    pub fn new(options: Option<ConnectorOptions>) -> Self {
        let mut quic_builder = SslContextBuilder::new(SslMethod::tls()).unwrap();
        // TODO: make these conf
        // Set supported ciphers.
        quic_builder.set_cipher_list(CIPHER_LIST).unwrap();
        // Set supported signature algorithms and ECDH (key exchange) curves.

        quic_builder
            .set_sigalgs_list(&SIGALG_LIST.to_lowercase())
            .unwrap();
        quic_builder.set_curves(BORINGSSL_CURVE_LIST).unwrap();

        quic_builder
            .set_max_proto_version(Some(SslVersion::TLS1_3))
            .unwrap();
        quic_builder
            .set_min_proto_version(Some(SslVersion::TLS1_3)) // HTTP3 requires TLS 1.3
            .unwrap();

        if let Some(conf) = options.as_ref() {
            if let Some(ca_file_path) = conf.ca_file.as_ref() {
                quic_builder.set_ca_file(ca_file_path).unwrap();
            } else {
                init_ssl_cert_env_vars();
                // load from default system wide trust location. (the name is misleading)
                quic_builder.set_default_verify_paths().unwrap();
            }
            if let Some((cert, key)) = conf.cert_key_file.as_ref() {
                quic_builder
                    .set_certificate_file(cert, SslFiletype::PEM)
                    .unwrap();
                quic_builder
                    .set_private_key_file(key, SslFiletype::PEM)
                    .unwrap();
            }
            if conf.debug_ssl_keylog {
                // write TLS keys to file specified by SSLKEYLOGFILE if it exists
                if let Some(quic_keylog) = std::env::var_os("SSLKEYLOGFILE").and_then(|mut path| {
                    path.push(OsStr::new(".quic")); // suffix to avoid collisions with TCP/TLS keylog
                    std::fs::OpenOptions::new()
                        .append(true)
                        .create(true)
                        .open(path)
                        .ok()
                }) {
                    use std::io::Write;
                    quic_builder.set_keylog_callback(move |_, line| {
                        let _ = writeln!(&quic_keylog, "{}", line);
                    });
                }
            }
        } else {
            init_ssl_cert_env_vars();
            quic_builder.set_default_verify_paths().unwrap();
        }

        Connector {
            quic_http3: QuicHttp3Configs::with_boring_ssl_ctx_builder(quic_builder).unwrap(),
        }
    }
}
