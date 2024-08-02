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

//! Extra information about the connection

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use once_cell::sync::OnceCell;

use super::l4::ext::{get_recv_buf, get_tcp_info, TCP_INFO};
use super::l4::socket::SocketAddr;
use super::raw_connect::ProxyDigest;
use super::ssl::digest::SslDigest;

/// The information can be extracted from a connection
#[derive(Clone, Debug, Default)]
pub struct Digest {
    /// Information regarding the TLS of this connection if any
    pub ssl_digest: Option<Arc<SslDigest>>,
    /// Timing information
    pub timing_digest: Vec<Option<TimingDigest>>,
    /// information regarding the CONNECT proxy this connection uses.
    pub proxy_digest: Option<Arc<ProxyDigest>>,
    /// Information about underlying socket/fd of this connection
    pub socket_digest: Option<Arc<SocketDigest>>,
}

/// The interface to return protocol related information
pub trait ProtoDigest {
    fn get_digest(&self) -> Option<&Digest> {
        None
    }
}

/// The timing information of the connection
#[derive(Clone, Debug)]
pub struct TimingDigest {
    /// When this connection was established
    pub established_ts: SystemTime,
}

impl Default for TimingDigest {
    fn default() -> Self {
        TimingDigest {
            established_ts: SystemTime::UNIX_EPOCH,
        }
    }
}

#[derive(Debug)]
/// The interface to return socket-related information
pub struct SocketDigest {
    #[cfg(unix)]
    raw_fd: std::os::unix::io::RawFd,
    #[cfg(windows)]
    raw_sock: std::os::windows::io::RawSocket,
    /// Remote socket address
    pub peer_addr: OnceCell<Option<SocketAddr>>,
    /// Local socket address
    pub local_addr: OnceCell<Option<SocketAddr>>,
}

impl SocketDigest {
    #[cfg(unix)]
    pub fn from_raw_fd(raw_fd: std::os::unix::io::RawFd) -> SocketDigest {
        SocketDigest {
            raw_fd,
            peer_addr: OnceCell::new(),
            local_addr: OnceCell::new(),
        }
    }

    #[cfg(windows)]
    pub fn from_raw_socket(raw_sock: std::os::windows::io::RawSocket) -> SocketDigest {
        SocketDigest {
            raw_sock,
            peer_addr: OnceCell::new(),
            local_addr: OnceCell::new(),
        }
    }
    #[cfg(unix)]
    pub fn peer_addr(&self) -> Option<&SocketAddr> {
        self.peer_addr
            .get_or_init(|| SocketAddr::from_raw_fd(self.raw_fd, true))
            .as_ref()
    }

    #[cfg(windows)]
    pub fn peer_addr(&self) -> Option<&SocketAddr> {
        self.peer_addr
            .get_or_init(|| SocketAddr::from_raw_socket(self.raw_sock, true))
            .as_ref()
    }

    #[cfg(unix)]
    pub fn local_addr(&self) -> Option<&SocketAddr> {
        self.local_addr
            .get_or_init(|| SocketAddr::from_raw_fd(self.raw_fd, false))
            .as_ref()
    }

    #[cfg(windows)]
    pub fn local_addr(&self) -> Option<&SocketAddr> {
        self.local_addr
            .get_or_init(|| SocketAddr::from_raw_socket(self.raw_sock, false))
            .as_ref()
    }

    fn is_inet(&self) -> bool {
        self.local_addr().and_then(|p| p.as_inet()).is_some()
    }

    #[cfg(unix)]
    pub fn tcp_info(&self) -> Option<TCP_INFO> {
        if self.is_inet() {
            get_tcp_info(self.raw_fd).ok()
        } else {
            None
        }
    }

    #[cfg(windows)]
    pub fn tcp_info(&self) -> Option<TCP_INFO> {
        if self.is_inet() {
            get_tcp_info(self.raw_sock).ok()
        } else {
            None
        }
    }

    #[cfg(unix)]
    pub fn get_recv_buf(&self) -> Option<usize> {
        if self.is_inet() {
            get_recv_buf(self.raw_fd).ok()
        } else {
            None
        }
    }

    #[cfg(windows)]
    pub fn get_recv_buf(&self) -> Option<usize> {
        if self.is_inet() {
            get_recv_buf(self.raw_sock).ok()
        } else {
            None
        }
    }
}

/// The interface to return timing information
pub trait GetTimingDigest {
    /// Return the timing for each layer from the lowest layer to upper
    fn get_timing_digest(&self) -> Vec<Option<TimingDigest>>;
    fn get_read_pending_time(&self) -> Duration {
        Duration::ZERO
    }
    fn get_write_pending_time(&self) -> Duration {
        Duration::ZERO
    }
}

/// The interface to set or return proxy information
pub trait GetProxyDigest {
    fn get_proxy_digest(&self) -> Option<Arc<ProxyDigest>>;
    fn set_proxy_digest(&mut self, _digest: ProxyDigest) {}
}

/// The interface to set or return socket information
pub trait GetSocketDigest {
    fn get_socket_digest(&self) -> Option<Arc<SocketDigest>>;
    fn set_socket_digest(&mut self, _socket_digest: SocketDigest) {}
}
