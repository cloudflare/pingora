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

//! The TLS layer implementations

pub mod digest;
pub use digest::*;

#[cfg(feature = "openssl_derived")]
mod boringssl_openssl;

#[cfg(feature = "openssl_derived")]
pub use boringssl_openssl::*;

#[cfg(feature = "rustls")]
mod rustls;

#[cfg(feature = "rustls")]
pub use rustls::*;

#[cfg(feature = "s2n")]
mod s2n;

#[cfg(feature = "s2n")]
pub use s2n::*;

#[cfg(not(feature = "any_tls"))]
pub mod noop_tls;

#[cfg(not(feature = "any_tls"))]
pub use noop_tls::*;

/// Containing type for a user callback to generate extensions for the `SslDigest` upon handshake
/// completion.
pub type HandshakeCompleteHook = std::sync::Arc<
    dyn Fn(&TlsRef) -> Option<std::sync::Arc<dyn std::any::Any + Send + Sync>> + Send + Sync,
>;

/// The protocol for Application-Layer Protocol Negotiation
#[derive(Hash, Clone, Debug, PartialEq, PartialOrd)]
pub enum ALPN {
    /// Prefer HTTP/1.1 only
    H1,
    /// Prefer HTTP/2 only
    H2,
    /// Prefer HTTP/2 over HTTP/1.1
    H2H1,
    /// Custom Protocol is stored in wire format (length-prefixed)
    /// Wire format is precomputed at creation to avoid dangling references
    Custom(CustomALPN),
}

/// Represents a Custom ALPN Protocol with a precomputed wire format and header offset.
#[derive(Hash, Clone, Debug, PartialEq, PartialOrd)]
pub struct CustomALPN {
    wire: Vec<u8>,
    header: usize,
}

impl CustomALPN {
    /// Create a new CustomALPN from a protocol byte vector
    pub fn new(proto: Vec<u8>) -> Self {
        // Validate before setting
        assert!(!proto.is_empty(), "Custom ALPN protocol must not be empty");
        // RFC-7301
        assert!(
            proto.len() <= 255,
            "ALPN protocol name must be 255 bytes or fewer"
        );

        match proto.as_slice() {
            b"http/1.1" | b"h2" => {
                panic!("Custom ALPN cannot be a reserved protocol (http/1.1 or h2)")
            }
            _ => {}
        }
        let mut wire = Vec::with_capacity(1 + proto.len());
        wire.push(proto.len() as u8);
        wire.extend_from_slice(&proto);

        Self {
            wire,
            header: 1, // Header is always at index 1 since we prefix one length byte
        }
    }

    /// Get the custom protocol name as a slice
    pub fn protocol(&self) -> &[u8] {
        &self.wire[self.header..]
    }

    /// Get the wire format used for ALPN negotiation
    pub fn as_wire(&self) -> &[u8] {
        &self.wire
    }
}

impl std::fmt::Display for ALPN {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ALPN::H1 => write!(f, "H1"),
            ALPN::H2 => write!(f, "H2"),
            ALPN::H2H1 => write!(f, "H2H1"),
            ALPN::Custom(custom) => {
                // extract protocol name, print as UTF-8 if possible, else judt itd raw bytes
                match std::str::from_utf8(custom.protocol()) {
                    Ok(s) => write!(f, "Custom({})", s),
                    Err(_) => write!(f, "Custom({:?})", custom.protocol()),
                }
            }
        }
    }
}

impl ALPN {
    /// Create a new ALPN according to the `max` and `min` version constraints
    pub fn new(max: u8, min: u8) -> Self {
        if max == 1 {
            ALPN::H1
        } else if min == 2 {
            ALPN::H2
        } else {
            ALPN::H2H1
        }
    }

    /// Return the max http version this [`ALPN`] allows
    pub fn get_max_http_version(&self) -> u8 {
        match self {
            ALPN::H1 => 1,
            ALPN::H2 | ALPN::H2H1 => 2,
            ALPN::Custom(_) => 0,
        }
    }

