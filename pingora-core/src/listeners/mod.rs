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

mod accept_limits;
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
use crate::upstreams::peer::Tracer;

#[cfg(unix)]
use crate::server::ListenFds;

use async_trait::async_trait;
use pingora_error::{Error, Result};
use pingora_timeout::timeout;
use std::{any::Any, fs::Permissions, sync::Arc};

pub(crate) use accept_limits::ConnectionPermit;
use accept_limits::{EndpointLimits, PendingHandshake, SkipReason};
use l4::{ListenerEndpoint, Stream as L4Stream};
use tls::{Acceptor, TlsSettings};

pub use crate::protocols::l4::stream::{
    L4BufferSettings, DEFAULT_L4_READ_BUFFER_SIZE, DEFAULT_L4_WRITE_BUFFER_SIZE,
};
pub use crate::protocols::tls::ALPN;
use crate::protocols::GetSocketDigest;
pub use accept_limits::{AcceptLimits, AcceptStats, DEFAULT_HANDSHAKE_TIMEOUT};
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
#[cfg(any(feature = "openssl_derived", feature = "rustls"))]
pub(crate) type SharedTlsAcceptCallbacks = Arc<dyn TlsAccept + Send + Sync>;

/// Callback for processing raw bytes before TLS handshake.
///
/// This trait allows applications to read and process data from the raw TCP stream
/// before the TLS handshake occurs. This is useful for protocols like HAProxy's
/// PROXY protocol, which sends client address information before TLS.
///
/// # Example
///
/// ```rust,ignore
/// use pingora_core::listeners::PreTlsProcess;
/// use pingora_core::protocols::l4::stream::Stream as L4Stream;
/// use async_trait::async_trait;
///
/// struct ProxyProtocolHandler;
///
/// #[async_trait]
/// impl PreTlsProcess for ProxyProtocolHandler {
///     async fn process(&self, stream: &mut L4Stream) -> pingora_error::Result<()> {
///         // Read PROXY protocol header, update socket digest, etc.
///         Ok(())
///     }
/// }
/// ```
#[async_trait]
pub trait PreTlsProcess: Send + Sync {
    /// Process the raw stream before TLS handshake.
    ///
    /// The implementation can read bytes from the stream (e.g., PROXY protocol header)
    /// and update the stream's socket digest with parsed information such as the
    /// real client address.
    ///
    /// If this method returns an error, the connection will be dropped.
    async fn process(&self, stream: &mut L4Stream) -> Result<()>;
}

/// Type alias for a boxed pre-TLS processor.
pub type PreTlsCallback = Arc<dyn PreTlsProcess>;

struct TransportStackBuilder {
    l4: ServerAddress,
    tls: Option<TlsSettings>,
    l4_buffer: L4BufferSettings,
    accept_limits: AcceptLimits,
    #[cfg(feature = "connection_filter")]
    connection_filter: Option<Arc<dyn ConnectionFilter>>,
    pre_tls_callback: Option<PreTlsCallback>,
    tracer: Option<Tracer>,
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
            l4_buffer: self.l4_buffer,
            pre_tls_callback: self.pre_tls_callback.clone(),
            tracer: self.tracer.clone(),
            limits: Arc::new(EndpointLimits::new(
                self.accept_limits.clone(),
                self.l4.as_ref().to_string(),
            )),
        })
    }
}

/// Configuration for one listening endpoint.
///
/// This configures the endpoint address and endpoint-specific transport
/// settings such as [`TcpSocketOptions`], [`TlsSettings`], L4 buffer sizes
/// ([`L4BufferSettings`]), and limits on accepted connections ([`AcceptLimits`]).
pub struct ListenerConfig {
    l4: ServerAddress,
    tls: Option<TlsSettings>,
    l4_buffer: L4BufferSettings,
    accept_limits: AcceptLimits,
}

impl ListenerConfig {
    /// Create a TCP listening endpoint config.
    pub fn tcp(addr: impl Into<String>) -> Self {
        Self {
            l4: ServerAddress::Tcp(addr.into(), None),
            tls: None,
            l4_buffer: L4BufferSettings::default(),
            accept_limits: AcceptLimits::default(),
        }
    }

    /// Create a Unix domain socket listening endpoint config.
    #[cfg(unix)]
    pub fn uds(addr: impl Into<String>) -> Self {
        Self {
            l4: ServerAddress::Uds(addr.into(), None),
            tls: None,
            l4_buffer: L4BufferSettings::default(),
            accept_limits: AcceptLimits::default(),
        }
    }

    /// Set TCP socket options for this endpoint.
    ///
    /// # Panics
    ///
    /// Panics if this endpoint is not TCP.
    #[track_caller]
    pub fn tcp_socket_options(mut self, options: TcpSocketOptions) -> Self {
        match &mut self.l4 {
            ServerAddress::Tcp(_, opt) => *opt = Some(options),
            #[cfg(unix)]
            ServerAddress::Uds(_, _) => {
                panic!("TCP socket options can only be set on TCP endpoints")
            }
        }
        self
    }

