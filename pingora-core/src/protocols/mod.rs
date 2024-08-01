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

//! Abstractions and implementations for protocols including TCP, TLS and HTTP

mod digest;
pub mod http;
pub mod l4;
pub mod raw_connect;
pub mod ssl;
#[cfg(windows)]
mod windows;

pub use digest::{
    Digest, GetProxyDigest, GetSocketDigest, GetTimingDigest, ProtoDigest, SocketDigest,
    TimingDigest,
};
pub use l4::ext::TcpKeepalive;
pub use ssl::ALPN;

use async_trait::async_trait;
use std::fmt::Debug;
use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;

/// Define how a protocol should shutdown its connection.
#[async_trait]
pub trait Shutdown {
    async fn shutdown(&mut self) -> ();
}

#[cfg(unix)]
pub type UniqueIDType = i32;
#[cfg(windows)]
pub type UniqueIDType = usize;

/// Define how a given session/connection identifies itself.
pub trait UniqueID {
    /// The ID returned should be unique among all existing connections of the same type.
    /// But ID can be recycled after a connection is shutdown.
    fn id(&self) -> UniqueIDType;
}

/// Interface to get TLS info
pub trait Ssl {
    /// Return the TLS info if the connection is over TLS
    fn get_ssl(&self) -> Option<&crate::tls::ssl::SslRef> {
        None
    }

    /// Return the [`ssl::SslDigest`] for logging
    fn get_ssl_digest(&self) -> Option<Arc<ssl::SslDigest>> {
        None
    }

    /// Return selected ALPN if any
    fn selected_alpn_proto(&self) -> Option<ALPN> {
        let ssl = self.get_ssl()?;
        ALPN::from_wire_selected(ssl.selected_alpn_protocol()?)
    }
}

use std::any::Any;
use tokio::io::{AsyncRead, AsyncWrite};

/// The abstraction of transport layer IO
pub trait IO:
    AsyncRead
    + AsyncWrite
    + Shutdown
    + UniqueID
    + Ssl
    + GetTimingDigest
    + GetProxyDigest
    + GetSocketDigest
    + Unpin
    + Debug
    + Send
    + Sync
{
    /// helper to cast as the reference of the concrete type
    fn as_any(&self) -> &dyn Any;
    /// helper to cast back of the concrete type
    fn into_any(self: Box<Self>) -> Box<dyn Any>;
}

impl<
        T: AsyncRead
            + AsyncWrite
            + Shutdown
            + UniqueID
            + Ssl
            + GetTimingDigest
            + GetProxyDigest
            + GetSocketDigest
            + Unpin
            + Debug
            + Send
            + Sync,
    > IO for T
where
    T: 'static,
{
    fn as_any(&self) -> &dyn Any {
        self
    }
    fn into_any(self: Box<Self>) -> Box<dyn Any> {
        self
    }
}

/// The type of any established transport layer connection
pub type Stream = Box<dyn IO>;

// Implement IO trait for 3rd party types, mostly for testing
mod ext_io_impl {
    use super::*;
    use tokio_test::io::Mock;

    #[async_trait]
    impl Shutdown for Mock {
        async fn shutdown(&mut self) -> () {}
    }
    impl UniqueID for Mock {
        fn id(&self) -> UniqueIDType {
            0
        }
    }
    impl Ssl for Mock {}
    impl GetTimingDigest for Mock {
        fn get_timing_digest(&self) -> Vec<Option<TimingDigest>> {
            vec![]
        }
    }
    impl GetProxyDigest for Mock {
        fn get_proxy_digest(&self) -> Option<Arc<raw_connect::ProxyDigest>> {
            None
        }
    }
    impl GetSocketDigest for Mock {
        fn get_socket_digest(&self) -> Option<Arc<SocketDigest>> {
            None
        }
    }

    use std::io::Cursor;

    #[async_trait]
    impl<T: Send> Shutdown for Cursor<T> {
        async fn shutdown(&mut self) -> () {}
    }
    impl<T> UniqueID for Cursor<T> {
        fn id(&self) -> UniqueIDType {
            0
        }
    }
    impl<T> Ssl for Cursor<T> {}
    impl<T> GetTimingDigest for Cursor<T> {
        fn get_timing_digest(&self) -> Vec<Option<TimingDigest>> {
            vec![]
        }
    }
    impl<T> GetProxyDigest for Cursor<T> {
        fn get_proxy_digest(&self) -> Option<Arc<raw_connect::ProxyDigest>> {
            None
        }
    }
    impl<T> GetSocketDigest for Cursor<T> {
        fn get_socket_digest(&self) -> Option<Arc<SocketDigest>> {
            None
        }
    }

    use tokio::io::DuplexStream;

