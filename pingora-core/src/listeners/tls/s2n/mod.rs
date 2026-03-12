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
use pingora_error::Result;
use pingora_s2n::{
    load_certs_and_key_files, ClientAuthType, Config, IgnoreVerifyHostnameCallback, S2NPolicy,
    TlsAcceptor, DEFAULT_TLS13,
};

use crate::protocols::tls::server::handshake;
use crate::protocols::tls::{CaType, PskConfig, PskType, S2NConnectionBuilder, TlsStream};
use crate::protocols::{ALPN, IO};

/// The TLS settings of a listening endpoint
pub struct TlsSettings {
    cert_path: Option<String>,
    key_path: Option<String>,
    ca: Option<CaType>,
    alpn: Option<ALPN>,
    psk_config: Option<Arc<PskType>>,
    security_policy: Option<S2NPolicy>,
    client_auth_required: bool,
    verify_client_hostname: bool,
    max_blinding_delay: Option<u32>,
}

pub struct Acceptor {
    pub acceptor: TlsAcceptor<S2NConnectionBuilder>,
}

impl TlsSettings {
    pub fn build(self) -> Acceptor {
        let mut builder = Config::builder();

        // Default security policy with TLS 1.3 support
        // https://aws.github.io/s2n-tls/usage-guide/ch06-security-policies.html
        let policy = self.security_policy.unwrap_or(DEFAULT_TLS13);

        if let Some(max_blinding_delay) = self.max_blinding_delay {
            builder.set_max_blinding_delay(max_blinding_delay).unwrap();
        }

        if self.client_auth_required {
            builder
                .set_client_auth_type(ClientAuthType::Required)
                .unwrap();
        }

        if let Some(alpn) = self.alpn {
            builder
                .set_application_protocol_preference(alpn.to_wire_protocols())
                .unwrap();
        }

        if let (Some(cert_path), Some(key_path)) = (self.cert_path, self.key_path) {
            let Ok((cert, key)) = load_certs_and_key_files(&cert_path, &key_path) else {
                panic!(
                    "Failed to load provided certificates \"{}\" or key \"{}\".",
                    cert_path, key_path
                )
            };

            builder.load_pem(&cert, &key).unwrap();
        }

        if let Some(ca) = self.ca {
            builder.trust_pem(&ca.raw_pem).expect("invalid ca pem");
        }

        if !self.verify_client_hostname {
            builder
                .set_verify_host_callback(IgnoreVerifyHostnameCallback::new())
                .unwrap();
        }

        let config = builder.build().unwrap();
        let connection_builder = S2NConnectionBuilder {
            config: config,
            psk_config: self.psk_config.clone(),
            security_policy: Some(policy.clone()),
        };

        Acceptor {
            acceptor: TlsAcceptor::new(connection_builder),
        }
    }

    /// Enable HTTP/2 support for this endpoint, which is default off.
    /// This effectively sets the ALPN to prefer HTTP/2 with HTTP/1.1 allowed
    pub fn enable_h2(&mut self) {
        self.set_alpn(ALPN::H2H1);
    }

    fn set_alpn(&mut self, alpn: ALPN) {
        self.alpn = Some(alpn);
    }

    /// Configure CA to use for mTLS
    pub fn set_ca(&mut self, ca: CaType) {
        self.ca = Some(ca);
    }

    /// Configure pre-shared keys to use for TLS-PSK handshake
    /// https://datatracker.ietf.org/doc/html/rfc4279
    pub fn set_psk_config(&mut self, psk_config: PskConfig) {
        self.psk_config = Some(Arc::new(psk_config));
    }

    /// S2N-TLS security policy to use. If not set, the default policy
    /// "default_tls13" will be used.
    /// https://aws.github.io/s2n-tls/usage-guide/ch06-security-policies.html
    pub fn set_policy(&mut self, policy: S2NPolicy) {
        self.security_policy = Some(policy);
    }

    /// The certificate and private key to use for TLS connections
    pub fn set_cert(&mut self, cert_path: &str, key_path: &str) {
        self.cert_path = Some(cert_path.to_string());
        self.key_path = Some(key_path.to_string());
    }

    /// Require client certificate authentication (mTLS)
    pub fn set_client_auth_required(&mut self, required: bool) {
        self.client_auth_required = required;
    }

    /// If validating client certificate, also verify client hostname (mTLS)
    pub fn set_verify_client_hostname(&mut self, verify: bool) {
        self.verify_client_hostname = verify;
    }

    /// S2N-TLS will delay a response up to the max blinding delay (default 30)
    /// seconds whenever an error triggered by a peer occurs to mitigate against
    /// timing side channels.
    pub fn set_max_blinding_delay(&mut self, delay: u32) {
        self.max_blinding_delay = Some(delay);
    }

    pub fn intermediate(cert_path: &str, key_path: &str) -> Result<Self>
    where
        Self: Sized,
    {
        Ok(TlsSettings {
            cert_path: Some(cert_path.to_string()),
            key_path: Some(key_path.to_string()),
            ca: None,
            security_policy: None,
            alpn: None,
            psk_config: None,
            client_auth_required: false,
            verify_client_hostname: false,
            max_blinding_delay: None,
        })
    }

    pub fn new() -> Self {
        TlsSettings {
            cert_path: None,
            key_path: None,
            ca: None,
            security_policy: None,
            alpn: None,
            psk_config: None,
            client_auth_required: false,
            verify_client_hostname: false,
            max_blinding_delay: None,
        }
    }
}

impl Acceptor {
    pub async fn tls_handshake<S: IO>(&self, stream: S) -> Result<TlsStream<S>> {
        debug!("new tls session");
        handshake(self, stream).await
    }
}
