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

//! Defines where to connect to and how to connect to a remote server

use ahash::AHasher;
use pingora_error::{
    ErrorType::{InternalError, SocketError},
    OrErr, Result,
};
use std::collections::BTreeMap;
use std::fmt::{Display, Formatter, Result as FmtResult};
use std::hash::{Hash, Hasher};
use std::net::{IpAddr, SocketAddr as InetSocketAddr, ToSocketAddrs as ToInetSocketAddrs};
use std::os::unix::net::SocketAddr as UnixSocketAddr;
use std::os::unix::prelude::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use crate::connectors::L4Connect;
use crate::protocols::l4::socket::SocketAddr;
use crate::protocols::ConnFdReusable;
use crate::protocols::TcpKeepalive;
use crate::tls::x509::X509;
use crate::utils::{get_organization_unit, CertKey};

pub use crate::protocols::ssl::ALPN;

/// The interface to trace the connection
pub trait Tracing: Send + Sync + std::fmt::Debug {
    /// This method is called when successfully connected to a remote server
    fn on_connected(&self);
    /// This method is called when the connection is disconnected.
    fn on_disconnected(&self);
    /// A way to clone itself
    fn boxed_clone(&self) -> Box<dyn Tracing>;
}

/// An object-safe version of Tracing object that can use Clone
#[derive(Debug)]
pub struct Tracer(pub Box<dyn Tracing>);

impl Clone for Tracer {
    fn clone(&self) -> Self {
        Tracer(self.0.boxed_clone())
    }
}

/// [`Peer`] defines the interface to communicate with the [`crate::connectors`] regarding where to
/// connect to and how to connect to it.
pub trait Peer: Display + Clone {
    /// The remote address to connect to
    fn address(&self) -> &SocketAddr;
    /// If TLS should be used;
    fn tls(&self) -> bool;
    /// The SNI to send, if TLS is used
    fn sni(&self) -> &str;
    /// To decide whether a [`Peer`] can use the connection established by another [`Peer`].
    ///
    /// The connections to two peers are considered reusable to each other if their reuse hashes are
    /// the same
    fn reuse_hash(&self) -> u64;
    /// Get the proxy setting to connect to the remote server
    fn get_proxy(&self) -> Option<&Proxy> {
        None
    }
    /// Get the additional options to connect to the peer.
    ///
    /// See [`PeerOptions`] for more details
    fn get_peer_options(&self) -> Option<&PeerOptions> {
        None
    }
    /// Get the additional options for modification.
    fn get_mut_peer_options(&mut self) -> Option<&mut PeerOptions> {
        None
    }
    /// Whether the TLS handshake should validate the cert of the server.
    fn verify_cert(&self) -> bool {
        match self.get_peer_options() {
            Some(opt) => opt.verify_cert,
            None => false,
        }
    }
    /// Whether the TLS handshake should verify that the server cert matches the SNI.
    fn verify_hostname(&self) -> bool {
        match self.get_peer_options() {
            Some(opt) => opt.verify_hostname,
            None => false,
        }
    }
    /// The alternative common name to use to verify the server cert.
    ///
    /// If the server cert doesn't match the SNI, this name will be used to
    /// verify the cert.
    fn alternative_cn(&self) -> Option<&String> {
        match self.get_peer_options() {
            Some(opt) => opt.alternative_cn.as_ref(),
            None => None,
        }
    }
    /// Which local source address this connection should be bind to.
    fn bind_to(&self) -> Option<&InetSocketAddr> {
        match self.get_peer_options() {
            Some(opt) => opt.bind_to.as_ref(),
            None => None,
        }
    }
    /// How long connect() call should be wait before it returns a timeout error.
    fn connection_timeout(&self) -> Option<Duration> {
        match self.get_peer_options() {
            Some(opt) => opt.connection_timeout,
            None => None,
        }
    }
    /// How long the overall connection establishment should take before a timeout error is returned.
    fn total_connection_timeout(&self) -> Option<Duration> {
        match self.get_peer_options() {
            Some(opt) => opt.total_connection_timeout,
            None => None,
        }
    }
    /// If the connection can be reused, how long the connection should wait to be reused before it
    /// shuts down.
    fn idle_timeout(&self) -> Option<Duration> {
        self.get_peer_options().and_then(|o| o.idle_timeout)
    }

    /// Get the ALPN preference.
    fn get_alpn(&self) -> Option<&ALPN> {
        self.get_peer_options().map(|opt| &opt.alpn)
    }

