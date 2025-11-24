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

//! The listening endpoints (TCP and TLS) and their configurations.
//!
//! This module provides the infrastructure for setting up network listeners
//! that accept incoming connections. It supports TCP, Unix domain sockets,
//! and TLS endpoints.
//!
//! # Connection Filtering
//!
//! With the `connection_filter` feature enabled, this module also provides
//! early connection filtering capabilities through the [`ConnectionFilter`] trait.
//! This allows dropping unwanted connections at the TCP level before any
//! expensive operations like TLS handshakes.
//!
//! ## Example with Connection Filtering
//!
//! ```rust,no_run
//! # #[cfg(feature = "connection_filter")]
//! # {
//! use pingora_core::listeners::{Listeners, ConnectionFilter};
//! use std::sync::Arc;
//!
//! // Create a custom filter
//! let filter = Arc::new(MyCustomFilter::new());
//!
//! // Apply to listeners
//! let mut listeners = Listeners::new();
//! listeners.set_connection_filter(filter);
//! listeners.add_tcp("0.0.0.0:8080");
//! # }
//! ```

mod l4;

#[cfg(feature = "connection_filter")]
pub mod connection_filter;

#[cfg(feature = "connection_filter")]
pub use connection_filter::{AcceptAllFilter, ConnectionFilter};

#[cfg(not(feature = "connection_filter"))]
#[derive(Debug, Clone)]
pub struct AcceptAllFilter;

#[cfg(not(feature = "connection_filter"))]
pub trait ConnectionFilter: std::fmt::Debug + Send + Sync {
    fn should_accept(&self, _addr: &std::net::SocketAddr) -> bool {
        true
    }
}

#[cfg(not(feature = "connection_filter"))]
impl ConnectionFilter for AcceptAllFilter {
    fn should_accept(&self, _addr: &std::net::SocketAddr) -> bool {
        true
    }
}
#[cfg(feature = "any_tls")]
pub mod tls;

#[cfg(not(feature = "any_tls"))]
pub use crate::tls::listeners as tls;

use crate::protocols::{
    l4::socket::SocketAddr,
    proxy_protocol,
    tls::TlsRef,
    Stream,
};
use log::{debug, warn};
use pingora_error::{OrErr, ErrorType::*};

/// Callback function type for ClientHello extraction
/// This allows external code (like moat) to generate fingerprints from ClientHello
pub type ClientHelloCallback = Option<fn(&crate::protocols::tls::client_hello::ClientHello, Option<SocketAddr>)>;

/// Global callback for ClientHello extraction
/// This is set by moat to generate fingerprints
static CLIENT_HELLO_CALLBACK: std::sync::OnceLock<std::sync::Mutex<ClientHelloCallback>> = std::sync::OnceLock::new();

/// Set the ClientHello callback function
/// This is called by moat to register fingerprint generation
///
/// # Example
/// ```
/// use pingora_core::listeners::set_client_hello_callback;
/// use pingora_core::protocols::tls::client_hello::ClientHello;
/// use pingora_core::protocols::l4::socket::SocketAddr;
///
/// set_client_hello_callback(Some(|hello: &ClientHello, peer_addr: Option<SocketAddr>| {
///     // Generate fingerprint from ClientHello
///     println!("SNI: {:?}, ALPN: {:?}", hello.sni, hello.alpn);
/// }));
/// ```
pub fn set_client_hello_callback(callback: ClientHelloCallback) {
    CLIENT_HELLO_CALLBACK.get_or_init(|| std::sync::Mutex::new(callback));
    if let Ok(mut cb) = CLIENT_HELLO_CALLBACK.get().unwrap().lock() {
        *cb = callback;
    }
}

/// Call the ClientHello callback if registered
fn call_client_hello_callback(hello: &crate::protocols::tls::client_hello::ClientHello, peer_addr: Option<SocketAddr>) {
    if let Some(cb_guard) = CLIENT_HELLO_CALLBACK.get() {
        if let Ok(cb) = cb_guard.lock() {
            if let Some(callback) = *cb {
                callback(hello, peer_addr);
            } else {
                log::debug!("ClientHello callback is None, skipping");
            }
        } else {
            log::debug!("Failed to lock ClientHello callback mutex");
        }
    } else {
        log::debug!("ClientHello callback not registered");
    }
}

