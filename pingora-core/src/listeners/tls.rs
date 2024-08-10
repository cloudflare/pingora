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

use log::debug;
use pingora_error::{ErrorType, OrErr, Result};
use std::ops::{Deref, DerefMut};

use crate::protocols::ssl::{
    server::{handshake, handshake_with_callback, TlsAcceptCallbacks},
    SslStream,
};
use crate::protocols::IO;
use crate::tls::ssl::{SslAcceptor, SslAcceptorBuilder, SslFiletype, SslMethod};

pub use crate::protocols::ssl::ALPN;

pub const TLS_CONF_ERR: ErrorType = ErrorType::Custom("TLSConfigError");

pub(crate) struct Acceptor {
    ssl_acceptor: SslAcceptor,
    callbacks: Option<TlsAcceptCallbacks>,
}

/// The TLS settings of a listening endpoint
pub struct TlsSettings {
    accept_builder: SslAcceptorBuilder,
    callbacks: Option<TlsAcceptCallbacks>,
}

impl From<SslAcceptorBuilder> for TlsSettings {
    fn from(settings: SslAcceptorBuilder) -> Self {
        TlsSettings {
            accept_builder: settings,
            callbacks: None,
        }
    }
}

impl Deref for TlsSettings {
    type Target = SslAcceptorBuilder;

    fn deref(&self) -> &Self::Target {
        &self.accept_builder
    }
}

impl DerefMut for TlsSettings {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.accept_builder
    }
}

impl TlsSettings {
    /// Create a new [`TlsSettings`] with the [Mozilla Intermediate](https://wiki.mozilla.org/Security/Server_Side_TLS#Intermediate_compatibility_.28recommended.29)
    /// server side TLS settings. Users can adjust the TLS settings after this object is created.
    /// Return error if the provided certificate and private key are invalid or not found.
    pub fn intermediate(cert_path: &str, key_path: &str) -> Result<Self> {
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
        Ok(TlsSettings {
            accept_builder,
            callbacks: None,
        })
    }

    /// Create a new [`TlsSettings`] similar to [TlsSettings::intermediate()]. A struct that implements [TlsAcceptCallbacks]
    /// is needed to provide the certificate during the TLS handshake.
    pub fn with_callbacks(callbacks: TlsAcceptCallbacks) -> Result<Self> {
        let accept_builder = SslAcceptor::mozilla_intermediate_v5(SslMethod::tls()).or_err(
            TLS_CONF_ERR,
            "fail to create mozilla_intermediate_v5 Acceptor",
        )?;
        Ok(TlsSettings {
            accept_builder,
            callbacks: Some(callbacks),
        })
    }

    /// Enable HTTP/2 support for this endpoint, which is default off.
    /// This effectively sets the ALPN to prefer HTTP/2 with HTTP/1.1 allowed
    pub fn enable_h2(&mut self) {
        self.set_alpn(ALPN::H2H1);
    }

    /// Set the ALPN preference of this endpoint. See [`ALPN`] for more details
    pub fn set_alpn(&mut self, alpn: ALPN) {
        match alpn {
            ALPN::H2H1 => self
                .accept_builder
                .set_alpn_select_callback(alpn::prefer_h2),
            ALPN::H1 => self.accept_builder.set_alpn_select_callback(alpn::h1_only),
            ALPN::H2 => self.accept_builder.set_alpn_select_callback(alpn::h2_only),
        }
    }

    pub(crate) fn build(self) -> Acceptor {
        Acceptor {
            ssl_acceptor: self.accept_builder.build(),
            callbacks: self.callbacks,
        }
    }
}

impl Acceptor {
    pub async fn tls_handshake<S: IO>(&self, stream: S) -> Result<SslStream<S>> {
        debug!("new ssl session");
        // TODO: be able to offload this handshake in a thread pool
        if let Some(cb) = self.callbacks.as_ref() {
            handshake_with_callback(&self.ssl_acceptor, stream, cb).await
        } else {
            handshake(&self.ssl_acceptor, stream).await
        }
    }
}

mod alpn {
    use super::*;
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
