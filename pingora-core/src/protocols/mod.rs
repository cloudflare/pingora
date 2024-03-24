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

pub use digest::{
    Digest, GetProxyDigest, GetSocketDigest, GetTimingDigest, ProtoDigest, SocketDigest,
    TimingDigest,
};
pub use ssl::ALPN;

use async_trait::async_trait;
use std::fmt::Debug;
use std::sync::Arc;

/// Define how a protocol should shutdown its connection.
#[async_trait]
pub trait Shutdown {
    async fn shutdown(&mut self) -> ();
}

/// Define how a given session/connection identifies itself.
pub trait UniqueID {
    /// The ID returned should be unique among all existing connections of the same type.
    /// But ID can be recycled after a connection is shutdown.
    fn id(&self) -> i32;
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
        fn id(&self) -> i32 {
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
        fn id(&self) -> i32 {
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
        fn id(&self) -> i32 {
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

pub(crate) trait ConnFdReusable {
    fn check_fd_match<V: AsRawFd>(&self, fd: V) -> bool;
}

use l4::socket::SocketAddr;
use log::{debug, error};
use nix::sys::socket::{getpeername, SockaddrStorage, UnixAddr};
use std::{net::SocketAddr as InetSocketAddr, os::unix::prelude::AsRawFd, path::Path};

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

impl ConnFdReusable for Path {
    fn check_fd_match<V: AsRawFd>(&self, fd: V) -> bool {
        let fd = fd.as_raw_fd();
        match getpeername::<UnixAddr>(fd) {
            Ok(peer) => match UnixAddr::new(self) {
                Ok(addr) => {
                    if addr == peer {
                        debug!("Unix FD to: {peer:?} is reusable");
                        true
                    } else {
                        error!("Crit: unix FD mismatch: fd: {fd:?}, peer: {peer:?}, addr: {addr}",);
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

impl ConnFdReusable for InetSocketAddr {
    fn check_fd_match<V: AsRawFd>(&self, fd: V) -> bool {
        let fd = fd.as_raw_fd();
        match getpeername::<SockaddrStorage>(fd) {
            Ok(peer) => {
                let addr = SockaddrStorage::from(*self);
                if addr == peer {
                    debug!("Inet FD to: {peer:?} is reusable");
                    true
                } else {
                    error!("Crit: FD mismatch: fd: {fd:?}, addr: {addr:?}, peer: {peer:?}",);
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
