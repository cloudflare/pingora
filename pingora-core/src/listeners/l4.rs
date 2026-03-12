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

#[cfg(feature = "connection_filter")]
use log::debug;
use log::warn;
use pingora_error::{
    ErrorType::{AcceptError, BindError},
    OrErr, Result,
};
use std::io::ErrorKind;
use std::net::{SocketAddr, ToSocketAddrs};
#[cfg(unix)]
use std::os::unix::io::{AsRawFd, FromRawFd};
#[cfg(unix)]
use std::os::unix::net::UnixListener as StdUnixListener;
#[cfg(windows)]
use std::os::windows::io::{AsRawSocket, FromRawSocket};
use std::time::Duration;
use std::{fs::Permissions, sync::Arc};
use tokio::net::TcpSocket;

#[cfg(feature = "connection_filter")]
use super::connection_filter::ConnectionFilter;
#[cfg(feature = "connection_filter")]
use crate::listeners::AcceptAllFilter;

use crate::protocols::l4::ext::{set_dscp, set_tcp_fastopen_backlog};
use crate::protocols::l4::listener::Listener;
pub use crate::protocols::l4::stream::Stream;
#[cfg(feature = "connection_filter")]
use crate::protocols::GetSocketDigest;
use crate::protocols::TcpKeepalive;
#[cfg(unix)]
use crate::server::ListenFds;

const TCP_LISTENER_MAX_TRY: usize = 30;
const TCP_LISTENER_TRY_STEP: Duration = Duration::from_secs(1);
// TODO: configurable backlog
const LISTENER_BACKLOG: u32 = 65535;

/// Address for listening server, either TCP/UDS socket.
#[derive(Clone, Debug)]
pub enum ServerAddress {
    Tcp(String, Option<TcpSocketOptions>),
    #[cfg(unix)]
    Uds(String, Option<Permissions>),
}

impl AsRef<str> for ServerAddress {
    fn as_ref(&self) -> &str {
        match &self {
            Self::Tcp(l, _) => l,
            #[cfg(unix)]
            Self::Uds(l, _) => l,
        }
    }
}

impl ServerAddress {
    fn tcp_sock_opts(&self) -> Option<&TcpSocketOptions> {
        match &self {
            Self::Tcp(_, op) => op.into(),
            _ => None,
        }
    }
}

/// TCP socket configuration options, this is used for setting options on
/// listening sockets and accepted connections.
#[non_exhaustive]
#[derive(Clone, Debug, Default)]
pub struct TcpSocketOptions {
    /// IPV6_V6ONLY flag (if true, limit socket to IPv6 communication only).
    /// This is mostly useful when binding to `[::]`, which on most Unix distributions
    /// will bind to both IPv4 and IPv6 addresses by default.
    pub ipv6_only: Option<bool>,
    /// Enable TCP fast open and set the backlog size of it.
    /// See the [man page](https://man7.org/linux/man-pages/man7/tcp.7.html) for more information.
    pub tcp_fastopen: Option<usize>,
    /// Enable TCP keepalive on accepted connections.
    /// See the [man page](https://man7.org/linux/man-pages/man7/tcp.7.html) for more information.
    pub tcp_keepalive: Option<TcpKeepalive>,
    /// Specifies the server should set the following DSCP value on outgoing connections.
    /// See the [RFC](https://datatracker.ietf.org/doc/html/rfc2474) for more details.
    pub dscp: Option<u8>,
    /// Enable SO_REUSEPORT to allow multiple sockets to bind to the same address and port.
    /// This is useful for load balancing across multiple worker processes.
    /// See the [man page](https://man7.org/linux/man-pages/man7/socket.7.html) for more information.
    pub so_reuseport: Option<bool>,
    // TODO: allow configuring reuseaddr, backlog, etc. from here?
}

#[cfg(unix)]
mod uds {
    use super::{OrErr, Result};
    use crate::protocols::l4::listener::Listener;
    use log::{debug, error};
    use pingora_error::ErrorType::BindError;
    use std::fs::{self, Permissions};
    use std::io::ErrorKind;
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::UnixListener as StdUnixListener;
    use tokio::net::UnixListener;

