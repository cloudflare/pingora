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

//! Connection filtering trait for early connection filtering
//!
//! This module provides the [`ConnectionFilter`] trait which allows filtering
//! incoming connections at the TCP level, before the TLS handshake occurs.
//!
//! # Feature Flag
//!
//! This functionality requires the `connection_filter` feature to be enabled:
//! ```toml
//! [dependencies]
//! pingora-core = { version = "0.5", features = ["connection_filter"] }
//! ```
//!
//! When the feature is disabled, a no-op implementation is provided for API compatibility.

use async_trait::async_trait;
use std::fmt::Debug;
use std::net::SocketAddr;

/// A trait for filtering incoming connections at the TCP level.
///
/// Implementations of this trait can inspect the peer address of incoming
/// connections and decide whether to accept or reject them before any
/// further processing (including TLS handshake) occurs.
///
/// # Example
///
/// ```rust,no_run
/// use async_trait::async_trait;
/// use pingora_core::listeners::ConnectionFilter;
/// use std::net::{IpAddr, Ipv4Addr, SocketAddr};
///
/// #[derive(Debug)]
/// struct BlocklistFilter {
///     blocked_ips: Vec<IpAddr>,
/// }
///
/// #[async_trait]
/// impl ConnectionFilter for BlocklistFilter {
///     async fn should_accept(&self, addr: &SocketAddr) -> bool {
///         !self.blocked_ips.contains(&addr.ip())
///     }
/// }
/// ```
///
/// # Performance Considerations
///
/// This filter is called for every incoming connection, so implementations
/// should be efficient. Consider caching or pre-computing data structures
/// for IP filtering rather than doing expensive operations per connection.
#[async_trait]
pub trait ConnectionFilter: Debug + Send + Sync {
    /// Determines whether an incoming connection should be accepted.
    ///
    /// This method is called after a TCP connection is accepted but before
    /// any further processing (including TLS handshake).
    ///
    /// # Arguments
    ///
    /// * `addr` - The socket address of the incoming connection
    ///
    /// # Returns
    ///
    /// * `true` - Accept the connection and continue processing
    /// * `false` - Drop the connection immediately
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// async fn should_accept(&self, addr: &SocketAddr) -> bool {
    ///     // Accept only connections from private IP ranges
    ///     match addr.ip() {
    ///         IpAddr::V4(ip) => ip.is_private(),
    ///         IpAddr::V6(_) => true,
    ///     }
    /// }
    ///
    async fn should_accept(&self, _addr: Option<&SocketAddr>) -> bool {
        true
    }
}

/// Default implementation that accepts all connections.
///
/// This filter accepts all incoming connections without any filtering.
/// It's used as the default when no custom filter is specified.
#[derive(Debug, Clone)]
pub struct AcceptAllFilter;

#[async_trait]
impl ConnectionFilter for AcceptAllFilter {
    // Uses default implementation
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};

    #[derive(Debug, Clone)]
    struct BlockListFilter {
        blocked_ips: Vec<IpAddr>,
    }

    #[async_trait]
    impl ConnectionFilter for BlockListFilter {
        async fn should_accept(&self, addr_opt: Option<&SocketAddr>) -> bool {
            addr_opt
                .map(|addr| !self.blocked_ips.contains(&addr.ip()))
                .unwrap_or(true)
        }
    }

    #[tokio::test]
    async fn test_accept_all_filter() {
        let filter = AcceptAllFilter;
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8080);
        assert!(filter.should_accept(Some(&addr)).await);
    }

    #[tokio::test]
    async fn test_blocklist_filter() {
        let filter = BlockListFilter {
            blocked_ips: vec![IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1))],
        };

        let blocked_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1)), 8080);
        let allowed_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 2)), 8080);

        assert!(!filter.should_accept(Some(&blocked_addr)).await);
        assert!(filter.should_accept(Some(&allowed_addr)).await);
    }
}