    /// Get the CA cert to use to validate the server cert.
    ///
    /// If not set, the default CAs will be used.
    fn get_ca(&self) -> Option<&Arc<Box<[X509]>>> {
        match self.get_peer_options() {
            Some(opt) => opt.ca.as_ref(),
            None => None,
        }
    }

    /// Get the client cert and key for mutual TLS if any
    fn get_client_cert_key(&self) -> Option<&Arc<CertKey>> {
        None
    }

    /// The TCP keepalive setting that should be applied to this connection
    fn tcp_keepalive(&self) -> Option<&TcpKeepalive> {
        self.get_peer_options()
            .and_then(|o| o.tcp_keepalive.as_ref())
    }

    /// The interval H2 pings to send to the server if any
    fn h2_ping_interval(&self) -> Option<Duration> {
        self.get_peer_options().and_then(|o| o.h2_ping_interval)
    }

    /// The size of the TCP receive buffer should be limited to. See SO_RCVBUF for more details.
    fn tcp_recv_buf(&self) -> Option<usize> {
        self.get_peer_options().and_then(|o| o.tcp_recv_buf)
    }

    /// The DSCP value that should be applied to the send side of this connection.
    /// See the [RFC](https://datatracker.ietf.org/doc/html/rfc2474) for more details.
    fn dscp(&self) -> Option<u8> {
        self.get_peer_options().and_then(|o| o.dscp)
    }

    /// Whether to enable TCP fast open.
    fn tcp_fast_open(&self) -> bool {
        self.get_peer_options()
            .map(|o| o.tcp_fast_open)
            .unwrap_or_default()
    }

    fn matches_fd<V: AsRawFd>(&self, fd: V) -> bool {
        self.address().check_fd_match(fd)
    }

    fn get_tracer(&self) -> Option<Tracer> {
        None
    }
}

/// A simple TCP or TLS peer without many complicated settings.
#[derive(Debug, Clone)]
pub struct BasicPeer {
    pub _address: SocketAddr,
    pub sni: String,
    pub options: PeerOptions,
}

impl BasicPeer {
    /// Create a new [`BasicPeer`].
    pub fn new(address: &str) -> Self {
        let addr = SocketAddr::Inet(address.parse().unwrap()); // TODO: check error
        Self::new_from_sockaddr(addr)
    }

    /// Create a new [`BasicPeer`] with the given path to a Unix domain socket.
    pub fn new_uds<P: AsRef<Path>>(path: P) -> Result<Self> {
        let addr = SocketAddr::Unix(
            UnixSocketAddr::from_pathname(path.as_ref())
                .or_err(InternalError, "while creating BasicPeer")?,
        );
        Ok(Self::new_from_sockaddr(addr))
    }

    fn new_from_sockaddr(sockaddr: SocketAddr) -> Self {
        BasicPeer {
            _address: sockaddr,
            sni: "".to_string(), // TODO: add support for SNI
            options: PeerOptions::new(),
        }
    }
}

impl Display for BasicPeer {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        write!(f, "{:?}", self)
    }
}

impl Peer for BasicPeer {
    fn address(&self) -> &SocketAddr {
        &self._address
    }

    fn tls(&self) -> bool {
        !self.sni.is_empty()
    }

    fn bind_to(&self) -> Option<&InetSocketAddr> {
        None
    }

    fn sni(&self) -> &str {
        &self.sni
    }

    // TODO: change connection pool to accept u64 instead of String
    fn reuse_hash(&self) -> u64 {
        let mut hasher = AHasher::default();
        self._address.hash(&mut hasher);
        hasher.finish()
    }

    fn get_peer_options(&self) -> Option<&PeerOptions> {
        Some(&self.options)
    }
}

/// Define whether to connect via http or https
#[derive(Hash, Clone, Debug, PartialEq)]
pub enum Scheme {
    HTTP,
    HTTPS,
}

impl Display for Scheme {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        match self {
            Scheme::HTTP => write!(f, "HTTP"),
            Scheme::HTTPS => write!(f, "HTTPS"),
        }
    }
}

impl Scheme {
    pub fn from_tls_bool(tls: bool) -> Self {
        if tls {
            Self::HTTPS
        } else {
            Self::HTTP
        }
    }
}