    /// Set Unix domain socket permissions for this endpoint.
    ///
    /// # Panics
    ///
    /// Panics if this endpoint is not a Unix domain socket.
    #[cfg(unix)]
    #[track_caller]
    pub fn permissions(mut self, permissions: Permissions) -> Self {
        match &mut self.l4 {
            ServerAddress::Uds(_, perm) => *perm = Some(permissions),
            ServerAddress::Tcp(_, _) => {
                panic!("Unix domain socket permissions can only be set on UDS endpoints")
            }
        }
        self
    }

    /// Set TLS settings for this endpoint.
    pub fn tls(mut self, settings: TlsSettings) -> Self {
        self.tls = Some(settings);
        self
    }

    /// Set L4 read and write buffer sizes for this endpoint.
    pub fn l4_buffer(mut self, settings: L4BufferSettings) -> Self {
        self.l4_buffer = settings;
        self
    }

    /// Set limits on the connections this endpoint accepts. See [`AcceptLimits`].
    pub fn accept_limits(mut self, limits: AcceptLimits) -> Self {
        self.accept_limits = limits;
        self
    }
}

#[derive(Clone)]
pub(crate) struct TransportStack {
    l4: ListenerEndpoint,
    tls: Option<Arc<Acceptor>>,
    l4_buffer: L4BufferSettings,
    pre_tls_callback: Option<PreTlsCallback>,
    tracer: Option<Tracer>,
    limits: Arc<EndpointLimits>,
}

impl TransportStack {
    pub fn as_str(&self) -> &str {
        self.l4.as_str()
    }

    pub async fn accept(&self) -> Result<UninitializedStream> {
        let mut stream = self.l4.accept().await?;
        // Attach the listener's tracer, if any, the same way the upstream connector does:
        // report the connection now and let `Stream`'s `Drop` report the disconnect. The
        // tracer rides on the L4 stream, which `handshake()` keeps alive in both the plain
        // and the TLS case, so the whole life of the connection is covered -- including
        // connections that never finish the handshake.
        if let Some(tracer) = self.tracer.as_ref() {
            let tracer = tracer.clone();
            tracer.0.on_connected();
            stream.tracer = Some(tracer);
        }
        Ok(UninitializedStream {
            l4: stream,
            tls: self.tls.clone(),
            l4_buffer: self.l4_buffer,
            pre_tls_callback: self.pre_tls_callback.clone(),
            limits: self.limits.clone(),
            pending: PendingHandshake::uncounted(),
        })
    }

    /// Admit an accepted connection under the endpoint's [`AcceptLimits`]. Returns `None`, and
    /// closes the connection, if the endpoint already has
    /// [`max_connections`](AcceptLimits::max_connections) open or
    /// [`max_pending_handshakes`](AcceptLimits::max_pending_handshakes) pending.
    pub fn admit(&self, mut stream: UninitializedStream) -> Option<UninitializedStream> {
        let (permit, pending) = self.limits.admit()?;
        // The stream holds the permit, so the connection counts until the socket closes.
        stream.l4.set_connection_permit(permit);
        stream.pending = pending;
        Some(stream)
    }

    pub fn cleanup(&mut self) {
        // placeholder
    }

    #[cfg(test)]
    pub(crate) fn local_addr(&self) -> Option<std::net::SocketAddr> {
        self.l4.local_addr()
    }
}

pub(crate) struct UninitializedStream {
    l4: L4Stream,
    tls: Option<Arc<Acceptor>>,
    l4_buffer: L4BufferSettings,
    pre_tls_callback: Option<PreTlsCallback>,
    limits: Arc<EndpointLimits>,
    pending: PendingHandshake,
}

/// How a handshake under an endpoint's [`AcceptLimits`] failed.
#[derive(Debug)]
pub(crate) enum HandshakeFailure {
    /// The connection was closed before its handshake started.
    Skipped(SkipReason),
    /// The handshake did not finish within the handshake timeout.
    TimedOut,
    /// The handshake failed.
    Error(Box<Error>),
}

impl From<Box<Error>> for HandshakeFailure {
    fn from(e: Box<Error>) -> Self {
        HandshakeFailure::Error(e)
    }
}

impl UninitializedStream {
    /// Run the handshake under the endpoint's [`AcceptLimits`]: close the connection first if
    /// the limits say so, and give up after the handshake timeout.
    pub(crate) async fn handshake_with_limits(mut self) -> Result<Stream, HandshakeFailure> {
        // Dropped when the handshake finishes, one way or another.
        let _pending = std::mem::replace(&mut self.pending, PendingHandshake::uncounted());
        let limits = self.limits.clone();
        if let Some(reason) = limits.check_before_handshake(&self.l4) {
            return Err(HandshakeFailure::Skipped(reason));
        }
        // A client can give up while its handshake waits for a TLS offload thread.
        let recheck = limits.skips_closed_peers().then(|| limits.clone());
        let handshake = self.handshake_checked(move |l4| {
            match recheck.and_then(|limits| limits.check_before_handshake(l4)) {
                Some(reason) => Err(HandshakeFailure::Skipped(reason)),
                None => Ok(()),
            }
        });
        match timeout(limits.handshake_timeout(), handshake).await {
            Ok(Ok(stream)) => Ok(stream),
            Ok(Err(HandshakeFailure::Error(e))) => {
                limits.count_handshake_error();
                Err(HandshakeFailure::Error(e))
            }
            // Skipped when the offload thread started the handshake.
            Ok(Err(failure)) => Err(failure),
            Err(_) => {
                limits.count_handshake_timeout();
                Err(HandshakeFailure::TimedOut)
            }
        }
    }

