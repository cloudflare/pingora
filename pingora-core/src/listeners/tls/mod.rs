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

use std::any::Any;

use async_trait::async_trait;
use log::debug;

#[cfg(not(feature = "rustls"))]
use crate::listeners::tls::boringssl_openssl::TlsAcceptorBuil;
#[cfg(feature = "rustls")]
use crate::listeners::tls::rustls::TlsAcceptorBuil;
#[cfg(not(feature = "rustls"))]
use crate::protocols::tls::boringssl_openssl::server::{handshake, handshake_with_callback};
#[cfg(feature = "rustls")]
use crate::protocols::tls::rustls::server::{handshake, handshake_with_callback};
use crate::protocols::tls::server::TlsAcceptCallbacks;
use crate::protocols::tls::TlsStream;
pub use crate::protocols::tls::ALPN;
use crate::protocols::IO;
use pingora_error::Result;

#[cfg(not(feature = "rustls"))]
pub mod boringssl_openssl;
#[cfg(feature = "rustls")]
pub(crate) mod rustls;

pub struct Acceptor {
    ssl_acceptor: Box<dyn TlsAcceptor + Send + Sync>,
    callbacks: Option<TlsAcceptCallbacks>,
}

#[async_trait]
pub trait TlsAcceptor {
    fn get_acceptor(&self) -> &dyn Any;
}

/// The TLS settings of a listening endpoint
pub struct TlsSettings {
    accept_builder: Box<dyn TlsAcceptorBuilder + Send + Sync>,
    callbacks: Option<TlsAcceptCallbacks>,
}

pub trait TlsAcceptorBuilder: Any {
    fn build(self: Box<Self>) -> Box<dyn TlsAcceptor + Send + Sync>;
    fn set_alpn(&mut self, alpn: ALPN);
    fn acceptor_intermediate(cert_path: &str, key_path: &str) -> Result<Self>
    where
        Self: Sized;
    fn acceptor_with_callbacks() -> Result<Self>
    where
        Self: Sized;
    fn as_any(&mut self) -> &mut dyn Any;
}

pub trait NativeBuilder {
    type Builder;
    fn native(&mut self) -> &mut Self::Builder;
}

impl TlsSettings {
    /// Create a new [`TlsSettings`] with the [Mozilla Intermediate](https://wiki.mozilla.org/Security/Server_Side_TLS#Intermediate_compatibility_.28recommended.29)
    /// server side TLS settings. Users can adjust the TLS settings after this object is created.
    /// Return error if the provided certificate and private key are invalid or not found.
    pub fn intermediate(cert_path: &str, key_path: &str) -> Result<Self> {
        Ok(TlsSettings {
            accept_builder: Box::new(TlsAcceptorBuil::acceptor_intermediate(cert_path, key_path)?),
            callbacks: None,
        })
    }

    /// Create a new [`TlsSettings`] similar to [TlsSettings::intermediate()]. A struct that implements [TlsAcceptCallbacks]
    /// is needed to provide the certificate during the TLS handshake.
    pub fn with_callbacks(callbacks: TlsAcceptCallbacks) -> Result<Self> {
        Ok(TlsSettings {
            accept_builder: Box::new(TlsAcceptorBuil::acceptor_with_callbacks()?),
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
            ssl_acceptor: self.accept_builder.build(),
            callbacks: self.callbacks,
        }
    }

    pub fn get_builder(&mut self) -> &mut Box<dyn TlsAcceptorBuilder + Send + Sync> {
        &mut (self.accept_builder)
    }
}

impl Acceptor {
    pub async fn handshake(
        &self,
        stream: Box<dyn IO + Send>,
    ) -> Result<TlsStream<Box<dyn IO + Send>>> {
        debug!("new tls session");
        // TODO: be able to offload this handshake in a thread pool
        if let Some(cb) = self.callbacks.as_ref() {
            handshake_with_callback(self, stream, cb).await
        } else {
            handshake(self, stream).await
        }
    }

    pub(crate) fn inner(&self) -> &dyn Any {
        self.ssl_acceptor.get_acceptor()
    }
}
