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

use ouroboros::self_referencing;
use pingora_error::Result;
use pingora_rustls::CertificateDer;
use std::hash::{Hash, Hasher};
use x509_parser::prelude::{FromDer, X509Certificate};

/// Get the organization and serial number associated with the given certificate
/// see https://en.wikipedia.org/wiki/X.509#Structure_of_a_certificate
pub fn get_organization_serial(x509cert: &WrappedX509) -> Result<(Option<String>, String)> {
    let serial = get_serial(x509cert)?;
    Ok((get_organization(x509cert), serial))
}

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
    certificates: Vec<WrappedX509>,
}

#[self_referencing]
#[derive(Debug)]
pub struct WrappedX509 {
    raw_cert: Vec<u8>,

    #[borrows(raw_cert)]
    #[covariant]
    cert: X509Certificate<'this>,
}

fn parse_x509<C>(raw_cert: &C) -> X509Certificate<'_>
where
    C: AsRef<[u8]>,
{
    X509Certificate::from_der(raw_cert.as_ref())
        .expect("Failed to parse certificate from DER format.")
        .1
}

impl Clone for CertKey {
    fn clone(&self) -> Self {
        CertKey {
            key: self.key.clone(),
            certificates: self
                .certificates
                .iter()
                .map(|wrapper| WrappedX509::new(wrapper.borrow_raw_cert().clone(), parse_x509))
                .collect::<Vec<_>>(),
        }
    }
}

impl CertKey {
    /// Create a new `CertKey` given a list of certificates and a private key.
    pub fn new(certificates: Vec<Vec<u8>>, key: Vec<u8>) -> CertKey {
        assert!(
            !certificates.is_empty() && !certificates.first().unwrap().is_empty(),
            "expected a non-empty vector of certificates in CertKey::new"
        );

        CertKey {
            key,
            certificates: certificates
                .into_iter()
                .map(|raw_cert| WrappedX509::new(raw_cert, parse_x509))
                .collect::<Vec<_>>(),
        }
    }

    /// Peek at the leaf certificate.
    pub fn leaf(&self) -> &WrappedX509 {
        // This is safe due to the assertion in creation of a `CertKey`
        &self.certificates[0]
    }

    /// Return the key.
    pub fn key(&self) -> &Vec<u8> {
        &self.key
    }

    /// Return a slice of intermediate certificates. An empty slice means there are none.
    pub fn intermediates(&self) -> Vec<&WrappedX509> {
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

impl Hash for CertKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        for certificate in &self.certificates {
            if let Ok(serial) = get_serial(certificate) {
                serial.hash(state)
            }
        }
    }
}

impl<'a> From<&'a WrappedX509> for CertificateDer<'static> {
    fn from(value: &'a WrappedX509) -> Self {
        CertificateDer::from(value.borrow_raw_cert().as_slice().to_owned())
    }
}