    #[async_trait]
    impl Shutdown for DuplexStream {
        async fn shutdown(&mut self) -> () {}
    }
    impl UniqueID for DuplexStream {
        fn id(&self) -> UniqueIDType {
            0
        }
    }
    impl Ssl for DuplexStream {}
    impl GetTimingDigest for DuplexStream {
        fn get_timing_digest(&self) -> Vec<Option<TimingDigest>> {
            vec![]
        }
    }
    impl GetProxyDigest for DuplexStream {
        fn get_proxy_digest(&self) -> Option<Arc<raw_connect::ProxyDigest>> {
            None
        }
    }
    impl GetSocketDigest for DuplexStream {
        fn get_socket_digest(&self) -> Option<Arc<SocketDigest>> {
            None
        }
    }
}

#[cfg(unix)]
pub(crate) trait ConnFdReusable {
    fn check_fd_match<V: AsRawFd>(&self, fd: V) -> bool;
}
#[cfg(windows)]
pub(crate) trait ConnSockReusable {
    fn check_sock_match<V: AsRawSocket>(&self, sock: V) -> bool;
}

use l4::socket::SocketAddr;
use log::{debug, error};
#[cfg(unix)]
use nix::sys::socket::{getpeername, SockaddrStorage, UnixAddr};
#[cfg(unix)]
use std::os::unix::prelude::AsRawFd;
#[cfg(windows)]
use std::os::windows::io::AsRawSocket;
use std::{net::SocketAddr as InetSocketAddr, path::Path};

#[cfg(unix)]
impl ConnFdReusable for SocketAddr {
    fn check_fd_match<V: AsRawFd>(&self, fd: V) -> bool {
        match self {
            SocketAddr::Inet(addr) => addr.check_fd_match(fd),
            SocketAddr::Unix(addr) => addr
                .as_pathname()
                .expect("non-pathname unix sockets not supported as peer")
                .check_fd_match(fd),
        }
    }
}
#[cfg(windows)]
impl ConnSockReusable for SocketAddr {
    fn check_sock_match<V: AsRawSocket>(&self, sock: V) -> bool {
        match self {
            SocketAddr::Inet(addr) => addr.check_sock_match(sock),
        }
    }
}

#[cfg(unix)]
impl ConnFdReusable for Path {
    fn check_fd_match<V: AsRawFd>(&self, fd: V) -> bool {
        let fd = fd.as_raw_fd();
        match getpeername::<UnixAddr>(fd) {
            Ok(peer) => match UnixAddr::new(self) {
                Ok(addr) => {
                    if addr == peer {
                        debug!("Unix FD to: {peer} is reusable");
                        true
                    } else {
                        error!("Crit: unix FD mismatch: fd: {fd:?}, peer: {peer}, addr: {addr}",);
                        false
                    }
                }
                Err(e) => {
                    error!("Bad addr: {self:?}, error: {e:?}");
                    false
                }
            },
            Err(e) => {
                error!("Idle unix connection is broken: {e:?}");
                false
            }
        }
    }
}

#[cfg(unix)]
impl ConnFdReusable for InetSocketAddr {
    fn check_fd_match<V: AsRawFd>(&self, fd: V) -> bool {
        let fd = fd.as_raw_fd();
        match getpeername::<SockaddrStorage>(fd) {
            Ok(peer) => {
                const ZERO: IpAddr = IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0));
                if self.ip() == ZERO {
                    // https://www.rfc-editor.org/rfc/rfc1122.html#section-3.2.1.3
                    // 0.0.0.0 should only be used as source IP not destination
                    // However in some systems this destination IP is mapped to 127.0.0.1.
                    // We just skip this check here to avoid false positive mismatch.
                    return true;
                }
                let addr = SockaddrStorage::from(*self);
                if addr == peer {
                    debug!("Inet FD to: {addr} is reusable");
                    true
                } else {
                    error!("Crit: FD mismatch: fd: {fd:?}, addr: {addr}, peer: {peer}",);
                    false
                }
            }
            Err(e) => {
                debug!("Idle connection is broken: {e:?}");
                false
            }
        }
    }
}

#[cfg(windows)]
impl ConnSockReusable for InetSocketAddr {
    fn check_sock_match<V: AsRawSocket>(&self, sock: V) -> bool {
        let sock = sock.as_raw_socket();
        match windows::peer_addr(sock) {
            Ok(peer) => {
                const ZERO: IpAddr = IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0));
                if self.ip() == ZERO {
                    // https://www.rfc-editor.org/rfc/rfc1122.html#section-3.2.1.3
                    // 0.0.0.0 should only be used as source IP not destination
                    // However in some systems this destination IP is mapped to 127.0.0.1.
                    // We just skip this check here to avoid false positive mismatch.
                    return true;
                }
                if self == &peer {
                    debug!("Inet FD to: {self} is reusable");
                    true
                } else {
                    error!("Crit: FD mismatch: fd: {sock:?}, addr: {self}, peer: {peer}",);
                    false
                }
            }
            Err(e) => {
                debug!("Idle connection is broken: {e:?}");
                false
            }
        }
    }
}