#[cfg(unix)]
use crate::server::ListenFds;

use async_trait::async_trait;
use pingora_error::Result;
use std::{fs::Permissions, sync::Arc};

use l4::{ListenerEndpoint, Stream as L4Stream};
use tls::{Acceptor, TlsSettings};

pub use crate::protocols::tls::ALPN;
use crate::protocols::GetSocketDigest;
pub use l4::{ServerAddress, TcpSocketOptions};

/// The APIs to customize things like certificate during TLS server side handshake
#[async_trait]
pub trait TlsAccept {
    // TODO: return error?
    /// This function is called in the middle of a TLS handshake. Structs who
    /// implement this function should provide tls certificate and key to the
    /// [TlsRef] via `ssl_use_certificate` and `ssl_use_private_key`.
    /// Note. This is only supported for openssl and boringssl
    async fn certificate_callback(&self, _ssl: &mut TlsRef) -> () {
        // does nothing by default
    }
}

pub type TlsAcceptCallbacks = Box<dyn TlsAccept + Send + Sync>;

struct TransportStackBuilder {
    l4: ServerAddress,
    tls: Option<TlsSettings>,
    #[cfg(feature = "connection_filter")]
    connection_filter: Option<Arc<dyn ConnectionFilter>>,
}

impl TransportStackBuilder {
    pub async fn build(
        &mut self,
        #[cfg(unix)] upgrade_listeners: Option<ListenFds>,
    ) -> Result<TransportStack> {
        let mut builder = ListenerEndpoint::builder();

        builder.listen_addr(self.l4.clone());

        #[cfg(feature = "connection_filter")]
        if let Some(filter) = &self.connection_filter {
            builder.connection_filter(filter.clone());
        }

        #[cfg(unix)]
        let l4 = builder.listen(upgrade_listeners).await?;

        #[cfg(windows)]
        let l4 = builder.listen().await?;

        Ok(TransportStack {
            l4,
            tls: self.tls.take().map(|tls| Arc::new(tls.build())),
        })
    }
}

#[derive(Clone)]
pub(crate) struct TransportStack {
    l4: ListenerEndpoint,
    tls: Option<Arc<Acceptor>>,
}

impl TransportStack {
    pub fn as_str(&self) -> &str {
        self.l4.as_str()
    }

    pub async fn accept(&self) -> Result<UninitializedStream> {
        let stream = self.l4.accept().await?;
        Ok(UninitializedStream {
            l4: stream,
            tls: self.tls.clone(),
        })
    }

    pub fn cleanup(&mut self) {
        // placeholder
    }
}

pub(crate) struct UninitializedStream {
    l4: L4Stream,
    tls: Option<Arc<Acceptor>>,
}