    use super::LISTENER_BACKLOG;

    pub(super) fn set_perms(path: &str, perms: Option<Permissions>) -> Result<()> {
        // set read/write permissions for all users on the socket by default
        let perms = perms.unwrap_or(Permissions::from_mode(0o666));
        fs::set_permissions(path, perms).or_err_with(BindError, || {
            format!("Fail to bind to {path}, could not set permissions")
        })
    }

    pub(super) fn set_backlog(l: StdUnixListener, backlog: u32) -> Result<UnixListener> {
        let socket: socket2::Socket = l.into();
        // Note that we call listen on an already listening socket
        // POSIX undefined but on Linux it will update the backlog size
        socket
            .listen(backlog as i32)
            .or_err_with(BindError, || format!("listen() failed on {socket:?}"))?;
        UnixListener::from_std(socket.into()).or_err(BindError, "Failed to convert to tokio socket")
    }

    pub(super) fn bind(addr: &str, perms: Option<Permissions>) -> Result<Listener> {
        /*
          We remove the filename/address in case there is a dangling reference.

          "Binding to a socket with a filename creates a socket in the
          filesystem that must be deleted by the caller when it is no
          longer needed (using unlink(2))"
        */
        match std::fs::remove_file(addr) {
            Ok(()) => {
                debug!("unlink {addr} done");
            }
            Err(e) => match e.kind() {
                ErrorKind::NotFound => debug!("unlink {addr} not found: {e}"),
                _ => error!("unlink {addr} failed: {e}"),
            },
        }
        let listener_socket = UnixListener::bind(addr)
            .or_err_with(BindError, || format!("Bind() failed on {addr}"))?;
        set_perms(addr, perms)?;
        let std_listener = listener_socket.into_std().unwrap();
        Ok(set_backlog(std_listener, LISTENER_BACKLOG)?.into())
    }
}

// currently, these options can only apply on sockets prior to calling bind()
fn apply_tcp_socket_options(sock: &TcpSocket, opt: Option<&TcpSocketOptions>) -> Result<()> {
    let Some(opt) = opt else {
        return Ok(());
    };

    let socket_ref = socket2::SockRef::from(sock);

    if let Some(ipv6_only) = opt.ipv6_only {
        socket_ref
            .set_only_v6(ipv6_only)
            .or_err(BindError, "failed to set IPV6_V6ONLY")?;
    }

    #[cfg(unix)]
    if let Some(reuseport) = opt.so_reuseport {
        socket_ref
            .set_reuse_port(reuseport)
            .or_err(BindError, "failed to set SO_REUSEPORT")?;
    }

    #[cfg(unix)]
    let raw = sock.as_raw_fd();
    #[cfg(windows)]
    let raw = sock.as_raw_socket();

    if let Some(backlog) = opt.tcp_fastopen {
        set_tcp_fastopen_backlog(raw, backlog)?;
    }

    if let Some(dscp) = opt.dscp {
        set_dscp(raw, dscp)?;
    }
    Ok(())
}

fn from_raw_fd(address: &ServerAddress, fd: i32) -> Result<Listener> {
    match address {
        #[cfg(unix)]
        ServerAddress::Uds(addr, perm) => {
            let std_listener = unsafe { StdUnixListener::from_raw_fd(fd) };
            // set permissions just in case
            uds::set_perms(addr, perm.clone())?;
            Ok(uds::set_backlog(std_listener, LISTENER_BACKLOG)?.into())
        }
        ServerAddress::Tcp(_, _) => {
            #[cfg(unix)]
            let std_listener_socket = unsafe { std::net::TcpStream::from_raw_fd(fd) };
            #[cfg(windows)]
            let std_listener_socket = unsafe { std::net::TcpStream::from_raw_socket(fd as u64) };
            let listener_socket = TcpSocket::from_std_stream(std_listener_socket);
            // Note that we call listen on an already listening socket
            // POSIX undefined but on Linux it will update the backlog size
            Ok(listener_socket
                .listen(LISTENER_BACKLOG)
                .or_err_with(BindError, || format!("Listen() failed on {address:?}"))?
                .into())
        }
    }
}

