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

//! Connection filtering trait for early connection filtering

use async_trait::async_trait;
use std::fmt::Debug;
use std::net::SocketAddr;

/// The trait for filtering connections at the TCP level before TLS handshake
#[async_trait]
pub trait ConnectionFilter: Debug + Send + Sync {
    /// Called when a new TCP connection is accepted, before TLS handshake
    /// Return true to accept the connection, false to drop it
    async fn should_accept(&self, _addr: &SocketAddr) -> bool {
        true
    }
}

/// Default implementation that accepts all connections
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
        async fn should_accept(&self, addr: &SocketAddr) -> bool {
            !self.blocked_ips.contains(&addr.ip())
        }
    }

    #[tokio::test]
    async fn test_accept_all_filter() {
        let filter = AcceptAllFilter;
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8080);
        assert!(filter.should_accept(&addr).await);
    }

    #[tokio::test]
    async fn test_blocklist_filter() {
        let filter = BlockListFilter {
            blocked_ips: vec![IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1))],
        };

        let blocked_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1)), 8080);
        let allowed_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 2)), 8080);

        assert!(!filter.should_accept(&blocked_addr).await);
        assert!(filter.should_accept(&allowed_addr).await);
    }
}
