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

#![warn(clippy::all)]

use std::fs::File;
use std::io::BufReader;

use log::{error, warn};
pub use no_debug::{Ellipses, NoDebug, WithTypeInfo};
pub use rustls::{version, ClientConfig, RootCertStore, ServerConfig, Stream};
pub use rustls_native_certs::load_native_certs;
use rustls_pemfile::Item;
pub use rustls_pki_types::{CertificateDer, PrivateKeyDer, ServerName};
pub use tokio_rustls::client::TlsStream as ClientTlsStream;
pub use tokio_rustls::server::TlsStream as ServerTlsStream;
pub use tokio_rustls::{Accept, Connect, TlsAcceptor, TlsConnector, TlsStream};

fn load_file(path: &String) -> BufReader<File> {
    let file = File::open(path).expect("io error");
    BufReader::new(file)
}
fn load_pem_file(path: &String) -> Result<Vec<Item>, std::io::Error> {
    let iter: Vec<Item> = rustls_pemfile::read_all(&mut load_file(path))
        .filter_map(|f| {
            if let Ok(f) = f {
                Some(f)
            } else {
                let err = f.err().unwrap();
                warn!(
                    "Skipping PEM element in file \"{}\" due to error \"{}\"",
                    path, err
                );
                None
            }
        })
        .collect();
    Ok(iter)
}

pub fn load_ca_file_into_store(path: &String, cert_store: &mut RootCertStore) {
    let ca_file = load_pem_file(path);
    match ca_file {
        Ok(cas) => {
            cas.into_iter().for_each(|pem_item| {
                // only loading certificates, handling a CA file
                match pem_item {
                    Item::X509Certificate(content) => match cert_store.add(content) {
                        Ok(_) => {}
                        Err(err) => {
                            error!("{}", err)
                        }
                    },
                    Item::Pkcs1Key(_) => {}
                    Item::Pkcs8Key(_) => {}
                    Item::Sec1Key(_) => {}
                    Item::Crl(_) => {}
                    Item::Csr(_) => {}
                    _ => {}
                }
            });
        }
        Err(err) => {
            error!(
                "Failed to load configured ca file located at \"{}\", error: \"{}\"",
                path, err
            );
        }
    }
}

pub fn load_platform_certs_incl_env_into_store(ca_certs: &mut RootCertStore) {
    // this includes handling of ENV vars SSL_CERT_FILE & SSL_CERT_DIR
    let native_platform_certs = load_native_certs();
    match native_platform_certs {
        Ok(certs) => {
            for cert in certs {
                ca_certs.add(cert).unwrap();
            }
        }
        Err(err) => {
            error!(
                "Failed to load native platform ca-certificates: \"{:?}\". Continuing without ...",
                err
            );
        }
    }
}

pub fn load_certs_key_file<'a>(
    cert: &String,
    key: &String,
) -> Option<(Vec<CertificateDer<'a>>, PrivateKeyDer<'a>)> {
    let certs_file = load_pem_file(cert)
        .unwrap_or_else(|_| panic!("Failed to load configured cert file located at {}.", cert));
    let key_file = load_pem_file(key)
        .unwrap_or_else(|_| panic!("Failed to load configured key file located at {}.", cert));

    let mut certs: Vec<CertificateDer<'a>> = vec![];
    certs_file.into_iter().for_each(|i| {
        if let Item::X509Certificate(cert) = i {
            certs.push(cert)
        }
    });

    let private_key = match key_file.into_iter().next()? {
        Item::Pkcs1Key(key) => Some(PrivateKeyDer::from(key)),
        Item::Pkcs8Key(key) => Some(PrivateKeyDer::from(key)),
        Item::Sec1Key(key) => Some(PrivateKeyDer::from(key)),
        _ => None,
    };

    if certs.is_empty() || private_key.is_none() {
        None
    } else {
        Some((certs, private_key?))
    }
}

pub fn load_pem_file_ca(path: &String) -> Vec<u8> {
    let mut reader = load_file(path);
    let cas_file = rustls_pemfile::certs(&mut reader);
    let ca = cas_file.into_iter().find_map(|pem_item| {
        if let Ok(item) = pem_item {
            Some(item)
        } else {
            None
        }
    });
    match ca {
        None => Vec::new(),
        Some(ca) => ca.to_vec(),
    }
}

pub fn load_pem_file_private_key(path: &String) -> Vec<u8> {
    let key = rustls_pemfile::private_key(&mut load_file(path));
    if let Ok(Some(key)) = key {
        return key.secret_der().to_vec();
    }
    Vec::new()
}

pub fn hash_certificate(cert: CertificateDer) -> Vec<u8> {
    let hash = ring::digest::digest(&ring::digest::SHA256, cert.as_ref());
    hash.as_ref().to_vec()
}
