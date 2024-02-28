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

// TODO: move below to its own mod
use crate::tls::{nid::Nid, pkey::PKey, pkey::Private, x509::X509};
use crate::Result;
use bytes::Bytes;
use pingora_error::{ErrorType::*, OrErr};
use std::hash::{Hash, Hasher};

/// A `BufRef` is a reference to a buffer of bytes. It removes the need for self-referential data
/// structures. It is safe to use as long as the underlying buffer does not get mutated.
///
/// # Panics
///
/// This will panic if an index is out of bounds.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct BufRef(pub usize, pub usize);

impl BufRef {
    /// Return a sub-slice of `buf`.
    pub fn get<'a>(&self, buf: &'a [u8]) -> &'a [u8] {
        &buf[self.0..self.1]
    }

    /// Return a slice of `buf`. This operation is O(1) and increases the reference count of `buf`.
    pub fn get_bytes(&self, buf: &Bytes) -> Bytes {
        buf.slice(self.0..self.1)
    }

    /// Return the size of the slice reference.
    pub fn len(&self) -> usize {
        self.1 - self.0
    }

    /// Return true if the length is zero.
    pub fn is_empty(&self) -> bool {
        self.1 == self.0
    }
}

impl BufRef {
    /// Initialize a `BufRef` that can reference a slice beginning at index `start` and has a
    /// length of `len`.
    pub fn new(start: usize, len: usize) -> Self {
        BufRef(start, start + len)
    }
}

/// A `KVRef` contains a key name and value pair, stored as two [BufRef] types.
#[derive(Clone)]
pub struct KVRef {
    name: BufRef,
    value: BufRef,
}

impl KVRef {
    /// Like [BufRef::get] for the name.
    pub fn get_name<'a>(&self, buf: &'a [u8]) -> &'a [u8] {
        self.name.get(buf)
    }

    /// Like [BufRef::get] for the value.
    pub fn get_value<'a>(&self, buf: &'a [u8]) -> &'a [u8] {
        self.value.get(buf)
    }

    /// Like [BufRef::get_bytes] for the name.
    pub fn get_name_bytes(&self, buf: &Bytes) -> Bytes {
        self.name.get_bytes(buf)
    }

    /// Like [BufRef::get_bytes] for the value.
    pub fn get_value_bytes(&self, buf: &Bytes) -> Bytes {
        self.value.get_bytes(buf)
    }

    /// Return a new `KVRef` with name and value start indices and lengths.
    pub fn new(name_s: usize, name_len: usize, value_s: usize, value_len: usize) -> Self {
        KVRef {
            name: BufRef(name_s, name_s + name_len),
            value: BufRef(value_s, value_s + value_len),
        }
    }

    /// Return a reference to the value.
    pub fn value(&self) -> &BufRef {
        &self.value
    }
}

/// A [KVRef] which contains empty sub-slices.
pub const EMPTY_KV_REF: KVRef = KVRef {
    name: BufRef(0, 0),
    value: BufRef(0, 0),
};

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
