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

//! Rustls TLS listener specific implementation

use std::any::Any;
use std::sync::Arc;

use async_trait::async_trait;

use pingora_error::ErrorType::InternalError;
use pingora_error::{Error, ErrorSource, ImmutStr, OrErr, Result};
use pingora_rustls::load_certs_key_file;
use pingora_rustls::ServerConfig;
use pingora_rustls::{version, TlsAcceptor as RusTlsAcceptor};

use crate::listeners::tls::{TlsAcceptor, TlsAcceptorBuilder};
use crate::listeners::ALPN;

pub(super) struct TlsAcceptorBuil {
    alpn_protocols: Option<Vec<Vec<u8>>>,
    cert_path: String,
    key_path: String,
}

struct TlsAcc {
    acceptor: RusTlsAcceptor,
}

#[async_trait]
impl TlsAcceptor for TlsAcc {
    fn get_acceptor(&self) -> &dyn Any {
        &self.acceptor
    }
}

impl TlsAcceptorBuilder for TlsAcceptorBuil {
    fn build(self: Box<Self>) -> Box<dyn TlsAcceptor + Send + Sync> {
        let (certs, key) = load_certs_key_file(&self.cert_path, &self.key_path).expect(
            format!(
                "Failed to load provided certificates \"{}\" or key \"{}\".",
                self.cert_path, self.key_path
            )
            .as_str(),
        );

        let mut config =
            ServerConfig::builder_with_protocol_versions(&vec![&version::TLS12, &version::TLS13])
                .with_no_client_auth()
                .with_single_cert(certs, key)
                .explain_err(InternalError, |e| {
                    format!("Failed to create server listener config: {}", e)
                })
                .unwrap();

        if let Some(alpn_protocols) = self.alpn_protocols {
            config.alpn_protocols = alpn_protocols;
        }

        Box::new(TlsAcc {
            acceptor: RusTlsAcceptor::from(Arc::new(config)),
        })
    }
    fn set_alpn(&mut self, alpn: ALPN) {
        self.alpn_protocols = Some(alpn.to_wire_protocols());
    }

    fn acceptor_intermediate(cert_path: &str, key_path: &str) -> Result<Self>
    where
        Self: Sized,
    {
        Ok(TlsAcceptorBuil {
            alpn_protocols: None,
            cert_path: cert_path.to_string(),
            key_path: key_path.to_string(),
        })
    }

    fn acceptor_with_callbacks() -> Result<Self>
    where
        Self: Sized,
    {
        // TODO: verify if/how callback in handshake can be done using Rustls
        Err(Error::create(
            InternalError,
            ErrorSource::Internal,
            Some(ImmutStr::from(
                "Certificate callbacks are not supported with feature \"rustls\".",
            )),
            None,
        ))
    }

    fn as_any(&mut self) -> &mut dyn Any {
        self as &mut dyn Any
    }
}