async fn bind_tcp(addr: &str, opt: Option<TcpSocketOptions>) -> Result<Listener> {
    let mut try_count = 0;
    loop {
        let sock_addr = addr
            .to_socket_addrs() // NOTE: this could invoke a blocking network lookup
            .or_err_with(BindError, || format!("Invalid listen address {addr}"))?
            .next() // take the first one for now
            .unwrap(); // assume there is always at least one

        let listener_socket = match sock_addr {
            SocketAddr::V4(_) => TcpSocket::new_v4(),
            SocketAddr::V6(_) => TcpSocket::new_v6(),
        }
        .or_err_with(BindError, || format!("fail to create address {sock_addr}"))?;

        // NOTE: this is to preserve the current TcpListener::bind() behavior.
        // We have a few tests relying on this behavior to allow multiple identical
        // test servers to coexist.
        listener_socket
            .set_reuseaddr(true)
            .or_err(BindError, "fail to set_reuseaddr(true)")?;

        apply_tcp_socket_options(&listener_socket, opt.as_ref())?;

        match listener_socket.bind(sock_addr) {
            Ok(()) => {
                break Ok(listener_socket
                    .listen(LISTENER_BACKLOG)
                    .or_err(BindError, "bind() failed")?
                    .into())
            }
            Err(e) => {
                if e.kind() != ErrorKind::AddrInUse {
                    break Err(e).or_err_with(BindError, || format!("bind() failed on {addr}"));
                }
                try_count += 1;
                if try_count >= TCP_LISTENER_MAX_TRY {
                    break Err(e).or_err_with(BindError, || {
                        format!("bind() failed, after retries, {addr} still in use")
                    });
                }
                warn!("{addr} is in use, will try again");
                tokio::time::sleep(TCP_LISTENER_TRY_STEP).await;
            }
        }
    }
}

async fn bind(addr: &ServerAddress) -> Result<Listener> {
    match addr {
        #[cfg(unix)]
        ServerAddress::Uds(l, perm) => uds::bind(l, perm.clone()),
        ServerAddress::Tcp(l, opt) => bind_tcp(l, opt.clone()).await,
    }
}

#[derive(Clone, Debug)]
pub struct ListenerEndpoint {
    listen_addr: ServerAddress,
    listener: Arc<Listener>,
    #[cfg(feature = "connection_filter")]
    connection_filter: Arc<dyn ConnectionFilter>,
}

#[derive(Default)]
pub struct ListenerEndpointBuilder {
    listen_addr: Option<ServerAddress>,
    #[cfg(feature = "connection_filter")]
    connection_filter: Option<Arc<dyn ConnectionFilter>>,
}

impl ListenerEndpointBuilder {
    pub fn new() -> ListenerEndpointBuilder {
        Self {
            listen_addr: None,
            #[cfg(feature = "connection_filter")]
            connection_filter: None,
        }
    }

    pub fn listen_addr(&mut self, addr: ServerAddress) -> &mut Self {
        self.listen_addr = Some(addr);
        self
    }

    #[cfg(feature = "connection_filter")]
    pub fn connection_filter(&mut self, filter: Arc<dyn ConnectionFilter>) -> &mut Self {
        self.connection_filter = Some(filter);
        self
    }

