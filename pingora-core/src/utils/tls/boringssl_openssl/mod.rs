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

//! This module contains various helpers that make it easier to work with X509 certificates.

use pingora_error::ErrorType::{InternalError, InvalidCert};
use pingora_error::OrErr;

use crate::tls::nid::Nid;
use crate::tls::pkey::{PKey, Private};
use crate::tls::x509::X509;

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
pub fn get_x509_organization(cert: &X509) -> Option<String> {
    get_subject_name(cert, Nid::ORGANIZATIONNAME)
}

/// Return the organization associated with the X509 certificate.
pub fn get_organization(cert: &[u8]) -> Option<String> {
    let cert = der_to_x509(cert).unwrap();
    get_subject_name(&cert, Nid::ORGANIZATIONNAME)
}

/// Return the common name associated with the X509 certificate.
pub fn get_common_name(cert: &[u8]) -> Option<String> {
    let cert = der_to_x509(cert).unwrap();
    get_subject_name(&cert, Nid::COMMONNAME)
}

/// Return the organizational unit associated with the X509 certificate.
pub fn get_organizational_unit(cert: &[u8]) -> Option<String> {
    let cert = der_to_x509(cert).unwrap();
    get_subject_name(&cert, Nid::ORGANIZATIONALUNITNAME)
}

/// Return the common name associated with the X509 certificate.
pub fn get_not_after(cert: &[u8]) -> String {
    let cert = der_to_x509(cert).unwrap();
    cert.not_after().to_string()
}

/// Return the serial number associated with the X509 certificate as a hexadecimal value.
pub fn get_serial(cert: &[u8]) -> pingora_error::Result<String> {
    let cert = der_to_x509(cert).unwrap();
    let bn = cert
        .serial_number()
        .to_bn()
        .or_err(InvalidCert, "Invalid serial")?;
    let hex = bn.to_hex_str().or_err(InvalidCert, "Invalid serial")?;

    let hex_str: &str = hex.as_ref();
    Ok(hex_str.to_owned())
}

pub fn get_x509_serial(cert: &X509) -> pingora_error::Result<String> {
    let bn = cert
        .serial_number()
        .to_bn()
        .or_err(InvalidCert, "Invalid serial")?;
    let hex = bn.to_hex_str().or_err(InvalidCert, "Invalid serial")?;

    let hex_str: &str = hex.as_ref();
    Ok(hex_str.to_owned())
}

pub fn der_to_x509(ca: &[u8]) -> pingora_error::Result<X509> {
    let cert = X509::from_der(ca).explain_err(InvalidCert, |e| {
        format!(
            "Failed to convert ca certificate in DER form to X509 cert. Error: {:?}",
            e
        )
    })?;
    Ok(cert)
}

pub fn der_to_private_key(key: &[u8]) -> pingora_error::Result<PKey<Private>> {
    let key = PKey::private_key_from_der(key).explain_err(InternalError, |e| {
        format!(
            "Failed to convert private key in DER form to Pkey. Error: {:?}",
            e
        )
    })?;
    Ok(key)
}