impl UninitializedStream {
    pub async fn handshake(mut self) -> Result<Stream> {
        self.l4.set_buffer();

        // Consume PROXY headers first (if enabled)
        // This reads PROXY headers from the socket and rewinds any leftover data (ClientHello) to the rewind buffer
        self.maybe_consume_proxy_header().await?;

        if let Some(tls) = self.tls {
            #[cfg(unix)]
            {
                // Use ClientHelloWrapper to extract ClientHello before TLS handshake
                // After PROXY header consumption, ClientHello is in the rewind buffer or socket buffer
                // peek_client_hello will handle PROXY protocol detection if headers are still in socket buffer
                use crate::protocols::ClientHelloWrapper;

                // Wrap stream with ClientHelloWrapper
                let mut wrapper = ClientHelloWrapper::new(self.l4);

                // Extract ClientHello asynchronously (avoids blocking the thread)
                // peek_client_hello will detect and skip PROXY headers if they're still in the socket buffer
                // If PROXY headers were consumed, ClientHello will be in the rewind buffer and peek won't see it,
                // but that's okay - TLS handshake will read it from the rewind buffer normally
                let hello = match wrapper.extract_client_hello_async().await {
                    Ok(Some(hello)) => Some(hello),
                    Ok(None) => {
                        debug!("No ClientHello detected in stream");
                        None
                    }
                    Err(e) => {
                        // Check if this is a connection error that should abort the handshake
                        match e.kind() {
                            std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionAborted => {
                                debug!("Connection closed during ClientHello extraction: {:?}", e);
                                // Return error to abort the connection instead of proceeding to TLS handshake
                                return Err(e).or_err(AcceptError, "Connection closed during ClientHello extraction");
                            }
                            _ => {
                                debug!("Non-fatal error extracting ClientHello: {:?}", e);
                                None
                            }
                        }
                    }
                };

                if let Some(hello) = hello {
                    // Get peer address if available
                    let peer_addr = wrapper.get_socket_digest()
                        .and_then(|d| d.peer_addr().cloned());

                    debug!("Extracted ClientHello: SNI={:?}, ALPN={:?}, Peer={:?}", hello.sni, hello.alpn, peer_addr);

                    // Call the callback to generate fingerprint (registered by moat)
                    call_client_hello_callback(&hello, peer_addr);
                } else {
                    debug!("Could not extract ClientHello");
                }

                // Perform TLS handshake with wrapped stream
                let tls_stream = tls.tls_handshake(wrapper).await?;
                Ok(Box::new(tls_stream))
            }

            #[cfg(not(unix))]
            {
                // On non-Unix systems, just perform normal TLS handshake
                let tls_stream = tls.tls_handshake(self.l4).await?;
                Ok(Box::new(tls_stream))
            }
        } else {
            Ok(Box::new(self.l4))
        }
    }

    /// Get the peer address of the connection if available
    pub fn peer_addr(&self) -> Option<SocketAddr> {
        self.l4
            .get_socket_digest()
            .and_then(|d| d.peer_addr().cloned())
    }

    async fn maybe_consume_proxy_header(&mut self) -> Result<()> {
        if !proxy_protocol::proxy_protocol_enabled() {
            return Ok(());
        }

        let peer_addr = self.l4
            .get_socket_digest()
            .and_then(|d| d.transport_peer_addr().cloned());
        let peer_str = peer_addr
            .as_ref()
            .map(|a| format!("{}", a))
            .unwrap_or_else(|| "unknown".to_string());

        match proxy_protocol::consume_proxy_header(&mut self.l4).await {
            Ok(Some(header)) => {
                if let Some(real_addr) = proxy_protocol::source_addr_from_header(&header) {
                    if let Some(digest) = self.l4.get_socket_digest() {
                        let client_addr = SocketAddr::Inet(real_addr);
                        digest.set_client_addr(client_addr.clone());
                        if let Some(proxy_addr) = digest.transport_peer_addr() {
                            debug!(
                                "PROXY protocol updated downstream peer {} -> {}",
                                proxy_addr, client_addr
                            );
                        } else {
                            debug!(
                                "PROXY protocol detected downstream client {}",
                                client_addr
                            );
                        }
                    }
                } else if proxy_protocol::header_has_source_addr(&header) {
                    warn!("PROXY protocol header lacked a usable client address");
                } else {
                    debug!("PROXY protocol header contained no client address (LOCAL command)");
                }
            }
            Ok(None) => {
                debug!("PROXY protocol is enabled but downstream connection from {} sent no header (connection will continue)", peer_str);
            }
            Err(e) => {
                // PROXY protocol parsing failed - this could be due to connection reset
                // Log at debug level since this is expected when connections are reset
                debug!("PROXY protocol parsing failed for connection from {}: {:?} (connection may continue if not a PROXY header)", peer_str, e);
                // Don't propagate the error - let the connection continue
                // The error might be due to connection reset, which is handled elsewhere
            }
        }

        Ok(())
    }
}

/// The struct to hold one more multiple listening endpoints
pub struct Listeners {
    stacks: Vec<TransportStackBuilder>,
    #[cfg(feature = "connection_filter")]
    connection_filter: Option<Arc<dyn ConnectionFilter>>,
}

impl Listeners {
    /// Create a new [`Listeners`] with no listening endpoints.
    pub fn new() -> Self {
        Listeners {
            stacks: vec![],
            #[cfg(feature = "connection_filter")]
            connection_filter: None,
        }
    }
    /// Create a new [`Listeners`] with a TCP server endpoint from the given string.
    pub fn tcp(addr: &str) -> Self {
        let mut listeners = Self::new();
        listeners.add_tcp(addr);
        listeners
    }