    #[cfg(unix)]
    pub async fn listen(self, fds: Option<ListenFds>) -> Result<ListenerEndpoint> {
        let listen_addr = self
            .listen_addr
            .expect("Tried to listen with no addr specified");

        let listener = if let Some(fds_table) = fds {
            let addr_str = listen_addr.as_ref();

            // consider make this mutex std::sync::Mutex or OnceCell
            let mut table = fds_table.lock().await;

            if let Some(fd) = table.get(addr_str) {
                from_raw_fd(&listen_addr, *fd)?
            } else {
                // not found
                let listener = bind(&listen_addr).await?;
                table.add(addr_str.to_string(), listener.as_raw_fd());
                listener
            }
        } else {
            // not found, no fd table
            bind(&listen_addr).await?
        };

        #[cfg(feature = "connection_filter")]
        let connection_filter = self
            .connection_filter
            .unwrap_or_else(|| Arc::new(AcceptAllFilter));

        Ok(ListenerEndpoint {
            listen_addr,
            listener: Arc::new(listener),
            #[cfg(feature = "connection_filter")]
            connection_filter,
        })
    }

    #[cfg(windows)]
    pub async fn listen(self) -> Result<ListenerEndpoint> {
        let listen_addr = self
            .listen_addr
            .expect("Tried to listen with no addr specified");

        let listener = bind(&listen_addr).await?;

        #[cfg(feature = "connection_filter")]
        let connection_filter = self
            .connection_filter
            .unwrap_or_else(|| Arc::new(AcceptAllFilter));

        Ok(ListenerEndpoint {
            listen_addr,
            listener: Arc::new(listener),
            #[cfg(feature = "connection_filter")]
            connection_filter,
        })
    }
}

impl ListenerEndpoint {
    pub fn builder() -> ListenerEndpointBuilder {
        ListenerEndpointBuilder::new()
    }

    pub fn as_str(&self) -> &str {
        self.listen_addr.as_ref()
    }

    fn apply_stream_settings(&self, stream: &mut Stream) -> Result<()> {
        // settings are applied based on whether the underlying stream supports it
        stream.set_nodelay()?;
        let Some(op) = self.listen_addr.tcp_sock_opts() else {
            return Ok(());
        };
        if let Some(ka) = op.tcp_keepalive.as_ref() {
            stream.set_keepalive(ka)?;
        }
        if let Some(dscp) = op.dscp {
            #[cfg(unix)]
            set_dscp(stream.as_raw_fd(), dscp)?;
            #[cfg(windows)]
            set_dscp(stream.as_raw_socket(), dscp)?;
        }
        Ok(())
    }

