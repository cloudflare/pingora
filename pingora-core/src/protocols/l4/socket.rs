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
use log::warn;
#[cfg(unix)]
use nix::sys::socket::{getpeername, getsockname, SockaddrStorage};
use std::cmp::Ordering;
use std::hash::{Hash, Hasher};
use std::net::SocketAddr as StdSockAddr;
#[cfg(unix)]
use std::os::unix::net::SocketAddr as StdUnixSockAddr;
#[cfg(unix)]
use tokio::net::unix::SocketAddr as TokioUnixSockAddr;

/// [`SocketAddr`] is a storage type that contains either a Internet (IP address)
/// socket address or a Unix domain socket address.
#[derive(Debug, Clone)]
pub enum SocketAddr {
    Inet(StdSockAddr),
    #[cfg(unix)]
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
    #[cfg(unix)]
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

    #[cfg(unix)]
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

    #[cfg(unix)]
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

    #[cfg(windows)]
    pub fn from_raw_socket(
        sock: std::os::windows::io::RawSocket,
        is_peer_addr: bool,
    ) -> Option<SocketAddr> {
        use crate::protocols::windows::{local_addr, peer_addr};
        if is_peer_addr {
            peer_addr(sock)
        } else {
            local_addr(sock)
        }
        .map(|s| s.into())
        .ok()
    }
}

impl std::fmt::Display for SocketAddr {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            SocketAddr::Inet(addr) => write!(f, "{addr}"),
            #[cfg(unix)]
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
            #[cfg(unix)]
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
            #[cfg(unix)]
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
            #[cfg(unix)]
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
    #[cfg(unix)]
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.starts_with("unix:") {
            // format unix:/tmp/server.socket
            let path = s.trim_start_matches("unix:");
            let uds_socket = StdUnixSockAddr::from_pathname(path)
                .or_err(crate::BindError, "invalid UDS path")?;
            Ok(SocketAddr::Unix(uds_socket))
        } else {
            match StdSockAddr::from_str(s) {
                Ok(addr) => Ok(SocketAddr::Inet(addr)),
                Err(_) => {
                    // Try to parse as UDS for backward compatibility
                    let uds_socket = StdUnixSockAddr::from_pathname(s)
                        .or_err(crate::BindError, "invalid UDS path")?;
                    warn!("Raw Unix domain socket path support will be deprecated, add 'unix:' prefix instead");
                    Ok(SocketAddr::Unix(uds_socket))
                }
            }
        }
    }
    #[cfg(windows)]
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let addr = StdSockAddr::from_str(s).or_err(crate::BindError, "invalid socket addr")?;
        Ok(SocketAddr::Inet(addr))
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

#[cfg(unix)]
impl From<StdUnixSockAddr> for SocketAddr {
    fn from(sockaddr: StdUnixSockAddr) -> Self {
        SocketAddr::Unix(sockaddr)
    }
}

// TODO: ideally mio/tokio will start using the std version of the unix `SocketAddr`
// so we can avoid a fallible conversion
// https://github.com/tokio-rs/mio/issues/1527
#[cfg(unix)]
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

    #[cfg(unix)]
    #[test]
    fn parse_uds() {
        let uds: SocketAddr = "/tmp/my.sock".parse().unwrap();
        assert!(uds.as_unix().is_some());
    }

    #[cfg(unix)]
    #[test]
    fn parse_uds_with_prefix() {
        let uds: SocketAddr = "unix:/tmp/my.sock".parse().unwrap();
        assert!(uds.as_unix().is_some());
    }
}
