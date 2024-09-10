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

use crate::protocols::tls::server::TlsAcceptCallbacks;
use crate::protocols::tls::TlsStream;
pub use crate::protocols::tls::ALPN;
use crate::protocols::IO;
#[cfg(not(feature = "rustls"))]
use crate::{
    listeners::tls::boringssl_openssl::{TlsAcc, TlsAcceptorBuil},
    protocols::tls::boringssl_openssl::server::{handshake, handshake_with_callback},
};
#[cfg(feature = "rustls")]
use crate::{
    listeners::tls::rustls::{TlsAcc, TlsAcceptorBuil},
    protocols::tls::rustls::server::{handshake, handshake_with_callback},
};
use log::debug;
use pingora_error::Result;
use std::ops::{Deref, DerefMut};

#[cfg(not(feature = "rustls"))]
pub(crate) mod boringssl_openssl;
#[cfg(feature = "rustls")]
pub(crate) mod rustls;

pub struct Acceptor {
    tls_acceptor: TlsAcc,
    callbacks: Option<TlsAcceptCallbacks>,
}

/// The TLS settings of a listening endpoint
pub struct TlsSettings {
    accept_builder: TlsAcceptorBuil,
    callbacks: Option<TlsAcceptCallbacks>,
}

// NOTE: keeping trait for documentation purpose
// switched to direct implementations to eliminate redirections in within the call-graph
// the below trait needs to be implemented for TlsAcceptorBuil
pub trait TlsAcceptorBuilder {
    type TlsAcceptor; // TlsAcc
    fn build(self) -> Self::TlsAcceptor;
    fn set_alpn(&mut self, alpn: ALPN);
    fn acceptor_intermediate(cert_path: &str, key_path: &str) -> Result<Self>
    where
        Self: Sized;
    fn acceptor_with_callbacks() -> Result<Self>
    where
        Self: Sized;
}

impl TlsSettings {
    /// Create a new [`TlsSettings`] with the [Mozilla Intermediate](https://wiki.mozilla.org/Security/Server_Side_TLS#Intermediate_compatibility_.28recommended.29)
    /// server side TLS settings. Users can adjust the TLS settings after this object is created.
    /// Return error if the provided certificate and private key are invalid or not found.
    pub fn intermediate(cert_path: &str, key_path: &str) -> Result<Self> {
        Ok(TlsSettings {
            accept_builder: TlsAcceptorBuil::acceptor_intermediate(cert_path, key_path)?,
            callbacks: None,
        })
    }

    /// Create a new [`TlsSettings`] similar to [TlsSettings::intermediate()]. A struct that implements [TlsAcceptCallbacks]
    /// is needed to provide the certificate during the TLS handshake.
    pub fn with_callbacks(callbacks: TlsAcceptCallbacks) -> Result<Self> {
        Ok(TlsSettings {
            accept_builder: TlsAcceptorBuil::acceptor_with_callbacks()?,
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
        self.accept_builder.set_alpn(alpn);
    }

    pub(crate) fn build(self) -> Acceptor {
        Acceptor {
            tls_acceptor: self.accept_builder.build(),
            callbacks: self.callbacks,
        }
    }
}

impl Acceptor {
    pub async fn handshake<S: IO>(&self, stream: S) -> Result<TlsStream<S>> {
        debug!("new tls session");
        // TODO: be able to offload this handshake in a thread pool
        if let Some(cb) = self.callbacks.as_ref() {
            handshake_with_callback(&self.tls_acceptor.0, stream, cb).await
        } else {
            handshake(&self.tls_acceptor.0, stream).await
        }
    }
}

impl Deref for TlsSettings {
    type Target = TlsAcceptorBuil;

    fn deref(&self) -> &Self::Target {
        &self.accept_builder
    }
}

impl DerefMut for TlsSettings {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.accept_builder
    }
}
