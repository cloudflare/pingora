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

//! This module contains all the rustls specific pingora integration for things
//! like loading certificates and private keys

#![warn(clippy::all)]

use std::fs::File;
use std::io::BufReader;
use std::path::Path;

use log::warn;
pub use no_debug::{Ellipses, NoDebug, WithTypeInfo};
use pingora_error::{Error, ErrorType, OrErr, Result};
pub use rustls::{version, ClientConfig, RootCertStore, ServerConfig, Stream};
pub use rustls_native_certs::load_native_certs;
use rustls_pemfile::Item;
pub use rustls_pki_types::{CertificateDer, PrivateKeyDer, ServerName};
pub use tokio_rustls::client::TlsStream as ClientTlsStream;
pub use tokio_rustls::server::TlsStream as ServerTlsStream;
pub use tokio_rustls::{Accept, Connect, TlsAcceptor, TlsConnector, TlsStream};

/// Load the given file from disk as a buffered reader and use the pingora Error
/// type instead of the std::io version
fn load_file<P>(path: P) -> Result<BufReader<File>>
where
    P: AsRef<Path>,
{
    File::open(path)
        .or_err(ErrorType::FileReadError, "Failed to load file")
        .map(BufReader::new)
}

/// Read the pem file at the given path from disk
fn load_pem_file<P>(path: P) -> Result<Vec<Item>>
where
    P: AsRef<Path>,
{
    rustls_pemfile::read_all(&mut load_file(path)?)
        .map(|item_res| {
            item_res.or_err(
                ErrorType::InvalidCert,
                "Certificate in pem file could not be read",
            )
        })
        .collect()
}

/// Load the certificates from the given pem file path into the given
/// certificate store
pub fn load_ca_file_into_store<P>(path: P, cert_store: &mut RootCertStore) -> Result<()>
where
    P: AsRef<Path>,
{
    for pem_item in load_pem_file(path)? {
        // only loading certificates, handling a CA file
        let Item::X509Certificate(content) = pem_item else {
            return Error::e_explain(
                ErrorType::InvalidCert,
                "Pem file contains un-loadable certificate type",
            );
        };
        cert_store.add(content).or_err(
            ErrorType::InvalidCert,
            "Failed to load X509 certificate into root store",
        )?;
    }

    Ok(())
}

/// Attempt to load the native cas into the given root-certificate store
pub fn load_platform_certs_incl_env_into_store(ca_certs: &mut RootCertStore) -> Result<()> {
    // this includes handling of ENV vars SSL_CERT_FILE & SSL_CERT_DIR
    for cert in load_native_certs()
        .or_err(ErrorType::InvalidCert, "Failed to load native certificates")?
        .into_iter()
    {
        ca_certs.add(cert).or_err(
            ErrorType::InvalidCert,
            "Failed to load native certificate into root store",
        )?;
    }

    Ok(())
}

/// Load the certificates and private key files
pub fn load_certs_and_key_files<'a>(
    cert: &str,
    key: &str,
) -> Result<Option<(Vec<CertificateDer<'a>>, PrivateKeyDer<'a>)>> {
    let certs_file = load_pem_file(cert)?;
    let key_file = load_pem_file(key)?;

    let certs = certs_file
        .into_iter()
        .filter_map(|item| {
            if let Item::X509Certificate(cert) = item {
                Some(cert)
            } else {
                None
            }
        })
        .collect::<Vec<_>>();

    // These are the currently supported pk types -
    // [https://doc.servo.org/rustls/key/struct.PrivateKey.html]
    let private_key_opt = key_file
        .into_iter()
        .filter_map(|key_item| match key_item {
            Item::Pkcs1Key(key) => Some(PrivateKeyDer::from(key)),
            Item::Pkcs8Key(key) => Some(PrivateKeyDer::from(key)),
            Item::Sec1Key(key) => Some(PrivateKeyDer::from(key)),
            _ => None,
        })
        .next();

    if let (Some(private_key), false) = (private_key_opt, certs.is_empty()) {
        Ok(Some((certs, private_key)))
    } else {
        Ok(None)
    }
}

/// Load the certificate
pub fn load_pem_file_ca(path: &String) -> Result<Vec<u8>> {
    let mut reader = load_file(path)?;
    let cas_file_items = rustls_pemfile::certs(&mut reader)
        .map(|item_res| {
            item_res.or_err(
                ErrorType::InvalidCert,
                "Failed to load certificate from file",
            )
        })
        .collect::<Result<Vec<_>>>()?;

    Ok(cas_file_items
        .first()
        .map(|ca| ca.to_vec())
        .unwrap_or_default())
}

pub fn load_pem_file_private_key(path: &String) -> Result<Vec<u8>> {
    Ok(rustls_pemfile::private_key(&mut load_file(path)?)
        .or_err(
            ErrorType::InvalidCert,
            "Failed to load private key from file",
        )?
        .map(|key| key.secret_der().to_vec())
        .unwrap_or_default())
}

pub fn hash_certificate(cert: &CertificateDer) -> Vec<u8> {
    let hash = ring::digest::digest(&ring::digest::SHA256, cert.as_ref());
    hash.as_ref().to_vec()
}
