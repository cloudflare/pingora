// Copyright 2025 Cloudflare, Inc.
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

#[cfg(unix)]
use crate::protocols::l4::ext::connect_uds;
use crate::protocols::l4::ext::{
    connect_with as tcp_connect, set_dscp, set_recv_buf, set_tcp_fastopen_connect,
};
use crate::protocols::l4::socket::SocketAddr;
use crate::protocols::l4::stream::Stream;
use crate::protocols::{GetSocketDigest, SocketDigest};
use crate::upstreams::peer::Peer;
use async_trait::async_trait;
use log::debug;
use pingora_error::{Context, Error, ErrorType::*, OrErr, Result};
use rand::seq::SliceRandom;
use std::net::SocketAddr as InetSocketAddr;
#[cfg(unix)]
use std::os::unix::io::AsRawFd;
#[cfg(windows)]
use std::os::windows::io::AsRawSocket;

/// The interface to establish a L4 connection
#[async_trait]
pub trait Connect: std::fmt::Debug {
    async fn connect(&self, addr: &SocketAddr) -> Result<Stream>;
}

/// Settings for binding on connect
#[derive(Clone, Debug, Default)]
pub struct BindTo {
    // local ip address
    pub addr: Option<InetSocketAddr>,
    // port range
    port_range: Option<(u16, u16)>,
    // whether we fallback and try again on bind errors when a port range is set
    fallback: bool,
}

impl BindTo {
    /// Sets the port range we will bind to where the first item in the tuple is the lower bound
    /// and the second item is the upper bound.
    ///
    /// Note this bind option is only supported on Linux since 6.3, this is a no-op on other systems.
    /// To reset the range, pass a `None` or `Some((0,0))`, more information can be found [here](https://man7.org/linux/man-pages/man7/ip.7.html)
    pub fn set_port_range(&mut self, range: Option<(u16, u16)>) -> Result<()> {
        if range.is_none() && self.port_range.is_none() {
            // nothing to do
            return Ok(());
        }

        match range {
            // 0,0 is valid for resets
            None | Some((0, 0)) => self.port_range = Some((0, 0)),
            // set the port range if valid
            Some((low, high)) if low > 0 && low < high => {
                self.port_range = Some((low, high));
            }
            _ => return Error::e_explain(SocketError, "invalid port range: {range}"),
        }
        Ok(())
    }

    /// Set whether we fallback on no address available if a port range is set
    pub fn set_fallback(&mut self, fallback: bool) {
        self.fallback = fallback
    }

    /// Configured bind port range
    pub fn port_range(&self) -> Option<(u16, u16)> {
        self.port_range
    }

    /// Whether we attempt to fallback on no address available
    pub fn will_fallback(&self) -> bool {
        self.fallback && self.port_range.is_some()
    }
}

