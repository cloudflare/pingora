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

use crate::connectors::{ConnectorOptions, TransportConnector};
use crate::protocols::http::v1::client::HttpSession;
use crate::upstreams::peer::Peer;

use pingora_error::Result;
use std::time::Duration;

pub struct Connector {
    transport: TransportConnector,
}

impl Connector {
    pub fn new(options: Option<ConnectorOptions>) -> Self {
        Connector {
            transport: TransportConnector::new(options),
        }
    }

    pub async fn get_http_session<P: Peer + Send + Sync + 'static>(
        &self,
        peer: &P,
    ) -> Result<(HttpSession, bool)> {
        let (stream, reused) = self.transport.get_stream(peer).await?;
        let http = HttpSession::new(stream);
        Ok((http, reused))
    }

    pub async fn reused_http_session<P: Peer + Send + Sync + 'static>(
        &self,
        peer: &P,
    ) -> Option<HttpSession> {
        self.transport
            .reused_stream(peer)
            .await
            .map(HttpSession::new)
    }

    pub async fn release_http_session<P: Peer + Send + Sync + 'static>(
        &self,
        mut session: HttpSession,
        peer: &P,
        idle_timeout: Option<Duration>,
    ) {
        session.respect_keepalive();
        if let Some(stream) = session.reuse().await {
            self.transport
                .release_stream(stream, peer.reuse_hash(), idle_timeout);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocols::l4::socket::SocketAddr;
    use crate::upstreams::peer::HttpPeer;
    use pingora_http::RequestHeader;

    async fn get_http(http: &mut HttpSession, expected_status: u16) {
        let mut req = Box::new(RequestHeader::build("GET", b"/", None).unwrap());
        req.append_header("Host", "one.one.one.one").unwrap();
        http.write_request_header(req).await.unwrap();
        http.read_response().await.unwrap();
        http.respect_keepalive();

        assert_eq!(http.get_status().unwrap(), expected_status);
        while http.read_body_bytes().await.unwrap().is_some() {}
    }

    #[tokio::test]
    async fn test_connect() {
        let connector = Connector::new(None);
        let peer = HttpPeer::new(("1.1.1.1", 80), false, "".into());
        // make a new connection to 1.1.1.1
        let (http, reused) = connector.get_http_session(&peer).await.unwrap();
        let server_addr = http.server_addr().unwrap();
        assert_eq!(*server_addr, "1.1.1.1:80".parse::<SocketAddr>().unwrap());
        assert!(!reused);

        // this http is not even used, so not be able to reuse
        connector.release_http_session(http, &peer, None).await;
        let (mut http, reused) = connector.get_http_session(&peer).await.unwrap();
        assert!(!reused);

        get_http(&mut http, 301).await;
        connector.release_http_session(http, &peer, None).await;
        let (_, reused) = connector.get_http_session(&peer).await.unwrap();
        assert!(reused);
    }

    #[tokio::test]
    async fn test_connect_tls() {
        let connector = Connector::new(None);
        let peer = HttpPeer::new(("1.1.1.1", 443), true, "one.one.one.one".into());
        // make a new connection to https://1.1.1.1
        let (http, reused) = connector.get_http_session(&peer).await.unwrap();
        let server_addr = http.server_addr().unwrap();
        assert_eq!(*server_addr, "1.1.1.1:443".parse::<SocketAddr>().unwrap());
        assert!(!reused);

        // this http is not even used, so not be able to reuse
        connector.release_http_session(http, &peer, None).await;
        let (mut http, reused) = connector.get_http_session(&peer).await.unwrap();
        assert!(!reused);

        get_http(&mut http, 200).await;
        connector.release_http_session(http, &peer, None).await;
        let (_, reused) = connector.get_http_session(&peer).await.unwrap();
        assert!(reused);
    }
}
