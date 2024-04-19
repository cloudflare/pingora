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

use log::debug;
use pingora_error::{Context, Error, ErrorType::*, OrErr, Result};
use rand::seq::SliceRandom;
use std::net::SocketAddr as InetSocketAddr;
use std::os::unix::io::AsRawFd;

use crate::protocols::l4::ext::{
    connect as tcp_connect, connect_uds, set_recv_buf, set_tcp_keepalive,
};
use crate::protocols::l4::socket::SocketAddr;
use crate::protocols::l4::stream::Stream;
use crate::protocols::{GetSocketDigest, SocketDigest};
use crate::upstreams::peer::Peer;

/// Establish a connection (l4) to the given peer using its settings and an optional bind address.
pub async fn connect<P>(peer: &P, bind_to: Option<InetSocketAddr>) -> Result<Stream>
where
    P: Peer + Send + Sync,
{
    if peer.get_proxy().is_some() {
        return proxy_connect(peer)
            .await
            .err_context(|| format!("Fail to establish CONNECT proxy: {}", peer));
    }
    let peer_addr = peer.address();
    let mut stream: Stream = match peer_addr {
        SocketAddr::Inet(addr) => {
            let connect_future = tcp_connect(addr, bind_to.as_ref());
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
                    if let Some(ka) = peer.tcp_keepalive() {
                        debug!("Setting tcp keepalive");
                        set_tcp_keepalive(&socket, ka)?;
                    }
                    if let Some(recv_buf) = peer.tcp_recv_buf() {
                        debug!("Setting recv buf size");
                        set_recv_buf(socket.as_raw_fd(), recv_buf)?;
                    }
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
                    // no SO_KEEPALIVE for UDS
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
    }?;
    let tracer = peer.get_tracer();
    if let Some(t) = tracer {
        t.0.on_connected();
        stream.tracer = Some(t);
    }

    stream.set_nodelay()?;

    let digest = SocketDigest::from_raw_fd(stream.as_raw_fd());
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
) -> Option<InetSocketAddr> {
    let selected = peer.get_peer_options().and_then(|o| o.bind_to);
    if selected.is_some() {
        return selected;
    }

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

    match peer.address() {
        SocketAddr::Inet(sockaddr) => match sockaddr {
            InetSocketAddr::V4(_) => bind_to_ips(v4_list),
            InetSocketAddr::V6(_) => bind_to_ips(v6_list),
        },
        SocketAddr::Unix(_) => None,
    }
}

use crate::protocols::raw_connect;

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::upstreams::peer::{BasicPeer, HttpPeer, Proxy};
    use std::collections::BTreeMap;
    use std::path::PathBuf;
    use tokio::io::AsyncWriteExt;
    use tokio::net::UnixListener;

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
        let new_session = connect(&peer, Some("192.0.2.2:0".parse().unwrap())).await;
        assert_eq!(new_session.unwrap_err().etype(), &InternalError)
    }

    #[tokio::test]
    async fn test_conn_error_other() {
        let peer = HttpPeer::new("240.0.0.1:80".to_string(), false, "".to_string()); // non localhost

        // create an error: cannot send from src addr: localhost to dst addr: a public IP
        let new_session = connect(&peer, Some("127.0.0.1:0".parse().unwrap())).await;
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

    const MOCK_UDS_PATH: &str = "/tmp/test_unix_connect_proxy.sock";

    // one-off mock server
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

    const MOCK_BAD_UDS_PATH: &str = "/tmp/test_unix_bad_connect_proxy.sock";

    // one-off mock bad proxy
    // closes connection upon accepting
    async fn mock_connect_bad_server() {
        let _ = std::fs::remove_file(MOCK_BAD_UDS_PATH);
        let listener = UnixListener::bind(MOCK_BAD_UDS_PATH).unwrap();
        if let Ok((mut stream, _addr)) = listener.accept().await {
            stream.shutdown().await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        let _ = std::fs::remove_file(MOCK_BAD_UDS_PATH);
    }

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
}
