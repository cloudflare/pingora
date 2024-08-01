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

//! Listeners

use crate::protocols::digest::{GetSocketDigest, SocketDigest};
use crate::protocols::l4::stream::Stream;
use std::io;
#[cfg(unix)]
use std::os::unix::io::AsRawFd;
#[cfg(windows)]
use std::os::windows::io::AsRawSocket;
use tokio::net::TcpListener;
#[cfg(unix)]
use tokio::net::UnixListener;

/// The type for generic listener for both TCP and Unix domain socket
#[derive(Debug)]
pub enum Listener {
    Tcp(TcpListener),
    #[cfg(unix)]
    Unix(UnixListener),
}

impl From<TcpListener> for Listener {
    fn from(s: TcpListener) -> Self {
        Self::Tcp(s)
    }
}

#[cfg(unix)]
impl From<UnixListener> for Listener {
    fn from(s: UnixListener) -> Self {
        Self::Unix(s)
    }
}

#[cfg(unix)]
impl AsRawFd for Listener {
    fn as_raw_fd(&self) -> std::os::unix::io::RawFd {
        match &self {
            Self::Tcp(l) => l.as_raw_fd(),
            Self::Unix(l) => l.as_raw_fd(),
        }
    }
}

#[cfg(windows)]
impl AsRawSocket for Listener {
    fn as_raw_socket(&self) -> std::os::windows::io::RawSocket {
        match &self {
            Self::Tcp(l) => l.as_raw_socket(),
        }
    }
}

impl Listener {
    /// Accept a connection from the listening endpoint
    pub async fn accept(&self) -> io::Result<Stream> {
        match &self {
            Self::Tcp(l) => l.accept().await.map(|(stream, peer_addr)| {
                let mut s: Stream = stream.into();
                #[cfg(unix)]
                let digest = SocketDigest::from_raw_fd(s.as_raw_fd());
                #[cfg(windows)]
                let digest = SocketDigest::from_raw_socket(s.as_raw_socket());
                digest
                    .peer_addr
                    .set(Some(peer_addr.into()))
                    .expect("newly created OnceCell must be empty");
                s.set_socket_digest(digest);
                // TODO: if listening on a specific bind address, we could save
                // an extra syscall looking up the local_addr later if we can pass
                // and init it in the socket digest here
                s
            }),
            #[cfg(unix)]
            Self::Unix(l) => l.accept().await.map(|(stream, peer_addr)| {
                let mut s: Stream = stream.into();
                let digest = SocketDigest::from_raw_fd(s.as_raw_fd());
                // note: if unnamed/abstract UDS, it will be `None`
                // (see TryFrom<tokio::net::unix::SocketAddr>)
                let addr = peer_addr.try_into().ok();
                digest
                    .peer_addr
                    .set(addr)
                    .expect("newly created OnceCell must be empty");
                s.set_socket_digest(digest);
                s
            }),
        }
    }
}
