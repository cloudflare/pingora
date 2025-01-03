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

use crate::tls::{nid::Nid, pkey::PKey, pkey::Private, x509::X509};
use crate::Result;
use pingora_error::{ErrorType::*, OrErr};
use std::hash::{Hash, Hasher};

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

/// Return the common name associated with the X509 certificate.
pub fn get_common_name(cert: &X509) -> Option<String> {
    get_subject_name(cert, Nid::COMMONNAME)
}

/// Return the common name associated with the X509 certificate.
pub fn get_organization_unit(cert: &X509) -> Option<String> {
    get_subject_name(cert, Nid::ORGANIZATIONALUNITNAME)
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

/// This type contains a list of one or more certificates and an associated private key. The leaf
/// certificate should always be first.
#[derive(Clone)]
pub struct CertKey {
    certificates: Vec<X509>,
    key: PKey<Private>,
}

impl CertKey {
    /// Create a new `CertKey` given a list of certificates and a private key.
    pub fn new(certificates: Vec<X509>, key: PKey<Private>) -> CertKey {
        assert!(
            !certificates.is_empty(),
            "expected a non-empty vector of certificates in CertKey::new"
        );

        CertKey { certificates, key }
    }

    /// Peek at the leaf certificate.
    pub fn leaf(&self) -> &X509 {
        // This is safe due to the assertion above.
        &self.certificates[0]
    }

    /// Return the key.
    pub fn key(&self) -> &PKey<Private> {
        &self.key
    }

    /// Return a slice of intermediate certificates. An empty slice means there are none.
    pub fn intermediates(&self) -> &[X509] {
        if self.certificates.len() <= 1 {
            return &[];
        }
        &self.certificates[1..]
    }

    /// Return the organization from the leaf certificate.
    pub fn organization(&self) -> Option<String> {
        get_organization(self.leaf())
    }

    /// Return the serial from the leaf certificate.
    pub fn serial(&self) -> Result<String> {
        get_serial(self.leaf())
    }
}

impl Hash for CertKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        for certificate in &self.certificates {
            if let Ok(serial) = get_serial(certificate) {
                serial.hash(state)
            }
        }
    }
}

// hide private key
impl std::fmt::Debug for CertKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CertKey")
            .field("X509", &self.leaf())
            .finish()
    }
}

impl std::fmt::Display for CertKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let leaf = self.leaf();
        if let Some(cn) = get_common_name(leaf) {
            // Write CN if it exists
            write!(f, "CN: {cn},")?;
        } else if let Some(org_unit) = get_organization_unit(leaf) {
            // CA cert might not have CN, so print its unit name instead
            write!(f, "Org Unit: {org_unit},")?;
        }
        write!(f, ", expire: {}", leaf.not_after())
        // ignore the details of the private key
    }
}
