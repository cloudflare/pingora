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

//! Connecting to HTTP servers

use crate::connectors::http::custom::Connection;
use crate::connectors::ConnectorOptions;
use crate::listeners::ALPN;
use crate::protocols::http::client::HttpSession;
use crate::protocols::http::v1::client::HttpSession as Http1Session;
use crate::upstreams::peer::Peer;
use pingora_error::Result;
use std::time::Duration;

pub mod custom;
pub mod v1;
pub mod v2;

pub struct Connector<C = ()>
where
    C: custom::Connector,
{
    h1: v1::Connector,
    h2: v2::Connector,
    custom: C,
}

impl Connector<()> {
    pub fn new(options: Option<ConnectorOptions>) -> Self {
        Connector {
            h1: v1::Connector::new(options.clone()),
            h2: v2::Connector::new(options.clone()),
            custom: Default::default(),
        }
    }
}

impl<C> Connector<C>
where
    C: custom::Connector,
{
    pub fn new_custom(options: Option<ConnectorOptions>, custom: C) -> Self {
        Connector {
            h1: v1::Connector::new(options.clone()),
            h2: v2::Connector::new(options.clone()),
            custom,
        }
    }

    /// Get an [HttpSession] to the given server.
    ///
    /// The second return value indicates whether the session is connected via a reused stream.
    pub async fn get_http_session<P: Peer + Send + Sync + 'static>(
        &self,
        peer: &P,
    ) -> Result<(HttpSession<C::Session>, bool)> {
        let peer_opts = peer.get_peer_options();

        // Switch to custom protocol as early as possible
        if peer_opts.is_some_and(|o| matches!(o.alpn, ALPN::Custom(_))) {
            // We create the Connector before TLS, so we need to make sure that the server also supports the same custom protocol.
            // We will first check for sessions that we can reuse, if not we will create a new one based on the negotiated protocol

            // Step 1: Look for reused Custom Session
            if let Some(session) = self.custom.reused_http_session(peer).await {
                return Ok((HttpSession::Custom(session), true));
            }
            // Step 2: Check reuse pool for reused H1 session
            if let Some(h1) = self.h1.reused_http_session(peer).await {
                return Ok((HttpSession::H1(h1), true));
            }
            // Step 3: Try and create a new Custom session
            let (connection, reused) = self.custom.get_http_session(peer).await?;
            // We create the Connector before TLS, so we need to make sure that the server also supports the same custom protocol
            match connection {
                Connection::Session(s) => {
                    return Ok((HttpSession::Custom(s), reused));
                }
                // Negotiated ALPN is not custom, create a new H1 session
                Connection::Stream(s) => {
                    return Ok((HttpSession::H1(Http1Session::new(s)), false));
                }
            }
        }

        // NOTE: maybe TODO: we do not yet enforce that only TLS traffic can use h2, which is the
        // de facto requirement for h2, because non TLS traffic lack the negotiation mechanism.

        // We assume no peer option == no ALPN == h1 only
        let h1_only = peer
            .get_peer_options()
            .is_none_or(|o| o.alpn.get_max_http_version() == 1);
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
                .is_some_and(|o| o.alpn.get_min_http_version() == 2)
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
        session: HttpSession<C::Session>,
        peer: &P,
        idle_timeout: Option<Duration>,
    ) {
        match session {
            HttpSession::H1(h1) => self.h1.release_http_session(h1, peer, idle_timeout).await,
            HttpSession::H2(h2) => self.h2.release_http_session(h2, peer, idle_timeout),
            HttpSession::Custom(c) => {
                self.custom
                    .release_http_session(c, peer, idle_timeout)
                    .await;
            }
        }
    }

    /// Tell the connector to always send h1 for ALPN for the given peer in the future.
    pub fn prefer_h1(&self, peer: &impl Peer) {
        self.h2.prefer_h1(peer);
    }
}