/// Establish a connection (l4) to the given peer using its settings and an optional bind address.
pub(crate) async fn connect<P>(peer: &P, bind_to: Option<BindTo>) -> Result<Stream>
where
    P: Peer + Send + Sync,
{
    if peer.get_proxy().is_some() {
        return proxy_connect(peer)
            .await
            .err_context(|| format!("Fail to establish CONNECT proxy: {}", peer));
    }
    let peer_addr = peer.address();
    let mut stream: Stream =
        if let Some(custom_l4) = peer.get_peer_options().and_then(|o| o.custom_l4.as_ref()) {
            custom_l4.connect(peer_addr).await?
        } else {
            match peer_addr {
                SocketAddr::Inet(addr) => {
                    let connect_future = tcp_connect(addr, bind_to.as_ref(), |socket| {
                        #[cfg(unix)]
                        let raw = socket.as_raw_fd();
                        #[cfg(windows)]
                        let raw = socket.as_raw_socket();

                        if peer.tcp_fast_open() {
                            set_tcp_fastopen_connect(raw)?;
                        }
                        if let Some(recv_buf) = peer.tcp_recv_buf() {
                            debug!("Setting recv buf size");
                            set_recv_buf(raw, recv_buf)?;
                        }
                        if let Some(dscp) = peer.dscp() {
                            debug!("Setting dscp");
                            set_dscp(raw, dscp)?;
                        }

                        if let Some(tweak_hook) = peer
                            .get_peer_options()
                            .and_then(|o| o.upstream_tcp_sock_tweak_hook.clone())
                        {
                            tweak_hook(socket)?;
                        }

                        Ok(())
                    });
                    let conn_res = match peer.connection_timeout() {
                        Some(t) => pingora_timeout::timeout(t, connect_future)
                            .await
                            .explain_err(ConnectTimedout, |_| {
                                format!("timeout {t:?} connecting to server {peer}")
                            })?,
                        None => connect_future.await,
                    };
                    match conn_res {
                        Ok(socket) => {
                            debug!("connected to new server: {}", peer.address());
                            Ok(socket.into())
                        }
                        Err(e) => {
                            let c = format!("Fail to connect to {peer}");
                            match e.etype() {
                                SocketError | BindError => Error::e_because(InternalError, c, e),
                                _ => Err(e.more_context(c)),
                            }
                        }
                    }
                }
                #[cfg(unix)]
                SocketAddr::Unix(addr) => {
                    let connect_future = connect_uds(
                        addr.as_pathname()
                            .expect("non-pathname unix sockets not supported as peer"),
                    );
                    let conn_res = match peer.connection_timeout() {
                        Some(t) => pingora_timeout::timeout(t, connect_future)
                            .await
                            .explain_err(ConnectTimedout, |_| {
                                format!("timeout {t:?} connecting to server {peer}")
                            })?,
                        None => connect_future.await,
                    };
                    match conn_res {
                        Ok(socket) => {
                            debug!("connected to new server: {}", peer.address());
                            Ok(socket.into())
                        }
                        Err(e) => {
                            let c = format!("Fail to connect to {peer}");
                            match e.etype() {
                                SocketError | BindError => Error::e_because(InternalError, c, e),
                                _ => Err(e.more_context(c)),
                            }
                        }
                    }
                }
            }?
        };

    let tracer = peer.get_tracer();
    if let Some(t) = tracer {
        t.0.on_connected();
        stream.tracer = Some(t);
    }

    // settings applied based on stream type
    if let Some(ka) = peer.tcp_keepalive() {
        stream.set_keepalive(ka)?;
    }
    stream.set_nodelay()?;

    #[cfg(unix)]
    let digest = SocketDigest::from_raw_fd(stream.as_raw_fd());
    #[cfg(windows)]
    let digest = SocketDigest::from_raw_socket(stream.as_raw_socket());
    digest
        .peer_addr
        .set(Some(peer_addr.clone()))
        .expect("newly created OnceCell must be empty");
    stream.set_socket_digest(digest);

    Ok(stream)
}

pub(crate) fn bind_to_random<P: Peer>(
    peer: &P,
    v4_list: &[InetSocketAddr],
    v6_list: &[InetSocketAddr],
) -> Option<BindTo> {
    // helper function for randomly picking address
    fn bind_to_ips(ips: &[InetSocketAddr]) -> Option<InetSocketAddr> {
        match ips.len() {
            0 => None,
            1 => Some(ips[0]),
            _ => {
                // pick a random bind ip
                ips.choose(&mut rand::thread_rng()).copied()
            }
        }
    }

    let mut bind_to = peer.get_peer_options().and_then(|o| o.bind_to.clone());
    if bind_to.as_ref().map(|b| b.addr).is_some() {
        // already have a bind address selected
        return bind_to;
    }

    let addr = match peer.address() {
        SocketAddr::Inet(sockaddr) => match sockaddr {
            InetSocketAddr::V4(_) => bind_to_ips(v4_list),
            InetSocketAddr::V6(_) => bind_to_ips(v6_list),
        },
        #[cfg(unix)]
        SocketAddr::Unix(_) => None,
    };

    if addr.is_some() {
        if let Some(bind_to) = bind_to.as_mut() {
            bind_to.addr = addr;
        } else {
            bind_to = Some(BindTo {
                addr,
                ..Default::default()
            });
        }
    }
    bind_to
}

use crate::protocols::raw_connect;

