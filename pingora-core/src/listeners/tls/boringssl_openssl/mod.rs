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

//! BoringSSL & OpenSSL listener specific implementation

use crate::listeners::tls::{NativeBuilder, TlsAcceptor, TlsAcceptorBuilder};
use crate::listeners::{TlsSettings, ALPN};
use crate::tls::ssl::{SslAcceptor, SslAcceptorBuilder, SslFiletype, SslMethod};
use async_trait::async_trait;
use core::any::Any;
use pingora_error::{ErrorType, OrErr, Result};
const TLS_CONF_ERR: ErrorType = ErrorType::Custom("TLSConfigError");

struct TlsAcc(SslAcceptor);
pub(super) struct TlsAcceptorBuil(SslAcceptorBuilder);

#[async_trait]
impl TlsAcceptor for TlsAcc {
    fn get_acceptor(&self) -> &dyn Any {
        &self.0
    }
}

impl TlsAcceptorBuilder for TlsAcceptorBuil {
    fn build(self: Box<Self>) -> Box<dyn TlsAcceptor + Send + Sync> {
        let builder = (*self).0;
        Box::new(TlsAcc(SslAcceptorBuilder::build(builder)))
    }

    fn set_alpn(&mut self, alpn: ALPN) {
        match alpn {
            ALPN::H2H1 => self.0.set_alpn_select_callback(alpn::prefer_h2),
            ALPN::H1 => self.0.set_alpn_select_callback(alpn::h1_only),
            ALPN::H2 => self.0.set_alpn_select_callback(alpn::h2_only),
        }
    }

    fn acceptor_intermediate(cert_path: &str, key_path: &str) -> Result<Self>
    where
        Self: Sized,
    {
        let mut accept_builder = SslAcceptor::mozilla_intermediate_v5(SslMethod::tls()).or_err(
            TLS_CONF_ERR,
            "fail to create mozilla_intermediate_v5 Acceptor",
        )?;
        accept_builder
            .set_private_key_file(key_path, SslFiletype::PEM)
            .or_err_with(TLS_CONF_ERR, || format!("fail to read key file {key_path}"))?;
        accept_builder
            .set_certificate_chain_file(cert_path)
            .or_err_with(TLS_CONF_ERR, || {
                format!("fail to read cert file {cert_path}")
            })?;
        Ok(TlsAcceptorBuil(accept_builder))
    }

    fn acceptor_with_callbacks() -> Result<Self>
    where
        Self: Sized,
    {
        let accept_builder = SslAcceptor::mozilla_intermediate_v5(SslMethod::tls()).or_err(
            TLS_CONF_ERR,
            "fail to create mozilla_intermediate_v5 Acceptor",
        )?;
        Ok(TlsAcceptorBuil(accept_builder))
    }

    fn as_any(&mut self) -> &mut dyn Any {
        self as &mut dyn Any
    }
}

impl NativeBuilder for Box<dyn TlsAcceptorBuilder + Send + Sync> {
    type Builder = SslAcceptorBuilder;

    fn native(&mut self) -> &mut Self::Builder {
        self.as_any().downcast_mut::<Self::Builder>().unwrap()
    }
}

impl From<SslAcceptorBuilder> for TlsSettings {
    fn from(settings: SslAcceptorBuilder) -> Self {
        TlsSettings {
            accept_builder: Box::new(TlsAcceptorBuil(settings)),
            callbacks: None,
        }
    }
}

mod alpn {
    use crate::protocols::ALPN;
    use crate::tls::ssl::{select_next_proto, AlpnError, SslRef};

    // A standard implementation provided by the SSL lib is used below

    pub fn prefer_h2<'a>(_ssl: &mut SslRef, alpn_in: &'a [u8]) -> Result<&'a [u8], AlpnError> {
        match select_next_proto(ALPN::H2H1.to_wire_preference(), alpn_in) {
            Some(p) => Ok(p),
            _ => Err(AlpnError::NOACK), // unknown ALPN, just ignore it. Most clients will fallback to h1
        }
    }

    pub fn h1_only<'a>(_ssl: &mut SslRef, alpn_in: &'a [u8]) -> Result<&'a [u8], AlpnError> {
        match select_next_proto(ALPN::H1.to_wire_preference(), alpn_in) {
            Some(p) => Ok(p),
            _ => Err(AlpnError::NOACK), // unknown ALPN, just ignore it. Most clients will fallback to h1
        }
    }

    pub fn h2_only<'a>(_ssl: &mut SslRef, alpn_in: &'a [u8]) -> Result<&'a [u8], AlpnError> {
        match select_next_proto(ALPN::H2.to_wire_preference(), alpn_in) {
            Some(p) => Ok(p),
            _ => Err(AlpnError::ALERT_FATAL), // cannot agree
        }
    }
}
