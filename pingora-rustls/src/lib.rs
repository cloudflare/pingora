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

//! This module contains all the rustls specific pingora integration for things
//! like loading certificates and private keys

#![warn(clippy::all)]

use std::fs::File;
use std::io::BufReader;
use std::path::Path;

use log::warn;
pub use no_debug::{Ellipses, NoDebug, WithTypeInfo};
use pingora_error::{Error, ErrorType, OrErr, Result};

pub use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
pub use rustls::server::{
    ClientCertVerifierBuilder, ClientHello, ResolvesServerCert, WebPkiClientVerifier,
};
pub use rustls::sign;
pub use rustls::{
    client::WebPkiServerVerifier, crypto::CryptoProvider, version, CertificateError, ClientConfig,
    DigitallySignedStruct, Error as RusTlsError, KeyLogFile, RootCertStore, ServerConfig,
    SignatureScheme, Stream,
};

/// Install the default `ring` CryptoProvider for rustls.
///
/// rustls 0.23+ requires an explicit provider. This function installs `ring`
/// as the process-level default. Safe to call multiple times — subsequent
/// calls are no-ops.
pub fn install_default_crypto_provider() {
    let _ = CryptoProvider::install_default(rustls::crypto::ring::default_provider());
}
pub use rustls_native_certs::load_native_certs;
use rustls_pki_types::pem::{PemObject, SectionKind};
pub use rustls_pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
pub use tokio_rustls::client::TlsStream as ClientTlsStream;
pub use tokio_rustls::server::TlsStream as ServerTlsStream;
pub use tokio_rustls::{Accept, Connect, TlsAcceptor, TlsConnector, TlsStream};

// This allows to skip certificate verification. Be highly cautious.
pub use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};

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
fn load_pem_file<P>(path: P) -> Result<Vec<(SectionKind, Vec<u8>)>>
where
    P: AsRef<Path>,
{
    <(SectionKind, Vec<u8>)>::pem_reader_iter(load_file(path)?)
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
    for (kind, der) in load_pem_file(path)? {
        // only loading certificates, handling a CA file
        let Some(content) = CertificateDer::from_pem(kind, der) else {
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
    let native_certs = load_native_certs();
    if let Some(error) = native_certs.errors.into_iter().next() {
        return Error::e_because(
            ErrorType::InvalidCert,
            "Failed to load native certificates",
            error,
        );
    }

    for cert in native_certs.certs {
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
        .filter_map(|(kind, der)| CertificateDer::from_pem(kind, der))
        .collect::<Vec<_>>();

    // These are the currently supported pk types -
    // [https://doc.servo.org/rustls/key/struct.PrivateKey.html]
    let private_key_opt = key_file
        .into_iter()
        .filter_map(|(kind, der)| PrivateKeyDer::from_pem(kind, der))
        .next();

    if let (Some(private_key), false) = (private_key_opt, certs.is_empty()) {
        Ok(Some((certs, private_key)))
    } else {
        Ok(None)
    }
}

/// Load the certificate
pub fn load_pem_file_ca(path: &String) -> Result<Vec<u8>> {
    let cas_file_items = CertificateDer::pem_reader_iter(load_file(path)?)
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
    Ok(PrivateKeyDer::pem_reader_iter(load_file(path)?)
        .next()
        .transpose()
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