#[cfg(unix)]
async fn proxy_connect<P: Peer>(peer: &P) -> Result<Stream> {
    // safe to unwrap
    let proxy = peer.get_proxy().unwrap();
    let options = peer.get_peer_options().unwrap();

    // combine required and optional headers
    let mut headers = proxy
        .headers
        .iter()
        .chain(options.extra_proxy_headers.iter());

    // not likely to timeout during connect() to UDS
    let stream: Box<Stream> = Box::new(
        connect_uds(&proxy.next_hop)
            .await
            .or_err_with(ConnectError, || {
                format!("CONNECT proxy connect() error to {:?}", &proxy.next_hop)
            })?
            .into(),
    );

    let req_header = raw_connect::generate_connect_header(&proxy.host, proxy.port, &mut headers)?;
    let fut = raw_connect::connect(stream, &req_header);
    let (mut stream, digest) = match peer.connection_timeout() {
        Some(t) => pingora_timeout::timeout(t, fut)
            .await
            .explain_err(ConnectTimedout, |_| "establishing CONNECT proxy")?,
        None => fut.await,
    }
    .map_err(|mut e| {
        // http protocol may ask to retry if reused client
        e.retry.decide_reuse(false);
        e
    })?;
    debug!("CONNECT proxy established: {:?}", proxy);
    stream.set_proxy_digest(digest);
    let stream = stream.into_any().downcast::<Stream>().unwrap(); // safe, it is Stream from above
    Ok(*stream)
}

