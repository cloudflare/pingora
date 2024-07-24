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

use pingora_error::Result;
use x509_parser::prelude::FromDer;

pub fn get_organization_serial(cert: &[u8]) -> (Option<String>, String) {
    let serial = get_serial(cert).expect("Failed to get serial for certificate.");
    (get_organization(cert), serial)
}

pub fn get_serial(cert: &[u8]) -> Result<String> {
    let (_, x509cert) = x509_parser::certificate::X509Certificate::from_der(cert)
        .expect("Failed to parse certificate from DER format.");
    Ok(x509cert.raw_serial_as_string())
}

/// Return the organization associated with the X509 certificate.
pub fn get_organization(cert: &[u8]) -> Option<String> {
    let (_, x509cert) = x509_parser::certificate::X509Certificate::from_der(cert)
        .expect("Failed to parse certificate from DER format.");
    x509cert
        .subject
        .iter_organization()
        .filter_map(|a| a.as_str().ok())
        .map(|a| a.to_string())
        .reduce(|cur, next| cur + &next)
}

/// Return the organization unit associated with the X509 certificate.
pub fn get_organizational_unit(cert: &[u8]) -> Option<String> {
    let (_, x509cert) = x509_parser::certificate::X509Certificate::from_der(cert)
        .expect("Failed to parse certificate from DER format.");
    x509cert
        .subject
        .iter_organizational_unit()
        .filter_map(|a| a.as_str().ok())
        .map(|a| a.to_string())
        .reduce(|cur, next| cur + &next)
}

/// Return the organization unit associated with the X509 certificate.
pub fn get_not_after(cert: &[u8]) -> String {
    let (_, x509cert) = x509_parser::certificate::X509Certificate::from_der(cert)
        .expect("Failed to parse certificate from DER format.");
    x509cert.validity.not_after.to_string()
}

pub fn get_common_name(cert: &[u8]) -> Option<String> {
    let (_, x509cert) = x509_parser::certificate::X509Certificate::from_der(cert)
        .expect("Failed to parse certificate from DER format.");
    x509cert
        .subject
        .iter_common_name()
        .filter_map(|a| a.as_str().ok())
        .map(|a| a.to_string())
        .reduce(|cur, next| cur + &next)
}