    /// Return the min http version this [`ALPN`] allows
    pub fn get_min_http_version(&self) -> u8 {
        match self {
            ALPN::H1 | ALPN::H2H1 => 1,
            ALPN::H2 => 2,
            ALPN::Custom(_) => 0,
        }
    }

    #[cfg(feature = "openssl_derived")]
    pub(crate) fn to_wire_preference(&self) -> &[u8] {
        // https://www.openssl.org/docs/manmaster/man3/SSL_CTX_set_alpn_select_cb.html
        // "vector of nonempty, 8-bit length-prefixed, byte strings"
        match self {
            Self::H1 => b"\x08http/1.1",
            Self::H2 => b"\x02h2",
            Self::H2H1 => b"\x02h2\x08http/1.1",
            Self::Custom(custom) => custom.as_wire(),
        }
    }

    #[cfg(feature = "any_tls")]
    pub(crate) fn from_wire_selected(raw: &[u8]) -> Option<Self> {
        match raw {
            b"http/1.1" => Some(Self::H1),
            b"h2" => Some(Self::H2),
            _ => Some(Self::Custom(CustomALPN::new(raw.to_vec()))),
        }
    }

    #[cfg(feature = "rustls")]
    pub(crate) fn to_wire_protocols(&self) -> Vec<Vec<u8>> {
        match self {
            ALPN::H1 => vec![b"http/1.1".to_vec()],
            ALPN::H2 => vec![b"h2".to_vec()],
            ALPN::H2H1 => vec![b"h2".to_vec(), b"http/1.1".to_vec()],
            ALPN::Custom(custom) => vec![custom.protocol().to_vec()],
        }
    }

    #[cfg(feature = "s2n")]
    pub(crate) fn to_wire_protocols(&self) -> Vec<Vec<u8>> {
        match self {
            ALPN::H1 => vec![b"http/1.1".to_vec()],
            ALPN::H2 => vec![b"h2".to_vec()],
            ALPN::H2H1 => vec![b"h2".to_vec(), b"http/1.1".to_vec()],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_valid_alpn_construction_and_versions() {
        // Standard Protocols
        assert_eq!(ALPN::H1.get_min_http_version(), 1);
        assert_eq!(ALPN::H1.get_max_http_version(), 1);

        assert_eq!(ALPN::H2.get_min_http_version(), 2);
        assert_eq!(ALPN::H2.get_max_http_version(), 2);

        assert_eq!(ALPN::H2H1.get_min_http_version(), 1);
        assert_eq!(ALPN::H2H1.get_max_http_version(), 2);

        // Custom Protocol
        let custom_protocol = ALPN::Custom(CustomALPN::new("custom/1.0".into()));
        assert_eq!(custom_protocol.get_min_http_version(), 0);
        assert_eq!(custom_protocol.get_max_http_version(), 0);
    }
    #[test]
    #[should_panic(expected = "Custom ALPN protocol must not be empty")]
    fn test_empty_custom_alpn() {
        let _ = ALPN::Custom(CustomALPN::new("".into()));
    }
    #[test]
    #[should_panic(expected = "ALPN protocol name must be 255 bytes or fewer")]
    fn test_large_custom_alpn() {
        let large_alpn = vec![b'a'; 256];
        let _ = ALPN::Custom(CustomALPN::new(large_alpn));
    }
    #[test]
    #[should_panic(expected = "Custom ALPN cannot be a reserved protocol (http/1.1 or h2)")]
    fn test_custom_h1_alpn() {
        let _ = ALPN::Custom(CustomALPN::new("http/1.1".into()));
    }
    #[test]
    #[should_panic(expected = "Custom ALPN cannot be a reserved protocol (http/1.1 or h2)")]
    fn test_custom_h2_alpn() {
        let _ = ALPN::Custom(CustomALPN::new("h2".into()));
    }
}
