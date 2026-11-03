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

pub use stream::*;

use crate::utils::tls::WrappedX509;
use pingora_rustls::{CertificateDer, NamedGroup};

pub type CaType = [WrappedX509];

/// TLS connection state exposed to post-handshake callbacks.
///
/// Provides access to peer certificates and negotiated TLS parameters
/// after a TLS handshake completes. This is the rustls equivalent of
/// the OpenSSL `SslRef` that is used as `TlsRef` in the boringssl/openssl path.
#[derive(Debug)]
pub struct TlsRef {
    pub(super) peer_certs: Option<Vec<CertificateDer<'static>>>,
    pub(super) cipher: Option<&'static str>,
    pub(super) version: Option<&'static str>,
    pub(super) server_name: Option<String>,
    pub(super) key_exchange_group: Option<NamedGroup>,
}

impl TlsRef {
    /// Returns the peer's leaf certificate in DER encoding, if present.
    pub fn peer_certificate_der(&self) -> Option<&[u8]> {
        self.peer_certs
            .as_ref()
            .and_then(|certs| certs.first())
            .map(|cert| cert.as_ref())
    }

    /// Returns the full peer certificate chain in DER encoding.
    /// The first entry is the leaf; subsequent entries are intermediates.
    pub fn peer_cert_chain_der(&self) -> Option<&[CertificateDer<'static>]> {
        self.peer_certs.as_deref()
    }

    /// Returns the negotiated cipher suite name, if available.
    pub fn current_cipher_name(&self) -> Option<&'static str> {
        self.cipher
    }

    /// Returns the negotiated key exchange group, if available.
    ///
    /// Returns `None` when no key exchange occurred, including resumed TLS 1.2
    /// handshakes. This reports the group for this handshake, not an earlier
    /// handshake that established a resumed session.
    ///
    /// A downstream [`TlsAccept`](crate::listeners::TlsAccept) callback can
    /// retain this value in an application-defined digest extension:
    ///
    /// ```
    /// use async_trait::async_trait;
    /// use pingora_core::listeners::TlsAccept;
    /// use pingora_core::protocols::tls::{SslDigest, TlsRef};
    /// use pingora_rustls::NamedGroup;
    /// use std::{any::Any, sync::Arc};
    ///
    /// struct Group(Option<NamedGroup>);
    /// struct CaptureGroup;
    ///
    /// #[async_trait]
    /// impl TlsAccept for CaptureGroup {
    ///     async fn handshake_complete_callback(
    ///         &self,
    ///         tls: &TlsRef,
    ///     ) -> Option<Arc<dyn Any + Send + Sync>> {
    ///         Some(Arc::new(Group(tls.negotiated_key_exchange_group())))
    ///     }
    /// }
    ///
    /// fn group_id(digest: &SslDigest) -> Option<u16> {
    ///     // Numeric identifiers also preserve groups without a known name.
    ///     digest.extension.get::<Group>()?.0.map(u16::from)
    /// }
    /// ```
    pub fn negotiated_key_exchange_group(&self) -> Option<NamedGroup> {
        self.key_exchange_group
    }

    /// Returns the negotiated TLS protocol version (e.g. "TLSv1.3"), if available.
    pub fn version(&self) -> Option<&'static str> {
        self.version
    }

    /// Returns the SNI hostname sent by the peer, if any. Only populated on
    /// server-side connections; always `None` on client streams.
    pub fn server_name(&self) -> Option<&str> {
        self.server_name.as_deref()
    }
}
