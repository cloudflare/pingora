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

use crate::protocols::{l4::socket::SocketAddr, tls::TlsRef, Stream};

#[cfg(unix)]
use crate::server::ListenFds;

use async_trait::async_trait;
use pingora_error::Result;
use std::{any::Any, fs::Permissions, sync::Arc};

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

    /// This function is called after the TLS handshake is complete.
    ///
    /// Any value returned from this function (other than `None`) will be stored in the
    /// `extension` field of `SslDigest`. This allows you to attach custom application-specific
    /// data to the TLS connection, which will be accessible from the HTTP layer via the
    /// `SslDigest` attached to the session digest.
    async fn handshake_complete_callback(
        &self,
        _ssl: &TlsRef,
    ) -> Option<Arc<dyn Any + Send + Sync>> {
        None
    }
}

pub type TlsAcceptCallbacks = Box<dyn TlsAccept + Send + Sync>;

/// Some protocols, such as the proxy protocol, must be inspected before the TLS
/// handshake. The below trait provides access to the raw TCP stream right
/// before TLS for these situations.
#[async_trait]
pub trait InspectPreTls: Send + Sync {
    /// The implementation can read bytes from the stream (e.g., PROXY protocol header)
    /// before the TLS handshake takes place.
    ///
    /// If this method returns an error, the connection will be dropped.
    async fn inspect(&self, stream: &mut L4Stream) -> Result<()>;
}

struct TransportStackBuilder {
    l4: ServerAddress,
    tls: Option<TlsSettings>,
    #[cfg(feature = "connection_filter")]
    connection_filter: Option<Arc<dyn ConnectionFilter>>,
    pre_tls_inspector: Option<Arc<dyn InspectPreTls>>,
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
            pre_tls_inspector: self.pre_tls_inspector.clone(),
        })
    }
}

#[derive(Clone)]
pub(crate) struct TransportStack {
    l4: ListenerEndpoint,
    tls: Option<Arc<Acceptor>>,
    pre_tls_inspector: Option<Arc<dyn InspectPreTls>>,
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
            pre_tls_inspector: self.pre_tls_inspector.clone(),
        })
    }

    pub fn cleanup(&mut self) {
        // placeholder
    }
}

pub(crate) struct UninitializedStream {
    l4: L4Stream,
    tls: Option<Arc<Acceptor>>,
    pre_tls_inspector: Option<Arc<dyn InspectPreTls>>,
}

impl UninitializedStream {
    pub async fn handshake(mut self) -> Result<Stream> {
        self.l4.set_buffer();

        // Expose raw l4 stream to any registered pre-TLS inspectors before
        // handshaking.
        if let Some(inspector) = self.pre_tls_inspector.as_ref() {
            inspector.inspect(&mut self.l4).await?;
        }

        let res_with_stream: Result<Stream> = if let Some(tls) = self.tls {
            let tls_stream = tls.tls_handshake(self.l4).await?;
            Ok(Box::new(tls_stream))
        } else {
            Ok(Box::new(self.l4))
        };

        res_with_stream
    }

    /// Get the peer address of the connection if available
    pub fn peer_addr(&self) -> Option<SocketAddr> {
        self.l4
            .get_socket_digest()
            .and_then(|d| d.peer_addr().cloned())
    }
}

/// The struct to hold one more multiple listening endpoints
pub struct Listeners {
    stacks: Vec<TransportStackBuilder>,
    #[cfg(feature = "connection_filter")]
    connection_filter: Option<Arc<dyn ConnectionFilter>>,
    pre_tls_inspector: Option<Arc<dyn InspectPreTls>>,
}