#[cfg(test)]
#[cfg(feature = "any_tls")]
mod tests {
    use super::*;
    use crate::connectors::TransportConnector;
    use crate::listeners::tls::TlsSettings;
    use crate::listeners::{Listeners, TransportStack, ALPN};
    use crate::protocols::http::v1::client::HttpSession as Http1Session;
    use crate::protocols::tls::CustomALPN;
    use crate::upstreams::peer::HttpPeer;
    use crate::upstreams::peer::PeerOptions;
    use async_trait::async_trait;
    use pingora_http::RequestHeader;
    use std::sync::Arc;
    use std::sync::Mutex;
    use tokio::io::AsyncWriteExt;
    use tokio::net::TcpListener;
    use tokio::task::JoinHandle;
    use tokio::time::sleep;

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
            HttpSession::Custom(_) => panic!("expect h2"),
        }

        connector.release_http_session(h2, &peer, None).await;

        let (h2, reused) = connector.get_http_session(&peer).await.unwrap();
        // reused this time
        assert!(reused);
        match &h2 {
            HttpSession::H1(_) => panic!("expect h2"),
            HttpSession::H2(h2_stream) => assert!(!h2_stream.ping_timedout()),
            HttpSession::Custom(_) => panic!("expect h2"),
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
            HttpSession::Custom(_) => panic!("expect h1"),
        }
        connector.release_http_session(h1, &peer, None).await;

        let (mut h1, reused) = connector.get_http_session(&peer).await.unwrap();
        // reused this time
        assert!(reused);
        match &mut h1 {
            HttpSession::H1(_) => {}
            HttpSession::H2(_) => panic!("expect h1"),
            HttpSession::Custom(_) => panic!("expect h1"),
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
            HttpSession::Custom(_) => panic!("expect h1"),
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
            HttpSession::Custom(_) => panic!("expect h1"),
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
            HttpSession::Custom(_) => panic!("expect h1"),
        }
        connector.release_http_session(h1, &peer, None).await;

        peer.options.set_http_version(2, 2);
        let (mut h1, reused) = connector.get_http_session(&peer).await.unwrap();
        // reused this time
        assert!(reused);
        match &mut h1 {
            HttpSession::H1(_) => {}
            HttpSession::H2(_) => panic!("expect h1"),
            HttpSession::Custom(_) => panic!("expect h1"),
        }
    }
    // Track the flow of calls when using a custom protocol. For this we need to create a Mock Connector
    struct MockConnector {
        transport: TransportConnector,
        reusable: Arc<Mutex<bool>>, // Mock for tracking reusable sessions
    }

    #[async_trait]
    impl custom::Connector for MockConnector {
        type Session = ();

        async fn get_http_session<P: Peer + Send + Sync + 'static>(
            &self,
            peer: &P,
        ) -> Result<(Connection<Self::Session>, bool)> {
            let (stream, _) = self.transport.get_stream(peer).await?;

            match stream.selected_alpn_proto() {
                Some(ALPN::Custom(_)) => Ok((custom::Connection::Session(()), false)),
                _ => Ok(((custom::Connection::Stream(stream)), false)),
            }
        }

        async fn reused_http_session<P: Peer + Send + Sync + 'static>(
            &self,
            _peer: &P,
        ) -> Option<Self::Session> {
            let mut flag = self.reusable.lock().unwrap();
            if *flag {
                *flag = false;
                Some(())
            } else {
                None
            }
        }

        async fn release_http_session<P: Peer + Send + Sync + 'static>(
            &self,
            _session: Self::Session,
            _peer: &P,
            _idle_timeout: Option<Duration>,
        ) {
            let mut flag = self.reusable.lock().unwrap();
            *flag = true;
        }
    }

    // Finds an available TCP port on localhost for test server setup.
    async fn get_available_port() -> u16 {
        TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }
    // Creates a test connector for integration/unit tests.
    // For rustls, only ConnectorOptions are used here; the actual dangerous verifier is patched in the TLS connector.
    fn create_test_connector() -> Connector<MockConnector> {
        #[cfg(feature = "rustls")]
        let custom_transport = {
            let options = ConnectorOptions::new(1);
            TransportConnector::new(Some(options))
        };
        #[cfg(not(feature = "rustls"))]
        let custom_transport = TransportConnector::new(None);
        Connector {
            h1: v1::Connector::new(None),
            h2: v2::Connector::new(None),
            custom: MockConnector {
                transport: custom_transport,
                reusable: Arc::new(Mutex::new(false)),
            },
        }
    }

    // Creates a test peer that uses a custom ALPN protocol and disables cert/hostname verification for tests.
    fn create_peer_with_custom_proto(port: u16, proto: &[u8]) -> HttpPeer {
        let mut peer = HttpPeer::new(("127.0.0.1", port), true, "localhost".into());
        let mut options = PeerOptions::new();
        options.alpn = ALPN::Custom(CustomALPN::new(proto.to_vec()));
        // Disable cert verification for this test (self-signed or invalid certs are OK)
        options.verify_cert = false;
        options.verify_hostname = false;
        peer.options = options;
        peer
    }
    async fn build_custom_tls_listener(port: u16, custom_alpn: CustomALPN) -> TransportStack {
        let cert_path = format!("{}/tests/keys/server.crt", env!("CARGO_MANIFEST_DIR"));
        let key_path = format!("{}/tests/keys/key.pem", env!("CARGO_MANIFEST_DIR"));
        let addr = format!("127.0.0.1:{}", port);
        let mut listeners = Listeners::new();
        let mut tls_settings = TlsSettings::intermediate(&cert_path, &key_path).unwrap();

        tls_settings.set_alpn(ALPN::Custom(custom_alpn));
        listeners.add_tls_with_settings(&addr, None, tls_settings);
        listeners
            .build(
                #[cfg(unix)]
                None,
            )
            .await
            .unwrap()
            .pop()
            .unwrap()
    }

    // Spawn a simple TLS Server
    fn spawn_test_tls_server(listener: TransportStack) -> JoinHandle<()> {
        tokio::spawn(async move {
            loop {
                let stream = match listener.accept().await {
                    Ok(stream) => stream,
                    Err(_) => break, // Exit if listener is closed
                };
                let mut stream = stream.handshake().await.unwrap();

                let _ = stream.write_all(b"CUSTOM").await; // Ignore write errors
            }
        })
    }

    // Both server and client are using the same custom protocol
    #[tokio::test]
    async fn test_custom_client_custom_upstream() {
        let port = get_available_port().await;
        let custom_protocol = b"custom".to_vec();

        let listener =
            build_custom_tls_listener(port, CustomALPN::new(custom_protocol.clone())).await;
        let server_handle = spawn_test_tls_server(listener);
        // Wait for server to start up
        sleep(Duration::from_millis(100)).await;

        let connector = create_test_connector();
        let peer = create_peer_with_custom_proto(port, &custom_protocol);

        // Check that the agreed ALPN is custom and matches the expected value
        if let Ok((stream, reused)) = connector.custom.transport.get_stream(&peer).await {
            assert!(!reused);
            match stream.selected_alpn_proto() {
                Some(ALPN::Custom(protocol)) => {
                    assert_eq!(
                        protocol.protocol(),
                        custom_protocol.as_slice(),
                        "Negotiated custom ALPN does not match expected value"
                    );
                }
                other => panic!("Expected custom ALPN, got {:?}", other),
            }
        } else {
            panic!("Should be able to create a stream");
        }

        let (custom, reused) = connector.get_http_session(&peer).await.unwrap();
        assert!(!reused);
        match custom {
            HttpSession::H1(_) => panic!("expect custom"),
            HttpSession::H2(_) => panic!("expect custom"),
            HttpSession::Custom(_) => {}
        }
        connector.release_http_session(custom, &peer, None).await;

        // Assert it returns a reused custom session this time
        let (custom, reused) = connector.get_http_session(&peer).await.unwrap();
        assert!(reused);
        match custom {
            HttpSession::H1(_) => panic!("expect custom"),
            HttpSession::H2(_) => panic!("expect custom"),
            HttpSession::Custom(_) => {}
        }

        // Kill the server task
        server_handle.abort();
        sleep(Duration::from_millis(100)).await;
    }

    // Both client and server are using custom protocols, but different ones - we should create H1 sessions as fallback.
    // For RusTLS if there is no agreed protocol, the handshake directly fails, so this won't work
    // TODO: If no ALPN is matched, rustls should return None instead of failing the handshake.
    #[cfg(not(feature = "rustls"))]
    #[tokio::test]
    async fn test_incompatible_custom_client_custom_upstream() {
        let port = get_available_port().await;
        let custom_protocol = b"custom".to_vec();

        let listener =
            build_custom_tls_listener(port, CustomALPN::new(b"different_custom".to_vec())).await;
        let server_handle = spawn_test_tls_server(listener);
        // Wait for server to start up
        sleep(Duration::from_millis(100)).await;

        let connector = create_test_connector();
        let peer = create_peer_with_custom_proto(port, &custom_protocol);

        // Verify that there is no agreed ALPN
        if let Ok((stream, reused)) = connector.custom.transport.get_stream(&peer).await {
            assert!(!reused);
            assert!(stream.selected_alpn_proto().is_none());
        } else {
            panic!("Should be able to create a stream");
        }

        let (h1, reused) = connector.get_http_session(&peer).await.unwrap();
        assert!(!reused);
        match h1 {
            HttpSession::H1(_) => {}
            HttpSession::H2(_) => panic!("expect h1"),
            HttpSession::Custom(_) => panic!("expect h1"),
        }
        // Not testing session reuse logic here as we haven't implemented it. Next test will test this.

        // Kill the server task
        server_handle.abort();
        sleep(Duration::from_millis(100)).await;
    }

    // Client thinks server is custom but server is not Custom. Should fallback to H1
    #[tokio::test]
    async fn test_custom_client_non_custom_upstream() {
        let custom_proto = b"custom".to_vec();
        let connector = create_test_connector();
        // Upstream supports H1 and H2
        let mut peer = HttpPeer::new(("1.1.1.1", 443), true, "one.one.one.one".into());
        // Client sets upstream ALPN as custom protocol
        peer.options.alpn = ALPN::Custom(CustomALPN::new(custom_proto));

        // Verify that there is no agreed ALPN
        if let Ok((stream, reused)) = connector.custom.transport.get_stream(&peer).await {
            assert!(!reused);
            assert!(stream.selected_alpn_proto().is_none());
        } else {
            panic!("Should be able to create a stream");
        }

        let (mut h1, reused) = connector.get_http_session(&peer).await.unwrap();
        // Assert it returns a new H1 session
        assert!(!reused);
        match &mut h1 {
            HttpSession::H1(http) => {
                get_http(http, 200).await;
            }
            HttpSession::H2(_) => panic!("expect h1"),
            HttpSession::Custom(_) => panic!("expect h1"),
        }
        connector.release_http_session(h1, &peer, None).await;

        // Assert it returns a reused h1 session this time
        let (mut h1, reused) = connector.get_http_session(&peer).await.unwrap();
        assert!(reused);
        match &mut h1 {
            HttpSession::H1(_) => {}
            HttpSession::H2(_) => panic!("expect h1"),
            HttpSession::Custom(_) => panic!("expect h1"),
        }
    }
}

