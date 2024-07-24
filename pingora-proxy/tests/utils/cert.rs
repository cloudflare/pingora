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

use once_cell::sync::Lazy;
use std::fs;

#[cfg(feature = "rustls")]
use pingora_core::tls::{load_pem_file_ca, load_pem_file_private_key};
#[cfg(not(feature = "rustls"))]
use pingora_core::tls::{
    pkey::{PKey, Private},
    x509::X509,
};

//pub static ROOT_CERT: Lazy<X509> = Lazy::new(|| load_cert("keys/root.crt"));
//pub static ROOT_KEY: Lazy<PKey<Private>> = Lazy::new(|| load_key("keys/root.key"));
pub static INTERMEDIATE_CERT: Lazy<Vec<u8>> = Lazy::new(|| load_cert("keys/intermediate.crt"));
//pub static INTERMEDIATE_KEY: Lazy<PKey<Private>> = Lazy::new(|| load_key("keys/intermediate.key"));
pub static LEAF_CERT: Lazy<Vec<u8>> = Lazy::new(|| load_cert("keys/leaf.crt"));
pub static LEAF2_CERT: Lazy<Vec<u8>> = Lazy::new(|| load_cert("keys/leaf2.crt"));
pub static LEAF_KEY: Lazy<Vec<u8>> = Lazy::new(|| load_key("keys/leaf.key"));
pub static LEAF2_KEY: Lazy<Vec<u8>> = Lazy::new(|| load_key("keys/leaf2.key"));
//pub static SERVER_CERT: Lazy<X509> = Lazy::new(|| load_cert("keys/server.crt"));
//pub static SERVER_KEY: Lazy<PKey<Private>> = Lazy::new(|| load_key("keys/key.pem"));
pub static CURVE_521_TEST_KEY: Lazy<Vec<u8>> =
    Lazy::new(|| load_key("keys/curve_test.521.key.pem"));
pub static CURVE_521_TEST_CERT: Lazy<Vec<u8>> = Lazy::new(|| load_cert("keys/curve_test.521.crt"));
pub static CURVE_384_TEST_KEY: Lazy<Vec<u8>> =
    Lazy::new(|| load_key("keys/curve_test.384.key.pem"));
pub static CURVE_384_TEST_CERT: Lazy<Vec<u8>> = Lazy::new(|| load_cert("keys/curve_test.384.crt"));

#[cfg(not(feature = "rustls"))]
fn load_cert(path: &str) -> Vec<u8> {
    let path = format!("{}/{path}", super::conf_dir());
    let cert_bytes = fs::read(path).unwrap();
    X509::from_pem(&cert_bytes).unwrap().to_der().unwrap()
}
#[cfg(not(feature = "rustls"))]
fn load_key(path: &str) -> Vec<u8> {
    let path = format!("{}/{path}", super::conf_dir());
    let key_bytes = fs::read(path).unwrap();
    PKey::private_key_from_pem(&key_bytes)
        .unwrap()
        .private_key_to_der()
        .unwrap()
}

#[cfg(feature = "rustls")]
fn load_cert(path: &str) -> Vec<u8> {
    let path = format!("{}/{path}", super::conf_dir());
    load_pem_file_ca(&path)
}

#[cfg(feature = "rustls")]
fn load_key(path: &str) -> Vec<u8> {
    let path = format!("{}/{path}", super::conf_dir());
    load_pem_file_private_key(&path)
}
