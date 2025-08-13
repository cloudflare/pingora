// Copyright 2025 Cloudflare, Inc.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applijable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use pingora_error::{Error, ErrorType, Result};
use std::fs;

pub use s2n_tls::{
    callbacks::VerifyHostNameCallback,
    config::{Builder as ConfigBuilder, Config},
    connection::{Builder as ConnectionBuilder, Connection},
    enums::{ClientAuthType, Mode, PskHmac},
    error::Error as S2NError,
    psk::Psk,
    security::{Policy as S2NPolicy, DEFAULT_TLS13},
};
pub use s2n_tls_tokio::{TlsAcceptor, TlsConnector, TlsStream};

pub fn load_certs_and_key_files(cert_file: &str, key_file: &str) -> Result<(Vec<u8>, Vec<u8>)> {
    let cert_bytes = load_pem_file(cert_file)?;
    let key_bytes = load_pem_file(key_file)?;
    Ok((cert_bytes, key_bytes))
}

pub fn load_pem_file(file: &str) -> Result<Vec<u8>> {
    if let Ok(bytes) = fs::read(file) {
        Ok(bytes)
    } else {
        Error::e_explain(
            ErrorType::InvalidCert,
            "Certificate in pem file could not be read",
        )
    }
}

pub fn hash_certificate(cert: &[u8]) -> Vec<u8> {
    let hash = ring::digest::digest(&ring::digest::SHA256, cert);
    hash.as_ref().to_vec()
}

/// Verify host name callback that always returns a success,
/// effectively ignoring hostname validation
pub struct IgnoreVerifyHostnameCallback {}

impl IgnoreVerifyHostnameCallback {
    pub fn new() -> Self {
        IgnoreVerifyHostnameCallback {}
    }
}

impl Default for IgnoreVerifyHostnameCallback {
    fn default() -> Self {
        Self::new()
    }
}

impl VerifyHostNameCallback for IgnoreVerifyHostnameCallback {
    fn verify_host_name(&self, _host_name: &str) -> bool {
        true
    }
}