// Used for disabling certificate/hostname verification in rustls for tests and custom ALPN/self-signed scenarios.
#[cfg(all(test, feature = "rustls"))]
pub mod rustls_no_verify {
    use rustls::client::danger::{ServerCertVerified, ServerCertVerifier};
    use rustls::pki_types::{CertificateDer, ServerName};
    use rustls::Error as TLSError;
    use std::sync::Arc;
    #[derive(Debug)]
    pub struct NoCertificateVerification;

    impl ServerCertVerifier for NoCertificateVerification {
        fn verify_server_cert(
            &self,
            _end_entity: &CertificateDer,
            _intermediates: &[CertificateDer],
            _server_name: &ServerName,
            _scts: &[u8],
            _now: rustls::pki_types::UnixTime,
        ) -> Result<ServerCertVerified, TLSError> {
            Ok(ServerCertVerified::assertion())
        }

        fn verify_tls12_signature(
            &self,
            _message: &[u8],
            _cert: &CertificateDer,
            _dss: &rustls::DigitallySignedStruct,
        ) -> Result<rustls::client::danger::HandshakeSignatureValid, TLSError> {
            Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
        }

        fn verify_tls13_signature(
            &self,
            _message: &[u8],
            _cert: &CertificateDer,
            _dss: &rustls::DigitallySignedStruct,
        ) -> Result<rustls::client::danger::HandshakeSignatureValid, TLSError> {
            Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
        }

        fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
            vec![rustls::SignatureScheme::ECDSA_NISTP256_SHA256]
        }
    }

    pub fn apply_no_verify(config: &mut rustls::ClientConfig) {
        config
            .dangerous()
            .set_certificate_verifier(Arc::new(NoCertificateVerification));
    }
}