/// The preferences to connect to a remote server
///
/// See [`Peer`] for the meaning of the fields
#[derive(Clone, Debug)]
pub struct PeerOptions {
    pub bind_to: Option<InetSocketAddr>,
    pub connection_timeout: Option<Duration>,
    pub total_connection_timeout: Option<Duration>,
    pub read_timeout: Option<Duration>,
    pub idle_timeout: Option<Duration>,
    pub write_timeout: Option<Duration>,
    pub verify_cert: bool,
    pub verify_hostname: bool,
    /* accept the cert if it's CN matches the SNI or this name */
    pub alternative_cn: Option<String>,
    pub alpn: ALPN,
    pub ca: Option<Arc<Box<[X509]>>>,
    pub tcp_keepalive: Option<TcpKeepalive>,
    pub tcp_recv_buf: Option<usize>,
    pub dscp: Option<u8>,
    pub no_header_eos: bool,
    pub h2_ping_interval: Option<Duration>,
    // how many concurrent h2 stream are allowed in the same connection
    pub max_h2_streams: usize,
    pub extra_proxy_headers: BTreeMap<String, Vec<u8>>,
    // The list of curve the tls connection should advertise
    // if `None`, the default curves will be used
    pub curves: Option<&'static str>,
    // see ssl_use_second_key_share
    pub second_keyshare: bool,
    // whether to enable TCP fast open
    pub tcp_fast_open: bool,
    // use Arc because Clone is required but not allowed in trait object
    pub tracer: Option<Tracer>,
    // A custom L4 connector to use to establish new L4 connections
    pub custom_l4: Option<Arc<dyn L4Connect + Send + Sync>>,
}

impl PeerOptions {
    /// Create a new [`PeerOptions`]
    pub fn new() -> Self {
        PeerOptions {
            bind_to: None,
            connection_timeout: None,
            total_connection_timeout: None,
            read_timeout: None,
            idle_timeout: None,
            write_timeout: None,
            verify_cert: true,
            verify_hostname: true,
            alternative_cn: None,
            alpn: ALPN::H1,
            ca: None,
            tcp_keepalive: None,
            tcp_recv_buf: None,
            dscp: None,
            no_header_eos: false,
            h2_ping_interval: None,
            max_h2_streams: 1,
            extra_proxy_headers: BTreeMap::new(),
            curves: None,
            second_keyshare: true, // default true and noop when not using PQ curves
            tcp_fast_open: false,
            tracer: None,
            custom_l4: None,
        }
    }

    /// Set the ALPN according to the `max` and `min` constrains.
    pub fn set_http_version(&mut self, max: u8, min: u8) {
        self.alpn = ALPN::new(max, min);
    }
}

impl Display for PeerOptions {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        if let Some(b) = self.bind_to {
            write!(f, "bind_to: {:?},", b)?;
        }
        if let Some(t) = self.connection_timeout {
            write!(f, "conn_timeout: {:?},", t)?;
        }
        if let Some(t) = self.total_connection_timeout {
            write!(f, "total_conn_timeout: {:?},", t)?;
        }
        if self.verify_cert {
            write!(f, "verify_cert: true,")?;
        }
        if self.verify_hostname {
            write!(f, "verify_hostname: true,")?;
        }
        if let Some(cn) = &self.alternative_cn {
            write!(f, "alt_cn: {},", cn)?;
        }
        write!(f, "alpn: {},", self.alpn)?;
        if let Some(cas) = &self.ca {
            for ca in cas.iter() {
                write!(
                    f,
                    "CA: {}, expire: {},",
                    get_organization_unit(ca).unwrap_or_default(),
                    ca.not_after()
                )?;
            }
        }
        if let Some(tcp_keepalive) = &self.tcp_keepalive {
            write!(f, "tcp_keepalive: {},", tcp_keepalive)?;
        }
        if self.no_header_eos {
            write!(f, "no_header_eos: true,")?;
        }
        if let Some(h2_ping_interval) = self.h2_ping_interval {
            write!(f, "h2_ping_interval: {:?},", h2_ping_interval)?;
        }
        Ok(())
    }
}

/// A peer representing the remote HTTP server to connect to
#[derive(Debug, Clone)]
pub struct HttpPeer {
    pub _address: SocketAddr,
    pub scheme: Scheme,
    pub sni: String,
    pub proxy: Option<Proxy>,
    pub client_cert_key: Option<Arc<CertKey>>,
    /// a custom field to isolate connection reuse. Requests with different group keys
    /// cannot share connections with each other.
    pub group_key: u64,
    pub options: PeerOptions,
}

impl HttpPeer {
    // These methods are pretty ad-hoc
    pub fn is_tls(&self) -> bool {
        match self.scheme {
            Scheme::HTTP => false,
            Scheme::HTTPS => true,
        }
    }

    fn new_from_sockaddr(address: SocketAddr, tls: bool, sni: String) -> Self {
        HttpPeer {
            _address: address,
            scheme: Scheme::from_tls_bool(tls),
            sni,
            proxy: None,
            client_cert_key: None,
            group_key: 0,
            options: PeerOptions::new(),
        }
    }

