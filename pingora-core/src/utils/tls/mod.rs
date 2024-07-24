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

//! This module contains various types that make it easier to work with bytes and X509
//! certificates.

#[cfg(not(feature = "rustls"))]
pub mod boringssl_openssl;
#[cfg(feature = "rustls")]
pub mod rustls;

#[cfg(not(feature = "rustls"))]
use boringssl_openssl::{
    get_common_name, get_not_after, get_organization, get_organizational_unit, get_serial,
};
#[cfg(feature = "rustls")]
use rustls::{
    get_common_name, get_not_after, get_organization, get_organizational_unit, get_serial,
};
use std::hash::{Hash, Hasher};

/// This type contains a list of one or more certificates and an associated private key. The leaf
/// certificate should always be first. The certificates and keys are stored in Vec<u8> DER encoded
/// form for usage within OpenSSL/BoringSSL & RusTLS.
#[derive(Clone)]
pub struct CertKey {
    certificates: Vec<Vec<u8>>,
    key: Vec<u8>,
}

impl CertKey {
    /// Create a new `CertKey` given a list of certificates and a private key.
    pub fn new(certificates: Vec<Vec<u8>>, key: Vec<u8>) -> CertKey {
        assert!(
            !certificates.is_empty() && !certificates.first().unwrap().is_empty(),
            "expected a non-empty vector of certificates in CertKey::new"
        );

        CertKey { certificates, key }
    }

    /// Peek at the leaf certificate.
    pub fn leaf(&self) -> &Vec<u8> {
        // This is safe due to the assertion above.
        &self.certificates[0]
    }

    /// Return the key.
    pub fn key(&self) -> &Vec<u8> {
        &self.key
    }

    /// Return a slice of intermediate certificates. An empty slice means there are none.
    pub fn intermediates(&self) -> Vec<&Vec<u8>> {
        self.certificates.iter().skip(1).collect()
    }

    /// Return the organization from the leaf certificate.
    pub fn organization(&self) -> Option<String> {
        get_organization(self.leaf())
    }

    /// Return the serial from the leaf certificate.
    pub fn serial(&self) -> String {
        get_serial(self.leaf()).unwrap()
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
        } else if let Some(org_unit) = get_organizational_unit(leaf) {
            // CA cert might not have CN, so print its unit name instead
            write!(f, "Org Unit: {org_unit},")?;
        }
        write!(f, ", expire: {}", get_not_after(leaf))
        // ignore the details of the private key
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
