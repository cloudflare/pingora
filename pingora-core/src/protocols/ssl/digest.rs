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

//! TLS information from the TLS connection

use crate::tls::{hash::MessageDigest, ssl::SslRef};
use crate::utils;

/// The TLS connection information
#[derive(Clone, Debug)]
pub struct SslDigest {
    /// The cipher used
    pub cipher: &'static str,
    /// The TLS version of this connection
    pub version: &'static str,
    /// The organization of the peer's certificate
    pub organization: Option<String>,
    /// The serial number of the peer's certificate
    pub serial_number: Option<String>,
    /// The digest of the peer's certificate
    pub cert_digest: Vec<u8>,
}

impl SslDigest {
    pub fn from_ssl(ssl: &SslRef) -> Self {
        let cipher = match ssl.current_cipher() {
            Some(c) => c.name(),
            None => "",
        };

        let (cert_digest, org, sn) = match ssl.peer_certificate() {
            Some(cert) => {
                let cert_digest = match cert.digest(MessageDigest::sha256()) {
                    Ok(c) => c.as_ref().to_vec(),
                    Err(_) => Vec::new(),
                };
                (
                    cert_digest,
                    utils::get_organization(&cert),
                    utils::get_serial(&cert).ok(),
                )
            }
            None => (Vec::new(), None, None),
        };

        SslDigest {
            cipher,
            version: ssl.version_str(),
            organization: org,
            serial_number: sn,
            cert_digest,
        }
    }
}
