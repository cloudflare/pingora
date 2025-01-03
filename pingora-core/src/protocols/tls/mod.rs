// Copyright 2025 Cloudflare, Inc.
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

#[cfg(not(feature = "any_tls"))]
pub mod noop_tls;

#[cfg(not(feature = "any_tls"))]
pub use noop_tls::*;

/// The protocol for Application-Layer Protocol Negotiation
#[derive(Hash, Clone, Debug)]
pub enum ALPN {
    /// Prefer HTTP/1.1 only
    H1,
    /// Prefer HTTP/2 only
    H2,
    /// Prefer HTTP/2 over HTTP/1.1
    H2H1,
}

impl std::fmt::Display for ALPN {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ALPN::H1 => write!(f, "H1"),
            ALPN::H2 => write!(f, "H2"),
            ALPN::H2H1 => write!(f, "H2H1"),
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
            _ => 2,
        }
    }

    /// Return the min http version this [`ALPN`] allows
    pub fn get_min_http_version(&self) -> u8 {
        match self {
            ALPN::H2 => 2,
            _ => 1,
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
        }
    }

    #[cfg(feature = "any_tls")]
    pub(crate) fn from_wire_selected(raw: &[u8]) -> Option<Self> {
        match raw {
            b"http/1.1" => Some(Self::H1),
            b"h2" => Some(Self::H2),
            _ => None,
        }
    }

    #[cfg(feature = "rustls")]
    pub(crate) fn to_wire_protocols(&self) -> Vec<Vec<u8>> {
        match self {
            ALPN::H1 => vec![b"http/1.1".to_vec()],
            ALPN::H2 => vec![b"h2".to_vec()],
            ALPN::H2H1 => vec![b"h2".to_vec(), b"http/1.1".to_vec()],
        }
    }
}
