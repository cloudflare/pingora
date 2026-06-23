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

use ouroboros::self_referencing;
use pingora_error::Result;
use std::hash::{Hash, Hasher};
use x509_parser::{
    pem::Pem,
    prelude::{FromDer, X509Certificate},
};

fn get_organization_serial_x509(
    x509cert: &X509Certificate<'_>,
) -> Result<(Option<String>, String)> {
    let serial = x509cert.raw_serial_as_string();
    Ok((get_organization_x509(x509cert), serial))
}

/// Get the serial number associated with the given certificate
/// see https://en.wikipedia.org/wiki/X.509#Structure_of_a_certificate
pub fn get_serial(x509cert: &WrappedX509) -> Result<String> {
    Ok(x509cert.borrow_cert().raw_serial_as_string())
}

/// Return the organization associated with the X509 certificate.
/// see https://en.wikipedia.org/wiki/X.509#Structure_of_a_certificate
pub fn get_organization(x509cert: &WrappedX509) -> Option<String> {
    get_organization_x509(x509cert.borrow_cert())
}

/// Return the organization associated with the X509 certificate.
/// see https://en.wikipedia.org/wiki/X.509#Structure_of_a_certificate
pub fn get_organization_x509(x509cert: &X509Certificate<'_>) -> Option<String> {
    x509cert
        .subject
        .iter_organization()
        .filter_map(|a| a.as_str().ok())
        .map(|a| a.to_string())
        .reduce(|cur, next| cur + &next)
}

/// Return the organization associated with the X509 certificate (as bytes).
/// see https://en.wikipedia.org/wiki/X.509#Structure_of_a_certificate
pub fn get_organization_serial_bytes(cert: &[u8]) -> Result<(Option<String>, String)> {
    let (_, x509cert) = x509_parser::certificate::X509Certificate::from_der(cert)
        .expect("Failed to parse certificate from DER format.");

    get_organization_serial_x509(&x509cert)
}

/// Return the organization unit associated with the X509 certificate.
/// see https://en.wikipedia.org/wiki/X.509#Structure_of_a_certificate
pub fn get_organization_unit(x509cert: &WrappedX509) -> Option<String> {
    x509cert
        .borrow_cert()
        .subject
        .iter_organizational_unit()
        .filter_map(|a| a.as_str().ok())
        .map(|a| a.to_string())
        .reduce(|cur, next| cur + &next)
}

/// Get a combination of the common names for the given certificate
/// see https://en.wikipedia.org/wiki/X.509#Structure_of_a_certificate
pub fn get_common_name(x509cert: &WrappedX509) -> Option<String> {
    x509cert
        .borrow_cert()
        .subject
        .iter_common_name()
        .filter_map(|a| a.as_str().ok())
        .map(|a| a.to_string())
        .reduce(|cur, next| cur + &next)
}

/// Get the `not_after` field for the valid time period for the given cert
/// see https://en.wikipedia.org/wiki/X.509#Structure_of_a_certificate
pub fn get_not_after(x509cert: &WrappedX509) -> String {
    x509cert.borrow_cert().validity.not_after.to_string()
}

/// This type contains a list of one or more certificates and an associated private key. The leaf
/// certificate should always be first.
pub struct CertKey {
    key: Vec<u8>,
    pem: X509Pem,
}

impl CertKey {
    /// Create a new `CertKey` given a list of certificates and a private key.
    pub fn new(pem_bytes: Vec<u8>, key: Vec<u8>) -> CertKey {
        let pem = X509Pem::new(pem_bytes);
        assert!(
            !pem.certs.is_empty(),
            "expected at least one certificate in PEM"
        );

        CertKey { key, pem }
    }

    /// Peek at the leaf certificate.
    pub fn leaf(&self) -> &WrappedX509 {
        // This is safe due to the assertion in creation of a `CertKey`
        &self.pem.certs[0]
    }

    /// Return the key.
    pub fn key(&self) -> &Vec<u8> {
        &self.key
    }

    /// Return a slice of intermediate certificates. An empty slice means there are none.
    pub fn intermediates(&self) -> Vec<&WrappedX509> {
        self.pem.certs.iter().skip(1).collect()
    }

    /// Return the organization from the leaf certificate.
    pub fn organization(&self) -> Option<String> {
        get_organization(self.leaf())
    }

    /// Return the serial from the leaf certificate.
    pub fn serial(&self) -> String {
        get_serial(self.leaf()).unwrap()
    }

    pub fn raw_pem(&self) -> &[u8] {
        &self.pem.raw_pem
    }
}

#[derive(Debug)]
pub struct X509Pem {
    pub raw_pem: Vec<u8>,
    pub certs: Vec<WrappedX509>,
}

impl X509Pem {
    pub fn new(raw_pem: Vec<u8>) -> Self {
        let certs = Pem::iter_from_buffer(&raw_pem)
            .map(|part| {
                let raw_cert = part.expect("Failed to parse PEM").contents;
                WrappedX509::new(raw_cert, parse_x509)
            })
            .collect();
        X509Pem { raw_pem, certs }
    }

    pub fn iter(&self) -> std::slice::Iter<'_, WrappedX509> {
        self.certs.iter()
    }
}

fn parse_x509<C>(raw_cert: &C) -> X509Certificate<'_>
where
    C: AsRef<[u8]>,
{
    X509Certificate::from_der(raw_cert.as_ref())
        .expect("Failed to parse certificate from DER format.")
        .1
}

#[self_referencing]
#[derive(Debug)]
pub struct WrappedX509 {
    raw_cert: Vec<u8>,

    #[borrows(raw_cert)]
    #[covariant]
    cert: X509Certificate<'this>,
}

impl WrappedX509 {
    pub fn not_after(&self) -> String {
        self.borrow_cert().validity.not_after.to_string()
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
        write!(f, ", expire: {}", get_not_after(leaf))
        // ignore the details of the private key
    }
}

impl Hash for X509Pem {
    fn hash<H: Hasher>(&self, state: &mut H) {
        for certificate in &self.certs {
            if let Ok(serial) = get_serial(certificate) {
                serial.hash(state)
            }
        }
    }
}

impl Hash for CertKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.pem.hash(state)
    }
}