    pub async fn accept(&self) -> Result<Stream> {
        #[cfg(feature = "connection_filter")]
        {
            loop {
                let mut stream = self
                    .listener
                    .accept()
                    .await
                    .or_err(AcceptError, "Fail to accept()")?;

                // Performance: nested if-let avoids cloning/allocations on each connection accept
                let should_accept = if let Some(digest) = stream.get_socket_digest() {
                    if let Some(peer_addr) = digest.peer_addr() {
                        self.connection_filter
                            .should_accept(peer_addr.as_inet())
                            .await
                    } else {
                        // No peer address available - accept by default
                        true
                    }
                } else {
                    // No socket digest available - accept by default
                    true
                };

                if !should_accept {
                    debug!("Connection rejected by filter");
                    drop(stream);
                    continue;
                }

                self.apply_stream_settings(&mut stream)?;
                return Ok(stream);
            }
        }
        #[cfg(not(feature = "connection_filter"))]
        {
            let mut stream = self
                .listener
                .accept()
                .await
                .or_err(AcceptError, "Fail to accept()")?;
            self.apply_stream_settings(&mut stream)?;
            Ok(stream)
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[tokio::test]
    async fn test_listen_tcp() {
        let addr = "127.0.0.1:7100";

        let mut builder = ListenerEndpoint::builder();

        builder.listen_addr(ServerAddress::Tcp(addr.into(), None));

        #[cfg(unix)]
        let listener = builder.listen(None).await.unwrap();

        #[cfg(windows)]
        let listener = builder.listen().await.unwrap();

        tokio::spawn(async move {
            // just try to accept once
            listener.accept().await.unwrap();
        });
        tokio::net::TcpStream::connect(addr)
            .await
            .expect("can connect to TCP listener");
    }

    #[tokio::test]
    async fn test_listen_tcp_ipv6_only() {
        let sock_opt = Some(TcpSocketOptions {
            ipv6_only: Some(true),
            ..Default::default()
        });

        let mut builder = ListenerEndpoint::builder();

        builder.listen_addr(ServerAddress::Tcp("[::]:7101".into(), sock_opt));

        #[cfg(unix)]
        let listener = builder.listen(None).await.unwrap();

        #[cfg(windows)]
        let listener = builder.listen().await.unwrap();

        tokio::spawn(async move {
            // just try to accept twice
            listener.accept().await.unwrap();
            listener.accept().await.unwrap();
        });
        tokio::net::TcpStream::connect("127.0.0.1:7101")
            .await
            .expect_err("cannot connect to v4 addr");
        tokio::net::TcpStream::connect("[::1]:7101")
            .await
            .expect("can connect to v6 addr");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_listen_uds() {
        let addr = "/tmp/test_listen_uds";

        let mut builder = ListenerEndpoint::builder();

        builder.listen_addr(ServerAddress::Uds(addr.into(), None));

        let listener = builder.listen(None).await.unwrap();

        tokio::spawn(async move {
            // just try to accept once
            listener.accept().await.unwrap();
        });
        tokio::net::UnixStream::connect(addr)
            .await
            .expect("can connect to UDS listener");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_tcp_so_reuseport() {
        let addr = "127.0.0.1:7201";
        let sock_opt = TcpSocketOptions {
            so_reuseport: Some(true),
            ..Default::default()
        };

        // Create first listener with SO_REUSEPORT
        let mut builder1 = ListenerEndpoint::builder();
        builder1.listen_addr(ServerAddress::Tcp(addr.into(), Some(sock_opt.clone())));
        let listener1 = builder1.listen(None).await.unwrap();

        // Create second listener with the same address and SO_REUSEPORT
        // This should succeed because SO_REUSEPORT is enabled
        let mut builder2 = ListenerEndpoint::builder();
        builder2.listen_addr(ServerAddress::Tcp(addr.into(), Some(sock_opt)));
        let listener2 = builder2.listen(None).await.unwrap();

        // Both listeners should be able to bind to the same address
        assert_eq!(listener1.as_str(), addr);
        assert_eq!(listener2.as_str(), addr);
    }

    #[tokio::test]
    async fn test_tcp_so_reuseport_false() {
        let addr = "127.0.0.1:7202";
        let sock_opt_no_reuseport = TcpSocketOptions {
            so_reuseport: Some(false), // Explicitly disable SO_REUSEPORT
            ..Default::default()
        };

        // Create first listener without SO_REUSEPORT
        let mut builder1 = ListenerEndpoint::builder();
        builder1.listen_addr(ServerAddress::Tcp(
            addr.into(),
            Some(sock_opt_no_reuseport.clone()),
        ));
        let listener1 = builder1.listen(None).await.unwrap();

        // Try to create second listener with the same address and no SO_REUSEPORT
        // This should fail with "address already in use"
        let mut builder2 = ListenerEndpoint::builder();
        builder2.listen_addr(ServerAddress::Tcp(addr.into(), Some(sock_opt_no_reuseport)));
        let result = builder2.listen(None).await;

        // The second bind should fail
        assert!(result.is_err());
        let error_msg = format!("{:?}", result.unwrap_err());
        assert!(
            error_msg.contains("address")
                || error_msg.contains("in use")
                || error_msg.contains("bind")
        );

        // Verify the first listener still works
        assert_eq!(listener1.as_str(), addr);
    }

    #[cfg(feature = "connection_filter")]
    #[tokio::test]
    async fn test_connection_filter_accept() {
        use crate::listeners::ConnectionFilter;
        use async_trait::async_trait;
        use std::sync::atomic::{AtomicUsize, Ordering};

        #[derive(Debug)]
        struct CountingFilter {
            accept_count: Arc<AtomicUsize>,
            reject_count: Arc<AtomicUsize>,
        }

        #[async_trait]
        impl ConnectionFilter for CountingFilter {
            async fn should_accept(&self, _addr: Option<&SocketAddr>) -> bool {
                let count = self.accept_count.fetch_add(1, Ordering::SeqCst);
                if count % 2 == 0 {
                    true
                } else {
                    self.reject_count.fetch_add(1, Ordering::SeqCst);
                    false
                }
            }
        }

        let addr = "127.0.0.1:7300";
        let accept_count = Arc::new(AtomicUsize::new(0));
        let reject_count = Arc::new(AtomicUsize::new(0));

        let filter = Arc::new(CountingFilter {
            accept_count: accept_count.clone(),
            reject_count: reject_count.clone(),
        });

        let mut builder = ListenerEndpoint::builder();
        builder
            .listen_addr(ServerAddress::Tcp(addr.into(), None))
            .connection_filter(filter);

        #[cfg(unix)]
        let listener = builder.listen(None).await.unwrap();
        #[cfg(windows)]
        let listener = builder.listen().await.unwrap();

        let listener_clone = listener.clone();
        tokio::spawn(async move {
            let _stream1 = listener_clone.accept().await.unwrap();
            let _stream2 = listener_clone.accept().await.unwrap();
        });

        tokio::time::sleep(Duration::from_millis(10)).await;

        let _conn1 = tokio::net::TcpStream::connect(addr).await.unwrap();
        let _conn2 = tokio::net::TcpStream::connect(addr).await.unwrap();
        let _conn3 = tokio::net::TcpStream::connect(addr).await.unwrap();

        tokio::time::sleep(Duration::from_millis(50)).await;

        assert_eq!(accept_count.load(Ordering::SeqCst), 3);
        assert_eq!(reject_count.load(Ordering::SeqCst), 1);
    }

    #[cfg(feature = "connection_filter")]
    #[tokio::test]
    async fn test_connection_filter_blocks_all() {
        use crate::listeners::ConnectionFilter;
        use async_trait::async_trait;
        use std::sync::atomic::{AtomicUsize, Ordering};

        #[derive(Debug)]
        struct RejectAllFilter {
            reject_count: Arc<AtomicUsize>,
        }

        #[async_trait]
        impl ConnectionFilter for RejectAllFilter {
            async fn should_accept(&self, _addr: Option<&SocketAddr>) -> bool {
                self.reject_count.fetch_add(1, Ordering::SeqCst);
                false
            }
        }

        let addr = "127.0.0.1:7301";
        let reject_count = Arc::new(AtomicUsize::new(0));

        let mut builder = ListenerEndpoint::builder();
        builder
            .listen_addr(ServerAddress::Tcp(addr.into(), None))
            .connection_filter(Arc::new(RejectAllFilter {
                reject_count: reject_count.clone(),
            }));

        #[cfg(unix)]
        let listener = builder.listen(None).await.unwrap();
        #[cfg(windows)]
        let listener = builder.listen().await.unwrap();

        let listener_clone = listener.clone();
        let _accept_handle = tokio::spawn(async move {
            // This will never return since all connections are rejected
            let _ = listener_clone.accept().await;
        });

        tokio::time::sleep(Duration::from_millis(50)).await;

        let mut handles = vec![];
        for _ in 0..3 {
            let handle = tokio::spawn(async move {
                if let Ok(stream) = tokio::net::TcpStream::connect(addr).await {
                    drop(stream);
                }
            });
            handles.push(handle);
        }

        for handle in handles {
            let _ = handle.await;
        }

        // Wait for rejections to be counted with timeout
        let start = tokio::time::Instant::now();
        let timeout = Duration::from_secs(2);

        loop {
            let rejected = reject_count.load(Ordering::SeqCst);
            if rejected >= 3 {
                assert_eq!(rejected, 3, "Should reject exactly 3 connections");
                break;
            }

            if start.elapsed() > timeout {
                panic!(
                    "Timeout waiting for rejections, got {} expected 3",
                    rejected
                );
            }

            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}
