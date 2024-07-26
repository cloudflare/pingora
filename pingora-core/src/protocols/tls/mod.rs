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

//! The TLS layer implementations

use async_trait::async_trait;
use pingora_error::Result;
use std::ops::{Deref, DerefMut};
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tokio::io::{AsyncRead, AsyncWrite};

use crate::protocols::digest::TimingDigest;
use crate::protocols::raw_connect::ProxyDigest;
use crate::protocols::{
    GetProxyDigest, GetSocketDigest, GetTimingDigest, SocketDigest, UniqueID, IO,
};

#[cfg(not(feature = "rustls"))]
pub(crate) mod boringssl_openssl;
#[cfg(feature = "rustls")]
pub(crate) mod rustls;
pub mod server;

#[cfg(not(feature = "rustls"))]
use boringssl_openssl::stream::InnerStream;
#[cfg(feature = "rustls")]
use rustls::stream::InnerStream;

/// The TLS connection
#[derive(Debug)]
pub struct TlsStream<T> {
    tls: InnerStream<T>,
    digest: Option<Arc<SslDigest>>,
    timing: TimingDigest,
}

#[async_trait]
pub trait InnerTlsStream {
    async fn connect(&mut self) -> Result<()>;
    async fn accept(&mut self) -> Result<()>;

    /// Return the [`ssl::SslDigest`] for logging
    fn digest(&mut self) -> Option<Arc<SslDigest>>;

    /// Return selected ALPN if any
    fn selected_alpn_proto(&mut self) -> Option<ALPN>;
}

impl GetSocketDigest for Box<dyn IO + Send> {
    fn get_socket_digest(&self) -> Option<Arc<SocketDigest>> {
        (**self).get_socket_digest()
    }
    fn set_socket_digest(&mut self, socket_digest: SocketDigest) {
        (**self).set_socket_digest(socket_digest)
    }
}

impl GetTimingDigest for Box<dyn IO + Send> {
    fn get_timing_digest(&self) -> Vec<Option<TimingDigest>> {
        vec![]
    }
}

impl GetProxyDigest for Box<dyn IO + Send> {
    fn get_proxy_digest(&self) -> Option<Arc<ProxyDigest>> {
        (**self).get_proxy_digest()
    }
}

impl UniqueID for Box<dyn IO + Send> {
    fn id(&self) -> i32 {
        (**self).id()
    }
}

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

    #[cfg(not(feature = "rustls"))]
    pub(crate) fn to_wire_preference(&self) -> &[u8] {
        // https://www.openssl.org/docs/manmaster/man3/SSL_CTX_set_alpn_select_cb.html
        // "vector of nonempty, 8-bit length-prefixed, byte strings"
        match self {
            Self::H1 => b"\x08http/1.1",
            Self::H2 => b"\x02h2",
            Self::H2H1 => b"\x02h2\x08http/1.1",
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

    pub(crate) fn from_wire_selected(raw: &[u8]) -> Option<Self> {
        match raw {
            b"http/1.1" => Some(Self::H1),
            b"h2" => Some(Self::H2),
            _ => None,
        }
    }
}

/// The TLS connection information
#[derive(Clone, Debug)]
pub struct SslDigest {
    /// The cipher used
    pub cipher: &'static str,
    /// The TLS version of this connection
    pub version: &'static str,
    /// The organization of the peer's certificate
    pub organization: Option<String>,
    /// The serial number of the peer's certificate
    pub serial_number: Option<String>,
    /// The digest of the peer's certificate
    pub cert_digest: Vec<u8>,
}

impl<S> GetSocketDigest for TlsStream<S>
where
    S: GetSocketDigest,
{
    fn get_socket_digest(&self) -> Option<Arc<SocketDigest>> {
        self.tls.get_socket_digest()
    }
    fn set_socket_digest(&mut self, socket_digest: SocketDigest) {
        self.tls.set_socket_digest(socket_digest)
    }
}

impl<S> GetTimingDigest for TlsStream<S>
where
    S: GetTimingDigest,
{
    fn get_timing_digest(&self) -> Vec<Option<TimingDigest>> {
        let mut ts_vec = self.tls.get_timing_digest();
        ts_vec.push(Some(self.timing.clone()));
        ts_vec
    }
    fn get_read_pending_time(&self) -> Duration {
        self.tls.get_read_pending_time()
    }

    fn get_write_pending_time(&self) -> Duration {
        self.tls.get_write_pending_time()
    }
}

impl<S> GetProxyDigest for TlsStream<S>
where
    S: GetProxyDigest,
{
    fn get_proxy_digest(&self) -> Option<Arc<ProxyDigest>> {
        self.tls.get_proxy_digest()
    }
}

impl<T> TlsStream<T> {
    pub fn ssl_digest(&self) -> Option<Arc<SslDigest>> {
        self.digest.clone()
    }
}

impl<T> TlsStream<T>
where
    T: AsyncRead + AsyncWrite + Unpin + Send,
{
    /// Connect to the remote TLS server as a client
    pub(crate) async fn connect(&mut self) -> Result<()> {
        self.tls.connect().await?;
        self.timing.established_ts = SystemTime::now();
        self.digest = self.tls.digest();
        Ok(())
    }

    /// Finish the TLS handshake from client as a server
    pub(crate) async fn accept(&mut self) -> Result<()> {
        self.tls.accept().await?;
        self.timing.established_ts = SystemTime::now();
        self.digest = self.tls.digest();
        Ok(())
    }
}

impl<T> Deref for TlsStream<T> {
    type Target = InnerStream<T>;

    fn deref(&self) -> &Self::Target {
        &self.tls
    }
}

impl<T> DerefMut for TlsStream<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.tls
    }
}