#[cfg(windows)]
async fn proxy_connect<P: Peer>(peer: &P) -> Result<Stream> {
    panic!("peer proxy not supported on windows")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::upstreams::peer::{BasicPeer, HttpPeer, Proxy};
    use pingora_error::ErrorType;
    use std::collections::BTreeMap;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};
    use tokio::io::AsyncWriteExt;
    #[cfg(unix)]
    use tokio::net::UnixListener;
    use tokio::time::sleep;

    /// Some of the tests below are flaky when making new connections to mock
    /// servers. The servers are simple tokio listeners, so failures there are
    /// not indicative of real errors. This function will retry the peer/server
    /// in increasing intervals until it either succeeds in connecting or a long
    /// timeout expires (max 10sec)
    #[cfg(unix)]
    async fn wait_for_peer<P>(peer: &P)
    where
        P: Peer + Send + Sync,
    {
        use ErrorType as E;
        let start = Instant::now();
        let mut res = connect(peer, None).await;
        let mut delay = Duration::from_millis(5);
        let max_delay = Duration::from_secs(10);

        while start.elapsed() < max_delay {
            match &res {
                Err(e) if e.etype == E::ConnectRefused => {}
                _ => break,
            }
            sleep(delay).await;
            delay *= 2;
            res = connect(peer, None).await;
        }
    }

    #[tokio::test]
    async fn test_conn_error_refused() {
        let peer = BasicPeer::new("127.0.0.1:79"); // hopefully port 79 is not used
        let new_session = connect(&peer, None).await;
        assert_eq!(new_session.unwrap_err().etype(), &ConnectRefused)
    }

    // TODO broken on arm64
    #[ignore]
    #[tokio::test]
    async fn test_conn_error_no_route() {
        let peer = BasicPeer::new("[::3]:79"); // no route
        let new_session = connect(&peer, None).await;
        assert_eq!(new_session.unwrap_err().etype(), &ConnectNoRoute)
    }

    #[tokio::test]
    async fn test_conn_error_addr_not_avail() {
        let peer = HttpPeer::new("127.0.0.1:121".to_string(), false, "".to_string());
        let addr = "192.0.2.2:0".parse().ok();
        let bind_to = BindTo {
            addr,
            ..Default::default()
        };
        let new_session = connect(&peer, Some(bind_to)).await;
        assert_eq!(new_session.unwrap_err().etype(), &InternalError)
    }

    #[tokio::test]
    async fn test_conn_error_other() {
        let peer = HttpPeer::new("240.0.0.1:80".to_string(), false, "".to_string()); // non localhost
        let addr = "127.0.0.1:0".parse().ok();
        // create an error: cannot send from src addr: localhost to dst addr: a public IP
        let bind_to = BindTo {
            addr,
            ..Default::default()
        };
        let new_session = connect(&peer, Some(bind_to)).await;
        let error = new_session.unwrap_err();
        // XXX: some system will allow the socket to bind and connect without error, only to timeout
        assert!(error.etype() == &ConnectError || error.etype() == &ConnectTimedout)
    }

    #[tokio::test]
    async fn test_conn_timeout() {
        // 192.0.2.1 is effectively a blackhole
        let mut peer = BasicPeer::new("192.0.2.1:79");
        peer.options.connection_timeout = Some(std::time::Duration::from_millis(1)); //1ms
        let new_session = connect(&peer, None).await;
        assert_eq!(new_session.unwrap_err().etype(), &ConnectTimedout)
    }

    #[tokio::test]
    async fn test_tweak_hook() {
        const INIT_FLAG: bool = false;

        let flag = Arc::new(AtomicBool::new(INIT_FLAG));

        let mut peer = BasicPeer::new("1.1.1.1:80");

        let move_flag = Arc::clone(&flag);

        peer.options.upstream_tcp_sock_tweak_hook = Some(Arc::new(move |_| {
            move_flag.fetch_xor(true, Ordering::SeqCst);
            Ok(())
        }));

        connect(&peer, None).await.unwrap();

        assert_eq!(!INIT_FLAG, flag.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn test_custom_connect() {
        #[derive(Debug)]
        struct MyL4;
        #[async_trait]
        impl Connect for MyL4 {
            async fn connect(&self, _addr: &SocketAddr) -> Result<Stream> {
                tokio::net::TcpStream::connect("1.1.1.1:80")
                    .await
                    .map(|s| s.into())
                    .or_fail()
            }
        }
        // :79 shouldn't be able to be connected to
        let mut peer = BasicPeer::new("1.1.1.1:79");
        peer.options.custom_l4 = Some(std::sync::Arc::new(MyL4 {}));

        let new_session = connect(&peer, None).await;

        // but MyL4 connects to :80 instead
        assert!(new_session.is_ok());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_connect_proxy_fail() {
        let mut peer = HttpPeer::new("1.1.1.1:80".to_string(), false, "".to_string());
        let mut path = PathBuf::new();
        path.push("/tmp/123");
        peer.proxy = Some(Proxy {
            next_hop: path.into(),
            host: "1.1.1.1".into(),
            port: 80,
            headers: BTreeMap::new(),
        });
        let new_session = connect(&peer, None).await;
        let e = new_session.unwrap_err();
        assert_eq!(e.etype(), &ConnectError);
        assert!(!e.retry());
    }

    #[cfg(unix)]
    const MOCK_UDS_PATH: &str = "/tmp/test_unix_connect_proxy.sock";

    // one-off mock server
    #[cfg(unix)]
    async fn mock_connect_server() {
        let _ = std::fs::remove_file(MOCK_UDS_PATH);
        let listener = UnixListener::bind(MOCK_UDS_PATH).unwrap();
        if let Ok((mut stream, _addr)) = listener.accept().await {
            stream.write_all(b"HTTP/1.1 200 OK\r\n\r\n").await.unwrap();
            // wait a bit so that the client can read
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        let _ = std::fs::remove_file(MOCK_UDS_PATH);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_connect_proxy_work() {
        tokio::spawn(async {
            mock_connect_server().await;
        });
        // wait for the server to start
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let mut peer = HttpPeer::new("1.1.1.1:80".to_string(), false, "".to_string());
        let mut path = PathBuf::new();
        path.push(MOCK_UDS_PATH);
        peer.proxy = Some(Proxy {
            next_hop: path.into(),
            host: "1.1.1.1".into(),
            port: 80,
            headers: BTreeMap::new(),
        });
        let new_session = connect(&peer, None).await;
        assert!(new_session.is_ok());
    }

    #[cfg(unix)]
    const MOCK_BAD_UDS_PATH: &str = "/tmp/test_unix_bad_connect_proxy.sock";

    // one-off mock bad proxy
    // closes connection upon accepting
    #[cfg(unix)]
    async fn mock_connect_bad_server() {
        let _ = std::fs::remove_file(MOCK_BAD_UDS_PATH);
        let listener = UnixListener::bind(MOCK_BAD_UDS_PATH).unwrap();
        if let Ok((mut stream, _addr)) = listener.accept().await {
            stream.shutdown().await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        let _ = std::fs::remove_file(MOCK_BAD_UDS_PATH);
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread")]
    async fn test_connect_proxy_conn_closed() {
        tokio::spawn(async {
            mock_connect_bad_server().await;
        });
        // wait for the server to start
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let mut peer = HttpPeer::new("1.1.1.1:80".to_string(), false, "".to_string());
        let mut path = PathBuf::new();
        path.push(MOCK_BAD_UDS_PATH);
        peer.proxy = Some(Proxy {
            next_hop: path.into(),
            host: "1.1.1.1".into(),
            port: 80,
            headers: BTreeMap::new(),
        });
        let new_session = connect(&peer, None).await;
        let err = new_session.unwrap_err();
        assert_eq!(err.etype(), &ConnectionClosed);
        assert!(!err.retry());
    }

    #[cfg(target_os = "linux")]
    #[tokio::test(flavor = "multi_thread")]
    async fn test_bind_to_port_range_on_connect() {
        fn get_ip_local_port_range() -> (u16, u16) {
            let path = "/proc/sys/net/ipv4/ip_local_port_range";
            let file = std::fs::read_to_string(path).unwrap();
            let mut parts = file.split_whitespace();
            (
                parts.next().unwrap().parse().unwrap(),
                parts.next().unwrap().parse().unwrap(),
            )
        }

        // one-off mock server
        async fn mock_inet_connect_server() -> u16 {
            use tokio::net::TcpListener;
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();

            let port = listener.local_addr().unwrap().port();

            tokio::spawn(async move {
                if let Ok((mut stream, _addr)) = listener.accept().await {
                    stream.write_all(b"HTTP/1.1 200 OK\r\n\r\n").await.unwrap();
                    // wait a bit so that the client can read
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
            });

            port
        }

        fn in_port_range(session: Stream, lower: u16, upper: u16) -> bool {
            let digest = session.get_socket_digest();
            let local_addr = digest
                .as_ref()
                .and_then(|s| s.local_addr())
                .unwrap()
                .as_inet()
                .unwrap();

            // assert range
            local_addr.port() >= lower && local_addr.port() <= upper
        }

        let port = mock_inet_connect_server().await;

        // need to read /proc/sys/net/ipv4/ip_local_port_range for this test to work
        // IP_LOCAL_PORT_RANGE clamp only works on ports in /proc/sys/net/ipv4/ip_local_port_range
        let (low, _) = get_ip_local_port_range();
        let high = low + 1;

        let peer = HttpPeer::new(format!("127.0.0.1:{port}"), false, "".to_string());
        let mut bind_to = BindTo {
            addr: "127.0.0.1:0".parse().ok(),
            ..Default::default()
        };

        // wait for the server to start
        wait_for_peer(&peer).await;

        bind_to.set_port_range(Some((low, high))).unwrap();

        let mut success_count = 0;
        let mut address_unavailable_count = 0;

        // Issue a bunch of requests at once and ensure that all successful
        // requests have ports in the right range and that there is at least
        // one address-unavailable error because we are restricting the number
        // of ports so heavily
        for _ in 0..10 {
            match connect(&peer, Some(bind_to.clone())).await {
                Ok(session) => {
                    assert!(in_port_range(session, low, high));
                    success_count += 1;
                }
                Err(e) if format!("{e:?}").contains("AddrNotAvailable") => {
                    address_unavailable_count += 1;
                }
                Err(e) => {
                    panic!("Unexpected error {e:?}")
                }
            }
        }

        assert!(address_unavailable_count > 0);
        assert!(success_count >= (high - low));

        // enable fallback, assert not in port range but successful
        bind_to.set_fallback(true);
        let session4 = connect(&peer, Some(bind_to.clone())).await.unwrap();
        assert!(!in_port_range(session4, low, high));

        // works without bind IP, shift up to use new ports
        let low = low + 2;
        let high = low + 1;
        let mut bind_to = BindTo::default();
        bind_to.set_port_range(Some((low, high))).unwrap();
        let session5 = connect(&peer, Some(bind_to.clone())).await.unwrap();
        assert!(in_port_range(session5, low, high));
    }

    #[test]
    fn test_bind_to_port_ranges() {
        let addr = "127.0.0.1:0".parse().ok();
        let mut bind_to = BindTo {
            addr,
            ..Default::default()
        };

        // None because the previous value was None
        bind_to.set_port_range(None).unwrap();
        assert!(bind_to.port_range.is_none());

        // zeroes are handled
        bind_to.set_port_range(Some((0, 0))).unwrap();
        assert_eq!(bind_to.port_range, Some((0, 0)));

        // zeroes because the previous value was Some
        bind_to.set_port_range(None).unwrap();
        assert_eq!(bind_to.port_range, Some((0, 0)));

        // low > high is error
        assert!(bind_to.set_port_range(Some((2000, 1000))).is_err());

        // low < high success
        bind_to.set_port_range(Some((1000, 2000))).unwrap();
        assert_eq!(bind_to.port_range, Some((1000, 2000)));
    }
}
