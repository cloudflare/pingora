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
use pingora_error::{Error, ErrorType::InvalidCert, Result};
use pingora_rustls::sign::SigningKey;
use pingora_rustls::CertificateDer;

pub type CaType = [WrappedX509];

/// Verify the leaf certificate's public key matches the given signing key.
/// Stands in for rustls's `with_single_cert` consistency check, which we
/// bypass to skip webpki policy validation on our own cert chain.
pub(crate) fn verify_cert_key_match(
    cert: &CertificateDer<'_>,
    signing_key: &dyn SigningKey,
) -> Result<()> {
    let Some(key_spki) = signing_key.public_key() else {
        return Ok(());
    };

    let (_, parsed) = x509_parser::parse_x509_certificate(cert.as_ref())
        .map_err(|_| Error::explain(InvalidCert, "Failed to parse certificate for key match"))?;

    if parsed.tbs_certificate.subject_pki.raw != key_spki.as_ref() {
        return Error::e_explain(
            InvalidCert,
            "Certificate public key does not match private key",
        );
    }
    Ok(())
}

/// TLS connection state exposed to post-handshake callbacks.
///
/// Provides access to peer certificates and negotiated cipher suite
/// after a TLS handshake completes. This is the rustls equivalent of
/// the OpenSSL `SslRef` that is used as `TlsRef` in the boringssl/openssl path.
pub struct TlsRef {
    /// Peer certificate chain (DER-encoded). The first entry is the leaf certificate.
    peer_certs: Option<Vec<CertificateDer<'static>>>,
    /// Negotiated cipher suite name (e.g. "TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256")
    cipher: Option<&'static str>,
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
}