    /// Create a new [`Listeners`] with a Unix domain socket endpoint from the given string.
    #[cfg(unix)]
    pub fn uds(addr: &str, perm: Option<Permissions>) -> Self {
        let mut listeners = Self::new();
        listeners.add_uds(addr, perm);
        listeners
    }

    /// Create a new [`Listeners`] with a TLS (TCP) endpoint with the given address string,
    /// and path to the certificate/private key pairs.
    /// This endpoint will adopt the [Mozilla Intermediate](https://wiki.mozilla.org/Security/Server_Side_TLS#Intermediate_compatibility_.28recommended.29)
    /// server side TLS settings.
    pub fn tls(addr: &str, cert_path: &str, key_path: &str) -> Result<Self> {
        let mut listeners = Self::new();
        listeners.add_tls(addr, cert_path, key_path)?;
        Ok(listeners)
    }

    /// Add a TCP endpoint to `self`.
    pub fn add_tcp(&mut self, addr: &str) {
        self.add_address(ServerAddress::Tcp(addr.into(), None));
    }

    /// Add a TCP endpoint to `self`, with the given [`TcpSocketOptions`].
    pub fn add_tcp_with_settings(&mut self, addr: &str, sock_opt: TcpSocketOptions) {
        self.add_address(ServerAddress::Tcp(addr.into(), Some(sock_opt)));
    }

    /// Add a Unix domain socket endpoint to `self`.
    #[cfg(unix)]
    pub fn add_uds(&mut self, addr: &str, perm: Option<Permissions>) {
        self.add_address(ServerAddress::Uds(addr.into(), perm));
    }

    /// Add a TLS endpoint to `self` with the [Mozilla Intermediate](https://wiki.mozilla.org/Security/Server_Side_TLS#Intermediate_compatibility_.28recommended.29)
    /// server side TLS settings.
    pub fn add_tls(&mut self, addr: &str, cert_path: &str, key_path: &str) -> Result<()> {
        self.add_tls_with_settings(addr, None, TlsSettings::intermediate(cert_path, key_path)?);
        Ok(())
    }

    /// Add a TLS endpoint to `self` with the given socket and server side TLS settings.
    /// See [`TlsSettings`] and [`TcpSocketOptions`] for more details.
    pub fn add_tls_with_settings(
        &mut self,
        addr: &str,
        sock_opt: Option<TcpSocketOptions>,
        settings: TlsSettings,
    ) {
        self.add_endpoint(ServerAddress::Tcp(addr.into(), sock_opt), Some(settings));
    }

    /// Add the given [`ServerAddress`] to `self`.
    pub fn add_address(&mut self, addr: ServerAddress) {
        self.add_endpoint(addr, None);
    }

    /// Set a connection filter for all endpoints in this listener collection
    #[cfg(feature = "connection_filter")]
    pub fn set_connection_filter(&mut self, filter: Arc<dyn ConnectionFilter>) {
        log::debug!("Setting connection filter on Listeners");

        // Store the filter for future endpoints
        self.connection_filter = Some(filter.clone());

        // Apply to existing stacks
        for stack in &mut self.stacks {
            stack.connection_filter = Some(filter.clone());
        }
    }

    /// Add the given [`ServerAddress`] to `self` with the given [`TlsSettings`] if provided
    pub fn add_endpoint(&mut self, l4: ServerAddress, tls: Option<TlsSettings>) {
        self.stacks.push(TransportStackBuilder {
            l4,
            tls,
            #[cfg(feature = "connection_filter")]
            connection_filter: self.connection_filter.clone(),
        })
    }

    pub(crate) async fn build(
        &mut self,
        #[cfg(unix)] upgrade_listeners: Option<ListenFds>,
    ) -> Result<Vec<TransportStack>> {
        let mut stacks = Vec::with_capacity(self.stacks.len());

        for b in self.stacks.iter_mut() {
            let new_stack = b
                .build(
                    #[cfg(unix)]
                    upgrade_listeners.clone(),
                )
                .await?;

            stacks.push(new_stack);
        }

        Ok(stacks)
    }

