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

//! Connecting to HTTP servers

use crate::connectors::ConnectorOptions;
use crate::protocols::http::client::HttpSession;
use crate::upstreams::peer::Peer;
use pingora_error::Result;
use std::time::Duration;

pub mod v1;
pub mod v2;

pub struct Connector {
    h1: v1::Connector,
    h2: v2::Connector,
}

impl Connector {
    pub fn new(options: Option<ConnectorOptions>) -> Self {
        Connector {
            h1: v1::Connector::new(options.clone()),
            h2: v2::Connector::new(options),
        }
    }

    /// Get an [HttpSession] to the given server.
    ///
    /// The second return value indicates whether the session is connected via a reused stream.
    pub async fn get_http_session<P: Peer + Send + Sync + 'static>(
        &self,
        peer: &P,
    ) -> Result<(HttpSession, bool)> {
        // NOTE: maybe TODO: we do not yet enforce that only TLS traffic can use h2, which is the
        // de facto requirement for h2, because non TLS traffic lack the negotiation mechanism.

        // We assume no peer option == no ALPN == h1 only
        let h1_only = peer
            .get_peer_options()
            .map_or(true, |o| o.alpn.get_max_http_version() == 1);
        if h1_only {
            let (h1, reused) = self.h1.get_http_session(peer).await?;
            Ok((HttpSession::H1(h1), reused))
        } else {
            // the peer allows h2, we first check the h2 reuse pool
            let reused_h2 = self.h2.reused_http_session(peer).await?;
            if let Some(h2) = reused_h2 {
                return Ok((HttpSession::H2(h2), true));
            }
            let h2_only = peer
                .get_peer_options()
                .map_or(false, |o| o.alpn.get_min_http_version() == 2)
                && !self.h2.h1_is_preferred(peer);
            if !h2_only {
                // We next check the reuse pool for h1 before creating a new h2 connection.
                // This is because the server may not support h2 at all, connections to
                // the server could all be h1.
                if let Some(h1) = self.h1.reused_http_session(peer).await {
                    return Ok((HttpSession::H1(h1), true));
                }
            }
            let session = self.h2.new_http_session(peer).await?;
            Ok((session, false))
        }
    }

    pub async fn release_http_session<P: Peer + Send + Sync + 'static>(
        &self,
        session: HttpSession,
        peer: &P,
        idle_timeout: Option<Duration>,
    ) {
        match session {
            HttpSession::H1(h1) => self.h1.release_http_session(h1, peer, idle_timeout).await,
            HttpSession::H2(h2) => self.h2.release_http_session(h2, peer, idle_timeout),
        }
    }

    /// Tell the connector to always send h1 for ALPN for the given peer in the future.
    pub fn prefer_h1(&self, peer: &impl Peer) {
        self.h2.prefer_h1(peer);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocols::http::v1::client::HttpSession as Http1Session;
    use crate::upstreams::peer::HttpPeer;
    use pingora_http::RequestHeader;

    async fn get_http(http: &mut Http1Session, expected_status: u16) {
        let mut req = Box::new(RequestHeader::build("GET", b"/", None).unwrap());
        req.append_header("Host", "one.one.one.one").unwrap();
        http.write_request_header(req).await.unwrap();
        http.read_response().await.unwrap();
        http.respect_keepalive();

        assert_eq!(http.get_status().unwrap(), expected_status);
        while http.read_body_bytes().await.unwrap().is_some() {}
    }

    #[tokio::test]
    async fn test_connect_h2() {
        let connector = Connector::new(None);
        let mut peer = HttpPeer::new(("1.1.1.1", 443), true, "one.one.one.one".into());
        peer.options.set_http_version(2, 2);
        let (h2, reused) = connector.get_http_session(&peer).await.unwrap();
        assert!(!reused);
        match &h2 {
            HttpSession::H1(_) => panic!("expect h2"),
            HttpSession::H2(h2_stream) => assert!(!h2_stream.ping_timedout()),
        }

        connector.release_http_session(h2, &peer, None).await;

        let (h2, reused) = connector.get_http_session(&peer).await.unwrap();
        // reused this time
        assert!(reused);
        match &h2 {
            HttpSession::H1(_) => panic!("expect h2"),
            HttpSession::H2(h2_stream) => assert!(!h2_stream.ping_timedout()),
        }
    }

    #[tokio::test]
    async fn test_connect_h1() {
        let connector = Connector::new(None);
        let mut peer = HttpPeer::new(("1.1.1.1", 443), true, "one.one.one.one".into());
        peer.options.set_http_version(1, 1);
        let (mut h1, reused) = connector.get_http_session(&peer).await.unwrap();
        assert!(!reused);
        match &mut h1 {
            HttpSession::H1(http) => {
                get_http(http, 200).await;
            }
            HttpSession::H2(_) => panic!("expect h1"),
        }
        connector.release_http_session(h1, &peer, None).await;

        let (mut h1, reused) = connector.get_http_session(&peer).await.unwrap();
        // reused this time
        assert!(reused);
        match &mut h1 {
            HttpSession::H1(_) => {}
            HttpSession::H2(_) => panic!("expect h1"),
        }
    }

    #[tokio::test]
    async fn test_connect_h2_fallback_h1_reuse() {
        // this test verify that if the server doesn't support h2, the Connector will reuse the
        // h1 session instead.

        let connector = Connector::new(None);
        let mut peer = HttpPeer::new(("1.1.1.1", 443), true, "one.one.one.one".into());
        // As it is hard to find a server that support only h1, we use the following hack to trick
        // the connector to think the server supports only h1. We force ALPN to use h1 and then
        // return the connection to the Connector. And then we use a Peer that allows h2
        peer.options.set_http_version(1, 1);
        let (mut h1, reused) = connector.get_http_session(&peer).await.unwrap();
        assert!(!reused);
        match &mut h1 {
            HttpSession::H1(http) => {
                get_http(http, 200).await;
            }
            HttpSession::H2(_) => panic!("expect h1"),
        }
        connector.release_http_session(h1, &peer, None).await;

        let mut peer = HttpPeer::new(("1.1.1.1", 443), true, "one.one.one.one".into());
        peer.options.set_http_version(2, 1);

        let (mut h1, reused) = connector.get_http_session(&peer).await.unwrap();
        // reused this time
        assert!(reused);
        match &mut h1 {
            HttpSession::H1(_) => {}
            HttpSession::H2(_) => panic!("expect h1"),
        }
    }

    #[tokio::test]
    async fn test_connect_prefer_h1() {
        let connector = Connector::new(None);
        let mut peer = HttpPeer::new(("1.1.1.1", 443), true, "one.one.one.one".into());
        peer.options.set_http_version(2, 1);
        connector.prefer_h1(&peer);

        let (mut h1, reused) = connector.get_http_session(&peer).await.unwrap();
        assert!(!reused);
        match &mut h1 {
            HttpSession::H1(http) => {
                get_http(http, 200).await;
            }
            HttpSession::H2(_) => panic!("expect h1"),
        }
        connector.release_http_session(h1, &peer, None).await;

        peer.options.set_http_version(2, 2);
        let (mut h1, reused) = connector.get_http_session(&peer).await.unwrap();
        // reused this time
        assert!(reused);
        match &mut h1 {
            HttpSession::H1(_) => {}
            HttpSession::H2(_) => panic!("expect h1"),
        }
    }
}
