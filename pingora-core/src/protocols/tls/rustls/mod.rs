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

pub mod client;
pub mod server;
mod stream;

pub use stream::*;
use x509_parser::prelude::FromDer;

pub type CaType = [Box<CertWrapper>];

#[derive(Debug)]
#[repr(transparent)]
pub struct CertWrapper(pub [u8]);

impl CertWrapper {
    pub fn not_after(&self) -> String {
        let (_, x509cert) = x509_parser::certificate::X509Certificate::from_der(&self.0)
            .expect("Failed to parse certificate from DER format.");
        x509cert.validity.not_after.to_string()
    }
}

pub struct TlsRef;