    pub(crate) fn cleanup(&self) {
        // placeholder
    }
}

#[cfg(test)]
mod test {
    use super::*;
    #[cfg(feature = "connection_filter")]
    use std::sync::atomic::{AtomicUsize, Ordering};
    #[cfg(feature = "any_tls")]
    use tokio::io::AsyncWriteExt;
    use tokio::net::TcpStream;
    use tokio::time::{sleep, Duration};

    #[tokio::test]
    async fn test_listen_tcp() {
        let addr1 = "127.0.0.1:7101";
        let addr2 = "127.0.0.1:7102";
        let mut listeners = Listeners::tcp(addr1);
        listeners.add_tcp(addr2);

        let listeners = listeners
            .build(
                #[cfg(unix)]
                None,
            )
            .await
            .unwrap();

        assert_eq!(listeners.len(), 2);
        for listener in listeners {
            tokio::spawn(async move {
                // just try to accept once
                let stream = listener.accept().await.unwrap();
                stream.handshake().await.unwrap();
            });
        }

        // make sure the above starts before the lines below
        sleep(Duration::from_millis(10)).await;

        TcpStream::connect(addr1).await.unwrap();
        TcpStream::connect(addr2).await.unwrap();
    }

    #[tokio::test]
    #[cfg(feature = "any_tls")]
    async fn test_listen_tls() {
        use tokio::io::AsyncReadExt;

        let addr = "127.0.0.1:7103";
        let cert_path = format!("{}/tests/keys/server.crt", env!("CARGO_MANIFEST_DIR"));
        let key_path = format!("{}/tests/keys/key.pem", env!("CARGO_MANIFEST_DIR"));
        let mut listeners = Listeners::tls(addr, &cert_path, &key_path).unwrap();
        let listener = listeners
            .build(
                #[cfg(unix)]
                None,
            )
            .await
            .unwrap()
            .pop()
            .unwrap();

        tokio::spawn(async move {
            // just try to accept once
            let stream = listener.accept().await.unwrap();
            let mut stream = stream.handshake().await.unwrap();
            let mut buf = [0; 1024];
            let _ = stream.read(&mut buf).await.unwrap();
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\n\r\na")
                .await
                .unwrap();
        });
        // make sure the above starts before the lines below
        sleep(Duration::from_millis(10)).await;

        let client = reqwest::Client::builder()
            .danger_accept_invalid_certs(true)
            .build()
            .unwrap();

        let res = client.get(format!("https://{addr}")).send().await.unwrap();
        assert_eq!(res.status(), reqwest::StatusCode::OK);
    }

    #[cfg(feature = "connection_filter")]
    #[test]
    fn test_connection_filter_inheritance() {
        #[derive(Debug, Clone)]
        struct TestFilter {
            counter: Arc<AtomicUsize>,
        }

        #[async_trait]
        impl ConnectionFilter for TestFilter {
            async fn should_accept(&self, _addr: Option<&std::net::SocketAddr>) -> bool {
                self.counter.fetch_add(1, Ordering::SeqCst);
                true
            }
        }

        let mut listeners = Listeners::new();

        // Add an endpoint before setting filter
        listeners.add_tcp("127.0.0.1:7104");

        // Set the connection filter
        let filter = Arc::new(TestFilter {
            counter: Arc::new(AtomicUsize::new(0)),
        });
        listeners.set_connection_filter(filter.clone());

        // Add endpoints after setting filter
        listeners.add_tcp("127.0.0.1:7105");
        #[cfg(feature = "any_tls")]
        {
            // Only test TLS if the feature is enabled
            if let Ok(tls_settings) = TlsSettings::intermediate(
                &format!("{}/tests/keys/server.crt", env!("CARGO_MANIFEST_DIR")),
                &format!("{}/tests/keys/key.pem", env!("CARGO_MANIFEST_DIR")),
            ) {
                listeners.add_tls_with_settings("127.0.0.1:7106", None, tls_settings);
            }
        }

        // Verify all stacks have the filter (only when feature is enabled)
        for stack in &listeners.stacks {
            assert!(
                stack.connection_filter.is_some(),
                "All stacks should have the connection filter set"
            );
        }
    }
}