impl Listeners {
    /// Create a new [`Listeners`] with no listening endpoints.
    pub fn new() -> Self {
        Listeners {
            stacks: vec![],
            #[cfg(feature = "connection_filter")]
            connection_filter: None,
            pre_tls_inspector: None,
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

    /// Set a pre-TLS inspector for all endpoints in this listener collection.
    ///
    /// The inspector will be invoked after TCP accept but before the TLS handshake,
    /// allowing the application to read and process data such as PROXY protocol
    /// headers that arrive before TLS.
    pub fn set_pre_tls_inspector(&mut self, inspector: Arc<dyn InspectPreTls>) {
        log::debug!("Setting pre-TLS inspector on Listeners");

        // Store the inspector for future endpoints
        self.pre_tls_inspector = Some(inspector.clone());

        // Apply to existing stacks
        for stack in &mut self.stacks {
            stack.pre_tls_inspector = Some(inspector.clone());
        }
    }

    /// Add the given [`ServerAddress`] to `self` with the given [`TlsSettings`] if provided
    pub fn add_endpoint(&mut self, l4: ServerAddress, tls: Option<TlsSettings>) {
        self.stacks.push(TransportStackBuilder {
            l4,
            tls,
            #[cfg(feature = "connection_filter")]
            connection_filter: self.connection_filter.clone(),
            pre_tls_inspector: self.pre_tls_inspector.clone(),
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
        let addr1 = "127.0.0.1:7107";
        let addr2 = "127.0.0.1:7108";
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

    #[tokio::test]
    #[cfg(any(feature = "openssl", feature = "boringssl"))]
    async fn test_inspect_pre_tls() {
        use pingora_error::{Error, Result};
        use std::pin::Pin;
        use std::sync::{Arc, Mutex};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        use crate::protocols::tls::SslStream;
        use crate::tls::ssl;
        struct HelloInspector {
            stored_bytes: Arc<Mutex<Vec<u8>>>,
        }

        #[async_trait]
        impl InspectPreTls for HelloInspector {
            async fn inspect(&self, stream: &mut L4Stream) -> Result<()> {
                let mut buf = [0u8; 5];
                stream.read_exact(&mut buf).await.map_err(|e| {
                    Error::new_str("failed to read pre-TLS bytes").more_context(format!("{e}"))
                })?;
                self.stored_bytes.lock().unwrap().extend_from_slice(&buf);
                if &buf != b"hello" {
                    return Err(Error::new_str("pre-TLS bytes did not match 'hello'"));
                }
                Ok(())
            }
        }

        let stored = Arc::new(Mutex::new(Vec::new()));
        let inspector = Arc::new(HelloInspector {
            stored_bytes: stored.clone(),
        });

        let addr = "127.0.0.1:7109";
        let cert_path = format!("{}/tests/keys/server.crt", env!("CARGO_MANIFEST_DIR"));
        let key_path = format!("{}/tests/keys/key.pem", env!("CARGO_MANIFEST_DIR"));
        let mut listeners = Listeners::tls(addr, &cert_path, &key_path).unwrap();

        // Register HelloInspector on the listener so it fires before TLS handshaking.
        listeners.set_pre_tls_inspector(inspector.clone());
        let listener = listeners
            .build(
                #[cfg(unix)]
                None,
            )
            .await
            .unwrap()
            .pop()
            .unwrap();

        let server_handle = tokio::spawn(async move {
            // Acceptor thread should handshake, which will perform pre-TLS inspection
            // and then the TLS handshake.
            let stream = listener.accept().await.unwrap();
            stream.handshake().await.unwrap();
        });

        // make sure the above starts before the lines below
        sleep(Duration::from_millis(10)).await;

        let client_handle = tokio::spawn(async move {
            // Prepend the TLS handshake with the bytes "hello".
            let mut tcp_stream = tokio::net::TcpStream::connect(addr).await.unwrap();
            tcp_stream.write_all(b"hello").await.unwrap();

            // Perform the TLS handshake with verification disabled because the
            // certificates aren't actually valid.
            let ssl_context = ssl::SslContext::builder(ssl::SslMethod::tls())
                .unwrap()
                .build();
            let mut ssl_obj = ssl::Ssl::new(&ssl_context).unwrap();
            ssl_obj.set_verify(ssl::SslVerifyMode::NONE);
            let mut tls_stream = SslStream::new(ssl_obj, tcp_stream).unwrap();
            Pin::new(&mut tls_stream).connect().await.unwrap();
        });

        server_handle.await.unwrap();
        client_handle.await.unwrap();

        assert_eq!(&*stored.lock().unwrap(), b"hello");
    }
}