    #[cfg(test)]
    pub async fn handshake(self) -> Result<Stream> {
        self.handshake_checked(|_| Ok(())).await
    }

    /// Run [`Self::handshake`]. If a TLS offload thread runs the TLS handshake, `check` runs
    /// first on that thread, and an error from it is returned instead of starting the handshake.
    /// Otherwise `check` is not called.
    async fn handshake_checked<E>(
        mut self,
        check: impl FnOnce(&L4Stream) -> Result<(), E> + Send + 'static,
    ) -> Result<Stream, E>
    where
        E: From<Box<Error>> + Send + 'static,
    {
        self.l4.set_buffer(self.l4_buffer);
        if let Some(tls) = self.tls {
            // Process pre-TLS data if a callback is configured (e.g., PROXY protocol)
            if let Some(ref callback) = self.pre_tls_callback {
                callback.process(&mut self.l4).await?;
            }

            let tls_stream = tls.tls_handshake_checked(self.l4, check).await?;
            Ok(Box::new(tls_stream))
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
}

/// The struct to hold one more multiple listening endpoints
pub struct Listeners {
    stacks: Vec<TransportStackBuilder>,
    #[cfg(feature = "connection_filter")]
    connection_filter: Option<Arc<dyn ConnectionFilter>>,
    pre_tls_callback: Option<PreTlsCallback>,
    tracer: Option<Tracer>,
}

impl Listeners {
    /// Create a new [`Listeners`] with no listening endpoints.
    pub fn new() -> Self {
        Listeners {
            stacks: vec![],
            #[cfg(feature = "connection_filter")]
            connection_filter: None,
            pre_tls_callback: None,
            tracer: None,
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
        self.add_listener(ListenerConfig::tcp(addr));
    }

    /// Add a TCP endpoint to `self`, with the given [`TcpSocketOptions`].
    pub fn add_tcp_with_settings(&mut self, addr: &str, sock_opt: TcpSocketOptions) {
        self.add_listener(ListenerConfig::tcp(addr).tcp_socket_options(sock_opt));
    }

    /// Add a Unix domain socket endpoint to `self`.
    #[cfg(unix)]
    pub fn add_uds(&mut self, addr: &str, perm: Option<Permissions>) {
        let endpoint = perm.map_or_else(
            || ListenerConfig::uds(addr),
            |perm| ListenerConfig::uds(addr).permissions(perm),
        );
        self.add_listener(endpoint);
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
        let mut endpoint = ListenerConfig::tcp(addr).tls(settings);
        if let Some(sock_opt) = sock_opt {
            endpoint = endpoint.tcp_socket_options(sock_opt);
        }
        self.add_listener(endpoint);
    }

    /// Add the given [`ServerAddress`] to `self`.
    pub fn add_address(&mut self, addr: ServerAddress) {
        self.add_endpoint(addr, None);
    }

    /// The configured bind addresses, using the keys expected by transferred listening fds.
    pub fn addresses(&self) -> Vec<String> {
        self.stacks
            .iter()
            .map(|stack| stack.l4.as_ref().to_string())
            .collect()
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

    /// Add the given listener endpoint to `self`.
    pub fn add_listener(&mut self, endpoint: ListenerConfig) {
        let ListenerConfig {
            l4,
            tls,
            l4_buffer,
            accept_limits,
        } = endpoint;
        self.stacks.push(TransportStackBuilder {
            l4,
            tls,
            l4_buffer,
            accept_limits,
            #[cfg(feature = "connection_filter")]
            connection_filter: self.connection_filter.clone(),
            pre_tls_callback: self.pre_tls_callback.clone(),
            tracer: self.tracer.clone(),
        });
    }

    /// Set a pre-TLS callback for all endpoints in this listener collection.
    ///
    /// The callback will be invoked after TCP accept but before the TLS handshake,
    /// allowing the application to read and process data such as PROXY protocol
    /// headers that arrive before TLS.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// use pingora_core::listeners::{Listeners, PreTlsProcess};
    /// use std::sync::Arc;
    ///
    /// let callback = Arc::new(MyProxyProtocolHandler::new());
    /// let mut listeners = Listeners::new();
    /// listeners.set_pre_tls_callback(callback);
    /// listeners.add_tls("0.0.0.0:443", "cert.pem", "key.pem")?;
    /// ```
    pub fn set_pre_tls_callback(&mut self, callback: PreTlsCallback) {
        log::debug!("Setting pre-TLS callback on Listeners");

        // Store the callback for future endpoints
        self.pre_tls_callback = Some(callback.clone());

        // Apply to existing stacks
        for stack in &mut self.stacks {
            stack.pre_tls_callback = Some(callback.clone());
        }
    }

    /// Set a [`Tracer`] to report the lifetime of accepted (downstream) connections.
    ///
    /// The tracer is cloned per connection. [`Tracing::on_connected`] is called right after
    /// `accept()` returns, before the TLS handshake, and [`Tracing::on_disconnected`] is called
    /// by [`Stream`]'s `Drop`. The two therefore pair by construction, and connections that
    /// never finish the handshake are covered as well.
    ///
    /// This is the same mechanism [`crate::upstreams::peer::Peer::get_tracer`] provides for
    /// connections Pingora opens *to* an upstream; this method is the downstream counterpart.
    ///
    /// [`Tracing::on_connected`]: crate::upstreams::peer::Tracing::on_connected
    /// [`Tracing::on_disconnected`]: crate::upstreams::peer::Tracing::on_disconnected
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// use pingora_core::listeners::Listeners;
    /// use pingora_core::upstreams::peer::Tracer;
    ///
    /// let mut listeners = Listeners::new();
    /// listeners.set_tracer(Tracer(Box::new(MyConnectionCount::default())));
    /// listeners.add_tcp("0.0.0.0:8080");
    /// ```
    pub fn set_tracer(&mut self, tracer: Tracer) {
        log::debug!("Setting tracer on Listeners");

        // Store the tracer for future endpoints
        self.tracer = Some(tracer.clone());

        // Apply to existing stacks
        for stack in &mut self.stacks {
            stack.tracer = Some(tracer.clone());
        }
    }

    /// Add the given [`ServerAddress`] to `self` with the given [`TlsSettings`] if provided.
    pub fn add_endpoint(&mut self, l4: ServerAddress, tls: Option<TlsSettings>) {
        self.stacks.push(TransportStackBuilder {
            l4,
            tls,
            l4_buffer: L4BufferSettings::default(),
            accept_limits: AcceptLimits::default(),
            #[cfg(feature = "connection_filter")]
            connection_filter: self.connection_filter.clone(),
            pre_tls_callback: self.pre_tls_callback.clone(),
            tracer: self.tracer.clone(),
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
    #[cfg(feature = "any_tls")]
    use pingora_error::ErrorType::InternalError;
    #[cfg(feature = "connection_filter")]
    use std::sync::atomic::{AtomicUsize, Ordering};
    #[cfg(any(feature = "any_tls", target_os = "linux"))]
    use tokio::io::AsyncWriteExt;
    use tokio::net::TcpStream;

    #[tokio::test]
    async fn test_listen_tcp() {
        let mut listeners = Listeners::tcp("127.0.0.1:0");
        listeners.add_tcp("127.0.0.1:0");

        let listeners = listeners
            .build(
                #[cfg(unix)]
                None,
            )
            .await
            .unwrap();

        assert_eq!(listeners.len(), 2);
        let addrs: Vec<_> = listeners
            .iter()
            .map(|s| s.l4.local_addr().unwrap())
            .collect();
        for listener in listeners {
            tokio::spawn(async move {
                // just try to accept once
                let stream = listener.accept().await.unwrap();
                stream.handshake().await.unwrap();
            });
        }

        // The listeners are already bound (port resolved during build()),
        // so the kernel accepts connections into the backlog immediately.
        // No readiness wait needed — connect will succeed as soon as the
        // OS has completed the TCP handshake.
        TcpStream::connect(addrs[0]).await.unwrap();
        TcpStream::connect(addrs[1]).await.unwrap();
    }

    #[tokio::test]
    async fn test_unbuffered_listener_serves_connection() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let mut listeners = Listeners::new();
        listeners.add_listener(
            ListenerConfig::tcp("127.0.0.1:0").l4_buffer(L4BufferSettings::unbuffered()),
        );
        let listener = listeners
            .build(
                #[cfg(unix)]
                None,
            )
            .await
            .unwrap()
            .pop()
            .unwrap();
        let addr = listener.l4.local_addr().unwrap();

        let server = tokio::spawn(async move {
            let mut stream = listener.accept().await.unwrap().handshake().await.unwrap();
            let mut request = [0; 4];
            stream.read_exact(&mut request).await.unwrap();
            assert_eq!(&request, b"ping");
            stream.write_all(b"pong").await.unwrap();
            stream.flush().await.unwrap();
        });

        let mut client = TcpStream::connect(addr).await.unwrap();
        client.write_all(b"ping").await.unwrap();
        server.await.unwrap();
        let mut response = [0; 4];
        client.read_exact(&mut response).await.unwrap();
        assert_eq!(&response, b"pong");
    }

    #[tokio::test]
    async fn test_listener_tracer_reports_connect_and_disconnect() {
        use crate::upstreams::peer::Tracing;
        use std::sync::atomic::{AtomicUsize, Ordering};

        #[derive(Debug, Default)]
        struct Counters {
            connected: AtomicUsize,
            disconnected: AtomicUsize,
        }

        // A tracer clone must share the counters with the original: the listener clones the
        // tracer once per connection.
        #[derive(Debug, Clone)]
        struct Count(Arc<Counters>);

        impl Tracing for Count {
            fn on_connected(&self) {
                self.0.connected.fetch_add(1, Ordering::Relaxed);
            }
            fn on_disconnected(&self) {
                self.0.disconnected.fetch_add(1, Ordering::Relaxed);
            }
            fn boxed_clone(&self) -> Box<dyn Tracing> {
                Box::new(self.clone())
            }
        }

        let counters = Arc::new(Counters::default());
        let mut listeners = Listeners::tcp("127.0.0.1:0");
        listeners.set_tracer(Tracer(Box::new(Count(counters.clone()))));

        let mut built = listeners
            .build(
                #[cfg(unix)]
                None,
            )
            .await
            .unwrap();
        assert_eq!(built.len(), 1);
        let stack = built.pop().unwrap();
        let addr = stack.l4.local_addr().unwrap();

        let accepted = tokio::spawn(async move { stack.accept().await.unwrap() });
        let client = TcpStream::connect(addr).await.unwrap();
        let stream = accepted.await.unwrap();

        // Reported at accept time, before any handshake.
        assert_eq!(counters.connected.load(Ordering::Relaxed), 1);
        assert_eq!(counters.disconnected.load(Ordering::Relaxed), 0);

        // Dropping the connection without ever handshaking still reports the disconnect,
        // which is the window an application-side counter cannot see.
        drop(stream);
        assert_eq!(counters.connected.load(Ordering::Relaxed), 1);
        assert_eq!(counters.disconnected.load(Ordering::Relaxed), 1);

        drop(client);
    }

    #[tokio::test]
    async fn test_listener_without_tracer_is_unchanged() {
        // The default is no tracer; this is the "nothing changes for anyone who does not opt
        // in" half, and it fails if `accept()` ever starts assuming a tracer is present.
        let mut listeners = Listeners::tcp("127.0.0.1:0");
        let mut built = listeners
            .build(
                #[cfg(unix)]
                None,
            )
            .await
            .unwrap();
        let stack = built.pop().unwrap();
        let addr = stack.l4.local_addr().unwrap();

        let accepted = tokio::spawn(async move { stack.accept().await.unwrap() });
        let _client = TcpStream::connect(addr).await.unwrap();
        let stream = accepted.await.unwrap();
        drop(stream);
    }

    #[test]
    fn test_add_listener_config_tcp_l4_buffer() {
        let mut listeners = Listeners::new();
        let tcp_options = TcpSocketOptions {
            dscp: Some(10),
            ..Default::default()
        };
        let l4_buffer = L4BufferSettings {
            read: Some(0),
            write: None,
        };

        listeners.add_listener(
            ListenerConfig::tcp("127.0.0.1:7107")
                .tcp_socket_options(tcp_options)
                .l4_buffer(l4_buffer),
        );

        assert_eq!(listeners.stacks.len(), 1);
        assert_eq!(listeners.stacks[0].l4_buffer, l4_buffer);
        assert_eq!(listeners.stacks[0].l4_buffer.read_capacity(), 0);
        assert_eq!(
            listeners.stacks[0].l4_buffer.write_capacity(),
            DEFAULT_L4_WRITE_BUFFER_SIZE
        );

        match &listeners.stacks[0].l4 {
            ServerAddress::Tcp(addr, Some(options)) => {
                assert_eq!(addr, "127.0.0.1:7107");
                assert_eq!(options.dscp, Some(10));
            }
            other => panic!("unexpected listener address: {other:?}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn test_add_listener_config_uds_l4_buffer() {
        let mut listeners = Listeners::new();
        let l4_buffer = L4BufferSettings::unbuffered();

        listeners.add_listener(ListenerConfig::uds("/tmp/test_builder_uds").l4_buffer(l4_buffer));

        assert_eq!(listeners.stacks.len(), 1);
        assert_eq!(listeners.stacks[0].l4_buffer, l4_buffer);
        assert_eq!(listeners.stacks[0].l4_buffer.read_capacity(), 0);
        assert_eq!(listeners.stacks[0].l4_buffer.write_capacity(), 0);

        match &listeners.stacks[0].l4 {
            ServerAddress::Uds(addr, None) => assert_eq!(addr, "/tmp/test_builder_uds"),
            other => panic!("unexpected listener address: {other:?}"),
        }
    }

    #[test]
    fn test_l4_buffer_settings_defaults_per_direction() {
        let l4_buffer = L4BufferSettings {
            read: None,
            write: Some(0),
        };

        assert_eq!(l4_buffer.read_capacity(), DEFAULT_L4_READ_BUFFER_SIZE);
        assert_eq!(l4_buffer.write_capacity(), 0);
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
        // The listener is already bound, so the kernel accepts connections
        // into the backlog immediately. No readiness wait needed.
        let client = reqwest::Client::builder()
            .danger_accept_invalid_certs(true)
            .build()
            .unwrap();

        let res = client.get(format!("https://{addr}")).send().await.unwrap();
        assert_eq!(res.status(), reqwest::StatusCode::OK);
    }

    #[tokio::test]
    #[cfg(feature = "any_tls")]
    async fn test_listen_tls_with_offload() {
        use tokio::io::AsyncReadExt;

        const REQUESTS: usize = 8;

        let cert_path = format!("{}/tests/keys/server.crt", env!("CARGO_MANIFEST_DIR"));
        let key_path = format!("{}/tests/keys/key.pem", env!("CARGO_MANIFEST_DIR"));
        let mut tls_settings = TlsSettings::intermediate(&cert_path, &key_path).unwrap();
        let conf = crate::server::configuration::ServerConf {
            downstream_tls_offload_threadpools: Some(2),
            downstream_tls_offload_thread_per_pool: Some(2),
            ..Default::default()
        };
        tls_settings.set_offload_threadpool_from_server_conf(&conf);

        let mut listeners = Listeners::new();
        listeners.add_tls_with_settings("127.0.0.1:0", None, tls_settings);
        let listener = listeners
            .build(
                #[cfg(unix)]
                None,
            )
            .await
            .unwrap()
            .pop()
            .unwrap();
        let addr = listener.l4.local_addr().unwrap();

        let server = tokio::spawn(async move {
            let mut streams = Vec::with_capacity(REQUESTS);
            for _ in 0..REQUESTS {
                streams.push(listener.accept().await.unwrap());
            }

            let mut responses = Vec::with_capacity(REQUESTS);
            for stream in streams {
                responses.push(tokio::spawn(async move {
                    let mut stream = stream.handshake().await.unwrap();
                    let mut buf = [0; 1024];
                    let _ = stream.read(&mut buf).await.unwrap();
                    stream
                        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\n\r\na")
                        .await
                        .unwrap();
                }));
            }

            for response in responses {
                response.await.unwrap();
            }
        });

        let url = format!("https://{addr}");
        let mut requests = Vec::with_capacity(REQUESTS);
        for _ in 0..REQUESTS {
            let url = url.clone();
            requests.push(tokio::spawn(async move {
                let client = reqwest::Client::builder()
                    .danger_accept_invalid_certs(true)
                    .build()
                    .unwrap();
                client.get(url).send().await.unwrap().status()
            }));
        }

        for request in requests {
            assert_eq!(request.await.unwrap(), reqwest::StatusCode::OK);
        }
        server.await.unwrap();
    }

    async fn build_one(config: ListenerConfig) -> TransportStack {
        let mut listeners = Listeners::new();
        listeners.add_listener(config);
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

    /// Whether the peer of `client` closes the connection within a second.
    async fn closed_by_peer(client: &mut TcpStream) -> bool {
        use tokio::io::AsyncReadExt;
        let mut buf = [0; 1];
        match tokio::time::timeout(std::time::Duration::from_secs(1), client.read(&mut buf)).await {
            Ok(Ok(0)) | Ok(Err(_)) => true,
            Ok(Ok(_)) | Err(_) => false,
        }
    }

    #[test]
    fn test_add_listener_config_accept_limits() {
        let mut listeners = Listeners::new();
        listeners.add_listener(
            ListenerConfig::tcp("127.0.0.1:7108").accept_limits(AcceptLimits {
                max_connections: Some(7),
                ..Default::default()
            }),
        );
        listeners.add_tcp("127.0.0.1:7109");

        assert_eq!(listeners.stacks[0].accept_limits.max_connections, Some(7));
        assert_eq!(listeners.stacks[1].accept_limits.max_connections, None);
        assert_eq!(
            listeners.stacks[1].accept_limits.handshake_timeout,
            DEFAULT_HANDSHAKE_TIMEOUT
        );
    }

    #[tokio::test]
    async fn test_max_connections_closes_connections_over_the_limit() {
        let stats = Arc::new(AcceptStats::default());
        let listener = build_one(
            ListenerConfig::tcp("127.0.0.1:0").accept_limits(AcceptLimits {
                max_connections: Some(1),
                stats: Some(stats.clone()),
                ..Default::default()
            }),
        )
        .await;
        let addr = listener.l4.local_addr().unwrap();

        let mut first_client = TcpStream::connect(addr).await.unwrap();
        let first = listener.admit(listener.accept().await.unwrap()).unwrap();
        assert_eq!(stats.open_connections(), 1);
        assert_eq!(stats.pending_handshakes(), 1);

        let mut second_client = TcpStream::connect(addr).await.unwrap();
        assert!(listener.admit(listener.accept().await.unwrap()).is_none());
        assert!(closed_by_peer(&mut second_client).await);
        assert!(!closed_by_peer(&mut first_client).await);
        assert_eq!(stats.closed_at_connection_limit(), 1);
        assert_eq!(stats.open_connections(), 1);

        // Closing the first connection frees its slot.
        drop(first);
        assert_eq!(stats.open_connections(), 0);
        assert_eq!(stats.pending_handshakes(), 0);
        let _third_client = TcpStream::connect(addr).await.unwrap();
        assert!(listener.admit(listener.accept().await.unwrap()).is_some());
    }

    #[tokio::test]
    async fn test_connection_counts_until_its_stream_drops() {
        let stats = Arc::new(AcceptStats::default());
        let listener = build_one(
            ListenerConfig::tcp("127.0.0.1:0").accept_limits(AcceptLimits {
                max_connections: Some(1),
                stats: Some(stats.clone()),
                ..Default::default()
            }),
        )
        .await;
        let addr = listener.l4.local_addr().unwrap();

        let _client = TcpStream::connect(addr).await.unwrap();
        let stream = listener.admit(listener.accept().await.unwrap()).unwrap();
        let stream = stream.handshake_with_limits().await.unwrap();
        assert_eq!(stats.pending_handshakes(), 0);
        assert_eq!(stats.open_connections(), 1);

        // The slot stays taken after the handshake, until the stream is dropped.
        let _second_client = TcpStream::connect(addr).await.unwrap();
        assert!(listener.admit(listener.accept().await.unwrap()).is_none());
        drop(stream);
        assert_eq!(stats.open_connections(), 0);
        let _third_client = TcpStream::connect(addr).await.unwrap();
        assert!(listener.admit(listener.accept().await.unwrap()).is_some());
    }

    #[tokio::test]
    #[cfg(feature = "any_tls")]
    async fn test_handshake_timeout() {
        let cert_path = format!("{}/tests/keys/server.crt", env!("CARGO_MANIFEST_DIR"));
        let key_path = format!("{}/tests/keys/key.pem", env!("CARGO_MANIFEST_DIR"));
        let stats = Arc::new(AcceptStats::default());
        let listener = build_one(
            ListenerConfig::tcp("127.0.0.1:0")
                .tls(TlsSettings::intermediate(&cert_path, &key_path).unwrap())
                .accept_limits(AcceptLimits {
                    handshake_timeout: std::time::Duration::from_millis(100),
                    max_pending_handshakes: Some(1),
                    stats: Some(stats.clone()),
                    ..Default::default()
                }),
        )
        .await;
        let addr = listener.l4.local_addr().unwrap();

        // A client that never sends its ClientHello.
        let _client = TcpStream::connect(addr).await.unwrap();
        let stream = listener.admit(listener.accept().await.unwrap()).unwrap();
        let started = std::time::Instant::now();
        let handshake = stream.handshake_with_limits().await;
        assert!(matches!(handshake, Err(HandshakeFailure::TimedOut)));
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
        assert_eq!(stats.handshake_timeouts(), 1);
        assert_eq!(stats.pending_handshakes(), 0);
        // The timed-out handshake gave back its handshake slot.
        let _second_client = TcpStream::connect(addr).await.unwrap();
        assert!(listener.admit(listener.accept().await.unwrap()).is_some());
        assert_eq!(stats.open_connections(), 0);
    }

    #[tokio::test]
    #[cfg(target_os = "linux")]
    async fn test_skip_closed_peers() {
        let stats = Arc::new(AcceptStats::default());
        let listener = build_one(
            ListenerConfig::tcp("127.0.0.1:0").accept_limits(AcceptLimits {
                skip_closed_peers: true,
                stats: Some(stats.clone()),
                ..Default::default()
            }),
        )
        .await;
        let addr = listener.l4.local_addr().unwrap();

        // A client that sends its first bytes and gives up before the server gets to them.
        let mut gone = TcpStream::connect(addr).await.unwrap();
        gone.write_all(b"hello").await.unwrap();
        drop(gone);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let stream = listener.admit(listener.accept().await.unwrap()).unwrap();
        let handshake = stream.handshake_with_limits().await;
        assert!(matches!(
            handshake,
            Err(HandshakeFailure::Skipped(SkipReason::PeerClosed))
        ));
        assert_eq!(stats.closed_peer_gone(), 1);
        assert_eq!(stats.pending_handshakes(), 0);
        assert_eq!(stats.open_connections(), 0);

        let mut waiting = TcpStream::connect(addr).await.unwrap();
        waiting.write_all(b"hello").await.unwrap();
        let stream = listener.admit(listener.accept().await.unwrap()).unwrap();
        let handshake = stream.handshake_with_limits().await;
        assert!(handshake.is_ok());
        assert_eq!(stats.closed_peer_gone(), 1);
    }

    #[tokio::test]
    async fn test_max_pending_handshakes_closes_connections_over_the_limit() {
        let stats = Arc::new(AcceptStats::default());
        let listener = build_one(
            ListenerConfig::tcp("127.0.0.1:0").accept_limits(AcceptLimits {
                max_pending_handshakes: Some(1),
                stats: Some(stats.clone()),
                ..Default::default()
            }),
        )
        .await;
        let addr = listener.l4.local_addr().unwrap();

        let _first_client = TcpStream::connect(addr).await.unwrap();
        let first = listener.admit(listener.accept().await.unwrap()).unwrap();
        let mut second_client = TcpStream::connect(addr).await.unwrap();
        assert!(listener.admit(listener.accept().await.unwrap()).is_none());
        assert!(closed_by_peer(&mut second_client).await);
        assert_eq!(stats.closed_at_handshake_limit(), 1);

        // Once its handshake ends, the first connection stays open but frees its slot.
        let _stream = first.handshake_with_limits().await.unwrap();
        assert_eq!(stats.pending_handshakes(), 0);
        assert_eq!(stats.open_connections(), 1);
        let _third_client = TcpStream::connect(addr).await.unwrap();
        assert!(listener.admit(listener.accept().await.unwrap()).is_some());
    }

    /// A TLS endpoint whose handshakes run on a single offload thread.
    #[cfg(feature = "any_tls")]
    async fn build_offloaded_tls(limits: AcceptLimits) -> TransportStack {
        let cert_path = format!("{}/tests/keys/server.crt", env!("CARGO_MANIFEST_DIR"));
        let key_path = format!("{}/tests/keys/key.pem", env!("CARGO_MANIFEST_DIR"));
        let mut tls = TlsSettings::intermediate(&cert_path, &key_path).unwrap();
        tls.set_offload_threadpool(1, 1);
        build_one(
            ListenerConfig::tcp("127.0.0.1:0")
                .tls(tls)
                .accept_limits(limits),
        )
        .await
    }

    /// Hold the only offload thread of `listener` until the returned sender sends or is dropped.
    #[cfg(feature = "any_tls")]
    async fn hold_offload_thread(listener: &TransportStack) -> std::sync::mpsc::Sender<()> {
        let acceptor = listener.tls.clone().unwrap();
        let held = Arc::new(tokio::sync::Notify::new());
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let (_, server) = tokio::io::duplex(1024);
        let notify = held.clone();
        tokio::spawn(async move {
            acceptor
                .tls_handshake_checked(server, move |_| {
                    notify.notify_one();
                    let _ = release_rx.recv();
                    Err(Error::explain(InternalError, "held"))
                })
                .await
        });
        held.notified().await;
        release_tx
    }

    #[tokio::test]
    #[cfg(feature = "any_tls")]
    async fn test_offloaded_tls_handshake_runs_its_check_first_on_the_offload_thread() {
        let listener = build_offloaded_tls(AcceptLimits::default()).await;
        let acceptor = listener.tls.clone().unwrap();
        // The client never sends a ClientHello, so a handshake that started would not finish.
        let (_client, server) = tokio::io::duplex(1024);
        let handshake = acceptor.tls_handshake_checked(server, |_| {
            let thread = std::thread::current().name().unwrap_or_default().to_owned();
            Err(Error::explain(InternalError, thread))
        });
        let handshake = tokio::time::timeout(std::time::Duration::from_secs(5), handshake)
            .await
            .expect("the handshake started");
        let Err(e) = handshake else {
            panic!("the check did not stop the handshake");
        };
        assert!(e.to_string().contains("downstream TLS offload"), "{e}");
    }

    #[tokio::test]
    #[cfg(feature = "any_tls")]
    async fn test_connection_counts_until_its_offloaded_handshake_is_dropped() {
        use std::time::Duration;

        let stats = Arc::new(AcceptStats::default());
        let listener = build_offloaded_tls(AcceptLimits {
            handshake_timeout: Duration::from_millis(100),
            max_connections: Some(1),
            stats: Some(stats.clone()),
            ..Default::default()
        })
        .await;
        let addr = listener.l4.local_addr().unwrap();
        let release = hold_offload_thread(&listener).await;

        // The handshake times out while it waits for the offload thread, which owns the socket.
        let mut client = TcpStream::connect(addr).await.unwrap();
        let stream = listener.admit(listener.accept().await.unwrap()).unwrap();
        let handshake = stream.handshake_with_limits().await;
        assert!(matches!(handshake, Err(HandshakeFailure::TimedOut)));
        assert_eq!(stats.open_connections(), 1);
        let mut second_client = TcpStream::connect(addr).await.unwrap();
        assert!(listener.admit(listener.accept().await.unwrap()).is_none());
        assert!(closed_by_peer(&mut second_client).await);

        // The offload thread drops the handshake when it gets to it, closing the socket.
        release.send(()).unwrap();
        assert!(closed_by_peer(&mut client).await);
        for _ in 0..100 {
            if stats.open_connections() == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(stats.open_connections(), 0);
        let _third_client = TcpStream::connect(addr).await.unwrap();
        assert!(listener.admit(listener.accept().await.unwrap()).is_some());
    }

    #[tokio::test]
    #[cfg(all(target_os = "linux", feature = "any_tls"))]
    async fn test_skip_closed_peers_checks_again_when_the_offload_thread_starts() {
        use std::time::Duration;

        let stats = Arc::new(AcceptStats::default());
        let listener = build_offloaded_tls(AcceptLimits {
            skip_closed_peers: true,
            stats: Some(stats.clone()),
            ..Default::default()
        })
        .await;
        let addr = listener.l4.local_addr().unwrap();

        // Keep the only offload thread busy until the client below has given up.
        let release = hold_offload_thread(&listener).await;

        let client = TcpStream::connect(addr).await.unwrap();
        let stream = listener.admit(listener.accept().await.unwrap()).unwrap();
        let handshake = tokio::spawn(stream.handshake_with_limits());
        // The handshake passes the first check, then waits for the offload thread.
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(stats.closed_peer_gone(), 0);
        drop(client);
        tokio::time::sleep(Duration::from_millis(50)).await;
        release.send(()).unwrap();

        let handshake = handshake.await.unwrap();
        assert!(matches!(
            handshake,
            Err(HandshakeFailure::Skipped(SkipReason::PeerClosed))
        ));
        assert_eq!(stats.closed_peer_gone(), 1);
        assert_eq!(stats.handshake_errors(), 0);
        assert_eq!(stats.pending_handshakes(), 0);
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
