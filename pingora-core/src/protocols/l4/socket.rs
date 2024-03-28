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

//! Generic socket type

use crate::{Error, OrErr};
use nix::sys::socket::{getpeername, getsockname, SockaddrStorage};
use std::cmp::Ordering;
use std::hash::{Hash, Hasher};
use std::net::SocketAddr as StdSockAddr;
use std::os::unix::net::SocketAddr as StdUnixSockAddr;
use tokio::net::unix::SocketAddr as TokioUnixSockAddr;

/// [`SocketAddr`] is a storage type that contains either a Internet (IP address)
/// socket address or a Unix domain socket address.
#[derive(Debug, Clone)]
pub enum SocketAddr {
    Inet(StdSockAddr),
    Unix(StdUnixSockAddr),
}

impl SocketAddr {
    /// Get a reference to the IP socket if it is one
    pub fn as_inet(&self) -> Option<&StdSockAddr> {
        if let SocketAddr::Inet(addr) = self {
            Some(addr)
        } else {
            None
        }
    }

    /// Get a reference to the Unix domain socket if it is one
    pub fn as_unix(&self) -> Option<&StdUnixSockAddr> {
        if let SocketAddr::Unix(addr) = self {
            Some(addr)
        } else {
            None
        }
    }

    /// Set the port if the address is an IP socket.
    pub fn set_port(&mut self, port: u16) {
        if let SocketAddr::Inet(addr) = self {
            addr.set_port(port)
        }
    }

    fn from_sockaddr_storage(sock: &SockaddrStorage) -> Option<SocketAddr> {
        if let Some(v4) = sock.as_sockaddr_in() {
            return Some(SocketAddr::Inet(StdSockAddr::V4(
                std::net::SocketAddrV4::new(v4.ip().into(), v4.port()),
            )));
        } else if let Some(v6) = sock.as_sockaddr_in6() {
            return Some(SocketAddr::Inet(StdSockAddr::V6(
                std::net::SocketAddrV6::new(v6.ip(), v6.port(), v6.flowinfo(), v6.scope_id()),
            )));
        }

        // TODO: don't set abstract / unnamed for now,
        // for parity with how we treat these types in TryFrom<TokioUnixSockAddr>
        Some(SocketAddr::Unix(
            sock.as_unix_addr()
                .map(|addr| addr.path().map(StdUnixSockAddr::from_pathname))??
                .ok()?,
        ))
    }

    pub fn from_raw_fd(fd: std::os::unix::io::RawFd, peer_addr: bool) -> Option<SocketAddr> {
        let sockaddr_storage = if peer_addr {
            getpeername(fd)
        } else {
            getsockname(fd)
        };
        match sockaddr_storage {
            Ok(sockaddr) => Self::from_sockaddr_storage(&sockaddr),
            // could be errors such as EBADF, i.e. fd is no longer a valid socket
            // fail open in this case
            Err(_e) => None,
        }
    }
}

impl std::fmt::Display for SocketAddr {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            SocketAddr::Inet(addr) => write!(f, "{addr}"),
            SocketAddr::Unix(addr) => {
                if let Some(path) = addr.as_pathname() {
                    write!(f, "{}", path.display())
                } else {
                    write!(f, "{addr:?}")
                }
            }
        }
    }
}

impl Hash for SocketAddr {
    fn hash<H: Hasher>(&self, state: &mut H) {
        match self {
            Self::Inet(sockaddr) => sockaddr.hash(state),
            Self::Unix(sockaddr) => {
                if let Some(path) = sockaddr.as_pathname() {
                    // use the underlying path as the hash
                    path.hash(state);
                } else {
                    // unnamed or abstract UDS
                    // abstract UDS name not yet exposed by std API
                    // panic for now, we can decide on the right way to hash them later
                    panic!("Unnamed and abstract UDS types not yet supported for hashing")
                }
            }
        }
    }
}

impl PartialEq for SocketAddr {
    fn eq(&self, other: &Self) -> bool {
        match self {
            Self::Inet(addr) => Some(addr) == other.as_inet(),
            Self::Unix(addr) => {
                let path = addr.as_pathname();
                // can only compare UDS with path, assume false on all unnamed UDS
                path.is_some() && path == other.as_unix().and_then(|addr| addr.as_pathname())
            }
        }
    }
}

impl PartialOrd for SocketAddr {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for SocketAddr {
    fn cmp(&self, other: &Self) -> Ordering {
        match self {
            Self::Inet(addr) => {
                if let Some(o) = other.as_inet() {
                    addr.cmp(o)
                } else {
                    // always make Inet < Unix "smallest for variants at the top"
                    Ordering::Less
                }
            }
            Self::Unix(addr) => {
                if let Some(o) = other.as_unix() {
                    // NOTE: unnamed UDS are consider the same
                    addr.as_pathname().cmp(&o.as_pathname())
                } else {
                    // always make Inet < Unix "smallest for variants at the top"
                    Ordering::Greater
                }
            }
        }
    }
}

impl Eq for SocketAddr {}

impl std::str::FromStr for SocketAddr {
    type Err = Box<Error>;

    // This is very basic parsing logic, it might treat invalid IP:PORT str as UDS path
    // TODO: require UDS to have some prefix
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match StdSockAddr::from_str(s) {
            Ok(addr) => Ok(SocketAddr::Inet(addr)),
            Err(_) => {
                let uds_socket = StdUnixSockAddr::from_pathname(s)
                    .or_err(crate::BindError, "invalid UDS path")?;
                Ok(SocketAddr::Unix(uds_socket))
            }
        }
    }
}

impl std::net::ToSocketAddrs for SocketAddr {
    type Iter = std::iter::Once<StdSockAddr>;

    // Error if UDS addr
    fn to_socket_addrs(&self) -> std::io::Result<Self::Iter> {
        if let Some(inet) = self.as_inet() {
            Ok(std::iter::once(*inet))
        } else {
            Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                "UDS socket cannot be used as inet socket",
            ))
        }
    }
}

impl From<StdSockAddr> for SocketAddr {
    fn from(sockaddr: StdSockAddr) -> Self {
        SocketAddr::Inet(sockaddr)
    }
}

impl From<StdUnixSockAddr> for SocketAddr {
    fn from(sockaddr: StdUnixSockAddr) -> Self {
        SocketAddr::Unix(sockaddr)
    }
}

// TODO: ideally mio/tokio will start using the std version of the unix `SocketAddr`
// so we can avoid a fallible conversion
// https://github.com/tokio-rs/mio/issues/1527
impl TryFrom<TokioUnixSockAddr> for SocketAddr {
    type Error = String;

    fn try_from(value: TokioUnixSockAddr) -> Result<Self, Self::Error> {
        if let Some(Ok(addr)) = value.as_pathname().map(StdUnixSockAddr::from_pathname) {
            Ok(addr.into())
        } else {
            // may be unnamed/abstract UDS
            Err(format!("could not convert {value:?} to SocketAddr"))
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn parse_ip() {
        let ip: SocketAddr = "127.0.0.1:80".parse().unwrap();
        assert!(ip.as_inet().is_some());
    }

    #[test]
    fn parse_uds() {
        let uds: SocketAddr = "/tmp/my.sock".parse().unwrap();
        assert!(uds.as_unix().is_some());
    }
}
