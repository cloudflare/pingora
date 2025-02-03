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

use crate::protocols::tls::SslDigest;
use pingora_boringssl::{hash::MessageDigest, ssl::SslRef};
use pingora_boringssl::{nid::Nid, x509::X509};
use pingora_error::{ErrorType::*, OrErr, Result};

// enables rustls & quic-boringssl usage
// at the cost of some duplication

impl SslDigest {
    pub(crate) fn from_quic_ssl(ssl: &SslRef) -> Self {
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
                (cert_digest, get_organization(&cert), get_serial(&cert).ok())
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

fn get_subject_name(cert: &X509, name_type: Nid) -> Option<String> {
    cert.subject_name()
        .entries_by_nid(name_type)
        .next()
        .map(|name| {
            name.data()
                .as_utf8()
                .map(|s| s.to_string())
                .unwrap_or_default()
        })
}

/// Return the organization associated with the X509 certificate.
pub fn get_organization(cert: &X509) -> Option<String> {
    get_subject_name(cert, Nid::ORGANIZATIONNAME)
}

/// Return the serial number associated with the X509 certificate as a hexadecimal value.
pub fn get_serial(cert: &X509) -> Result<String> {
    let bn = cert
        .serial_number()
        .to_bn()
        .or_err(InvalidCert, "Invalid serial")?;
    let hex = bn.to_hex_str().or_err(InvalidCert, "Invalid serial")?;

    let hex_str: &str = hex.as_ref();
    Ok(hex_str.to_owned())
}
