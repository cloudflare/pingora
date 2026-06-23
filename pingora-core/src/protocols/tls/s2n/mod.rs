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

pub mod client;
pub mod server;
mod stream;

use std::{
    hash::{Hash, Hasher},
    sync::Arc,
};

use pingora_s2n::{
    Config, Connection, ConnectionBuilder, Mode, Psk as S2NPsk, PskHmac, S2NError, S2NPolicy,
};
pub use stream::*;

use crate::utils::tls::X509Pem;

pub type CaType = X509Pem;

pub type PskType = PskConfig;

#[derive(Debug)]
pub struct PskConfig {
    pub keys: Vec<Psk>,
}

impl PskConfig {
    pub fn new(keys: Vec<Psk>) -> Self {
        Self { keys }
    }
}

impl Hash for PskConfig {
    fn hash<H: Hasher>(&self, state: &mut H) {
        for psk in self.keys.iter() {
            psk.identity.hash(state);
            psk.secret.hash(state);
        }
    }
}

#[derive(Debug)]
pub struct Psk {
    pub identity: Vec<u8>,
    pub secret: Vec<u8>,
    pub hmac: PskHmac,
}

impl Psk {
    pub fn new(identity: String, secret: Vec<u8>, hmac: PskHmac) -> Self {
        Self {
            identity: identity.into_bytes(),
            secret,
            hmac,
        }
    }
}

pub struct TlsRef;

/// Custom s2n-tls connection builder. The s2n-tls-tokio crate doesn't expose
/// a higher level api to configure private shared keys on a TLS connection.
///
/// This builder will create a new connection and configure it with the appropriate
/// psk configurations based on the provided private shared keys.
/// ```
#[derive(Debug, Clone)]
pub struct S2NConnectionBuilder {
    pub config: Config,
    pub psk_config: Option<Arc<PskConfig>>,
    pub security_policy: Option<S2NPolicy>,
}

impl ConnectionBuilder for S2NConnectionBuilder {
    type Output = Connection;
    fn build_connection(&self, mode: Mode) -> std::result::Result<Self::Output, S2NError> {
        let mut conn = Connection::new(mode);
        conn.set_config(self.config.clone())?;

        if let Some(psk_config) = &self.psk_config {
            for psk in psk_config.keys.iter() {
                let mut psk_builder = S2NPsk::builder()?;
                psk_builder.set_identity(&psk.identity)?;
                psk_builder.set_hmac(PskHmac::SHA256)?;
                psk_builder.set_secret(&psk.secret)?;
                conn.append_psk(&psk_builder.build()?)?;
            }
        }

        if let Some(policy) = &self.security_policy {
            conn.set_security_policy(policy)?;
        }

        Ok(conn)
    }
}