    /// Create a new [`HttpPeer`] with the given socket address and TLS settings.
    pub fn new<A: ToInetSocketAddrs>(address: A, tls: bool, sni: String) -> Self {
        let mut addrs_iter = address.to_socket_addrs().unwrap(); //TODO: handle error
        let addr = addrs_iter.next().unwrap();
        Self::new_from_sockaddr(SocketAddr::Inet(addr), tls, sni)
    }

    /// Create a new [`HttpPeer`] with the given path to Unix domain socket and TLS settings.
    pub fn new_uds(path: &str, tls: bool, sni: String) -> Result<Self> {
        let addr = SocketAddr::Unix(
            UnixSocketAddr::from_pathname(Path::new(path)).or_err(SocketError, "invalid path")?,
        );
        Ok(Self::new_from_sockaddr(addr, tls, sni))
    }

    /// Create a new [`HttpPeer`] that uses a proxy to connect to the upstream IP and port
    /// combination.
    pub fn new_proxy(
        next_hop: &str,
        ip_addr: IpAddr,
        port: u16,
        tls: bool,
        sni: &str,
        headers: BTreeMap<String, Vec<u8>>,
    ) -> Self {
        HttpPeer {
            _address: SocketAddr::Inet(InetSocketAddr::new(ip_addr, port)),
            scheme: Scheme::from_tls_bool(tls),
            sni: sni.to_string(),
            proxy: Some(Proxy {
                next_hop: PathBuf::from(next_hop).into(),
                host: ip_addr.to_string(),
                port,
                headers,
            }),
            client_cert_key: None,
            group_key: 0,
            options: PeerOptions::new(),
        }
    }

    fn peer_hash(&self) -> u64 {
        let mut hasher = AHasher::default();
        self.hash(&mut hasher);
        hasher.finish()
    }
}

impl Hash for HttpPeer {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self._address.hash(state);
        self.scheme.hash(state);
        self.proxy.hash(state);
        self.sni.hash(state);
        // client cert serial
        self.client_cert_key.hash(state);
        // origin server cert verification
        self.verify_cert().hash(state);
        self.verify_hostname().hash(state);
        self.alternative_cn().hash(state);
        self.group_key.hash(state);
    }
}

impl Display for HttpPeer {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        write!(f, "addr: {}, scheme: {},", self._address, self.scheme)?;
        if !self.sni.is_empty() {
            write!(f, "sni: {},", self.sni)?;
        }
        if let Some(p) = self.proxy.as_ref() {
            write!(f, "proxy: {p},")?;
        }
        if let Some(cert) = &self.client_cert_key {
            write!(f, "client cert: {},", cert)?;
        }
        Ok(())
    }
}

impl Peer for HttpPeer {
    fn address(&self) -> &SocketAddr {
        &self._address
    }

    fn tls(&self) -> bool {
        self.is_tls()
    }

    fn sni(&self) -> &str {
        &self.sni
    }

    // TODO: change connection pool to accept u64 instead of String
    fn reuse_hash(&self) -> u64 {
        self.peer_hash()
    }

    fn get_peer_options(&self) -> Option<&PeerOptions> {
        Some(&self.options)
    }

    fn get_mut_peer_options(&mut self) -> Option<&mut PeerOptions> {
        Some(&mut self.options)
    }

    fn get_proxy(&self) -> Option<&Proxy> {
        self.proxy.as_ref()
    }

    fn matches_fd<V: AsRawFd>(&self, fd: V) -> bool {
        if let Some(proxy) = self.get_proxy() {
            proxy.next_hop.check_fd_match(fd)
        } else {
            self.address().check_fd_match(fd)
        }
    }

    fn get_client_cert_key(&self) -> Option<&Arc<CertKey>> {
        self.client_cert_key.as_ref()
    }

    fn get_tracer(&self) -> Option<Tracer> {
        self.options.tracer.clone()
    }
}

/// The proxy settings to connect to the remote server, CONNECT only for now
#[derive(Debug, Hash, Clone)]
pub struct Proxy {
    pub next_hop: Box<Path>, // for now this will be the path to the UDS
    pub host: String,        // the proxied host. Could be either IP addr or hostname.
    pub port: u16,           // the port to proxy to
    pub headers: BTreeMap<String, Vec<u8>>, // the additional headers to add to CONNECT
}

impl Display for Proxy {
    fn fmt(&self, f: &mut Formatter) -> FmtResult {
        write!(
            f,
            "next_hop: {}, host: {}, port: {}",
            self.next_hop.display(),
            self.host,
            self.port
        )
    }
}
