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

//! Connecting to servers

pub mod http;
mod l4;
mod offload;
mod tls;

use crate::protocols::Stream;
use crate::server::configuration::ServerConf;
use crate::tls::ssl::SslConnector;
use crate::upstreams::peer::{Peer, ALPN};

use l4::connect as l4_connect;
pub use l4::Connect as L4Connect;
use log::{debug, error, warn};
use offload::OffloadRuntime;
use parking_lot::RwLock;
use pingora_error::{Error, ErrorType::*, OrErr, Result};
use pingora_pool::{ConnectionMeta, ConnectionPool};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::Mutex;

/// The options to configure a [TransportConnector]
#[derive(Clone)]
pub struct ConnectorOptions {
    /// Path to the CA file used to validate server certs.
    ///
    /// If `None`, the CA in the [default](https://www.openssl.org/docs/manmaster/man3/SSL_CTX_set_default_verify_paths.html)
    /// locations will be loaded
    pub ca_file: Option<String>,
    /// The default client cert and key to use for mTLS
    ///
    /// Each individual connection can use their own cert key to override this.
    pub cert_key_file: Option<(String, String)>,
    /// When enabled allows TLS keys to be written to a file specified by the SSLKEYLOG
    /// env variable. This can be used by tools like Wireshark to decrypt traffic
    /// for debugging purposes.
    pub debug_ssl_keylog: bool,
    /// How many connections to keepalive
    pub keepalive_pool_size: usize,
    /// Optionally offload the connection establishment to dedicated thread pools
    ///
    /// TCP and TLS connection establishment can be CPU intensive. Sometimes such tasks can slow
    /// down the entire service, which causes timeouts which leads to more connections which
    /// snowballs the issue. Use this option to isolate these CPU intensive tasks from impacting
    /// other traffic.
    ///
    /// Syntax: (#pools, #thread in each pool)
    pub offload_threadpool: Option<(usize, usize)>,
    /// Bind to any of the given source IPv6 addresses
    pub bind_to_v4: Vec<SocketAddr>,
    /// Bind to any of the given source IPv4 addresses
    pub bind_to_v6: Vec<SocketAddr>,
}

impl ConnectorOptions {
    /// Derive the [ConnectorOptions] from a [ServerConf]
    pub fn from_server_conf(server_conf: &ServerConf) -> Self {
        // if both pools and threads are Some(>0)
        let offload_threadpool = server_conf
            .upstream_connect_offload_threadpools
            .zip(server_conf.upstream_connect_offload_thread_per_pool)
            .filter(|(pools, threads)| *pools > 0 && *threads > 0);

        // create SocketAddrs with port 0 for src addr bind

        let bind_to_v4 = server_conf
            .client_bind_to_ipv4
            .iter()
            .map(|v4| {
                let ip = v4.parse().unwrap();
                SocketAddr::new(ip, 0)
            })
            .collect();

        let bind_to_v6 = server_conf
            .client_bind_to_ipv6
            .iter()
            .map(|v6| {
                let ip = v6.parse().unwrap();
                SocketAddr::new(ip, 0)
            })
            .collect();
        ConnectorOptions {
            ca_file: server_conf.ca_file.clone(),
            cert_key_file: None, // TODO: use it
            debug_ssl_keylog: server_conf.upstream_debug_ssl_keylog,
            keepalive_pool_size: server_conf.upstream_keepalive_pool_size,
            offload_threadpool,
            bind_to_v4,
            bind_to_v6,
        }
    }

    /// Create a new [ConnectorOptions] with the given keepalive pool size
    pub fn new(keepalive_pool_size: usize) -> Self {
        ConnectorOptions {
            ca_file: None,
            cert_key_file: None,
            debug_ssl_keylog: false,
            keepalive_pool_size,
            offload_threadpool: None,
            bind_to_v4: vec![],
            bind_to_v6: vec![],
        }
    }
}

/// [TransportConnector] provides APIs to connect to servers via TCP or TLS with connection reuse
pub struct TransportConnector {
    tls_ctx: tls::Connector,
    connection_pool: Arc<ConnectionPool<Arc<Mutex<Stream>>>>,
    offload: Option<OffloadRuntime>,
    bind_to_v4: Vec<SocketAddr>,
    bind_to_v6: Vec<SocketAddr>,
    preferred_http_version: PreferredHttpVersion,
}

const DEFAULT_POOL_SIZE: usize = 128;

impl TransportConnector {
    /// Create a new [TransportConnector] with the given [ConnectorOptions]
    pub fn new(mut options: Option<ConnectorOptions>) -> Self {
        let pool_size = options
            .as_ref()
            .map_or(DEFAULT_POOL_SIZE, |c| c.keepalive_pool_size);
        // Take the offloading setting there because this layer has implement offloading,
        // so no need for stacks at lower layer to offload again.
        let offload = options.as_mut().and_then(|o| o.offload_threadpool.take());
        let bind_to_v4 = options
            .as_ref()
            .map_or_else(Vec::new, |o| o.bind_to_v4.clone());
        let bind_to_v6 = options
            .as_ref()
            .map_or_else(Vec::new, |o| o.bind_to_v6.clone());
        TransportConnector {
            tls_ctx: tls::Connector::new(options),
            connection_pool: Arc::new(ConnectionPool::new(pool_size)),
            offload: offload.map(|v| OffloadRuntime::new(v.0, v.1)),
            bind_to_v4,
            bind_to_v6,
            preferred_http_version: PreferredHttpVersion::new(),
        }
    }

    /// Connect to the given server [Peer]
    ///
    /// No connection is reused.
    pub async fn new_stream<P: Peer + Send + Sync + 'static>(&self, peer: &P) -> Result<Stream> {
        let rt = self
            .offload
            .as_ref()
            .map(|o| o.get_runtime(peer.reuse_hash()));
        let bind_to = l4::bind_to_random(peer, &self.bind_to_v4, &self.bind_to_v6);
        let alpn_override = self.preferred_http_version.get(peer);
        let stream = if let Some(rt) = rt {
            let peer = peer.clone();
            let tls_ctx = self.tls_ctx.clone();
            rt.spawn(async move { do_connect(&peer, bind_to, alpn_override, &tls_ctx.ctx).await })
                .await
                .or_err(InternalError, "offload runtime failure")??
        } else {
            do_connect(peer, bind_to, alpn_override, &self.tls_ctx.ctx).await?
        };

        Ok(stream)
    }

    /// Try to find a reusable connection to the given server [Peer]
    pub async fn reused_stream<P: Peer + Send + Sync>(&self, peer: &P) -> Option<Stream> {
        match self.connection_pool.get(&peer.reuse_hash()) {
            Some(s) => {
                debug!("find reusable stream, trying to acquire it");
                {
                    let _ = s.lock().await;
                } // wait for the idle poll to release it
                match Arc::try_unwrap(s) {
                    Ok(l) => {
                        let mut stream = l.into_inner();
                        // test_reusable_stream: we assume server would never actively send data
                        // first on an idle stream.
                        #[cfg(unix)]
                        if peer.matches_fd(stream.id()) && test_reusable_stream(&mut stream) {
                            Some(stream)
                        } else {
                            None
                        }
                        #[cfg(windows)]
                        {
                            use std::os::windows::io::{AsRawSocket, RawSocket};
                            struct WrappedRawSocket(RawSocket);
                            impl AsRawSocket for WrappedRawSocket {
                                fn as_raw_socket(&self) -> RawSocket {
                                    self.0
                                }
                            }
                            if peer.matches_sock(WrappedRawSocket(stream.id() as RawSocket))
                                && test_reusable_stream(&mut stream)
                            {
                                Some(stream)
                            } else {
                                None
                            }
                        }
                    }
                    Err(_) => {
                        error!("failed to acquire reusable stream");
                        None
                    }
                }
            }
            None => {
                debug!("No reusable connection found for {peer}");
                None
            }
        }
    }

    /// Return the [Stream] to the [TransportConnector] for connection reuse.
    ///
    /// Not all TCP/TLS connections can be reused. It is the caller's responsibility to make sure
    /// that protocol over the [Stream] supports connection reuse and the [Stream] itself is ready
    /// to be reused.
    ///
    /// If a [Stream] is dropped instead of being returned via this function. it will be closed.
    pub fn release_stream(
        &self,
        mut stream: Stream,
        key: u64, // usually peer.reuse_hash()
        idle_timeout: Option<std::time::Duration>,
    ) {
        if !test_reusable_stream(&mut stream) {
            return;
        }
        let id = stream.id();
        let meta = ConnectionMeta::new(key, id);
        debug!("Try to keepalive client session");
        let stream = Arc::new(Mutex::new(stream));
        let locked_stream = stream.clone().try_lock_owned().unwrap(); // safe as we just created it
        let (notify_close, watch_use) = self.connection_pool.put(&meta, stream);
        let pool = self.connection_pool.clone(); //clone the arc
        let rt = pingora_runtime::current_handle();
        rt.spawn(async move {
            pool.idle_poll(locked_stream, &meta, idle_timeout, notify_close, watch_use)
                .await;
        });
    }

    /// Get a stream to the given server [Peer]
    ///
    /// This function will try to find a reusable [Stream] first. If there is none, a new connection
    /// will be made to the server.
    ///
    /// The returned boolean will indicate whether the stream is reused.
    pub async fn get_stream<P: Peer + Send + Sync + 'static>(
        &self,
        peer: &P,
    ) -> Result<(Stream, bool)> {
        let reused_stream = self.reused_stream(peer).await;
        if let Some(s) = reused_stream {
            Ok((s, true))
        } else {
            let s = self.new_stream(peer).await?;
            Ok((s, false))
        }
    }

    /// Tell the connector to always send h1 for ALPN for the given peer in the future.
    pub fn prefer_h1(&self, peer: &impl Peer) {
        self.preferred_http_version.add(peer, 1);
    }
}

// Perform the actual L4 and tls connection steps while respecting the peer's
// connection timeout if there is one
async fn do_connect<P: Peer + Send + Sync>(
    peer: &P,
    bind_to: Option<SocketAddr>,
    alpn_override: Option<ALPN>,
    tls_ctx: &SslConnector,
) -> Result<Stream> {
    // Create the future that does the connections, but don't evaluate it until
    // we decide if we need a timeout or not
    let connect_future = do_connect_inner(peer, bind_to, alpn_override, tls_ctx);

    match peer.total_connection_timeout() {
        Some(t) => match pingora_timeout::timeout(t, connect_future).await {
            Ok(res) => res,
            Err(_) => Error::e_explain(
                ConnectTimedout,
                format!("connecting to server {peer}, total-connection timeout {t:?}"),
            ),
        },
        None => connect_future.await,
    }
}

// Perform the actual L4 and tls connection steps with no timeout
async fn do_connect_inner<P: Peer + Send + Sync>(
    peer: &P,
    bind_to: Option<SocketAddr>,
    alpn_override: Option<ALPN>,
    tls_ctx: &SslConnector,
) -> Result<Stream> {
    let stream = l4_connect(peer, bind_to).await?;
    if peer.tls() {
        let tls_stream = tls::connect(stream, peer, alpn_override, tls_ctx).await?;
        Ok(Box::new(tls_stream))
    } else {
        Ok(Box::new(stream))
    }
}

struct PreferredHttpVersion {
    // TODO: shard to avoid the global lock
    versions: RwLock<HashMap<u64, u8>>, // <hash of peer, version>
}

// TODO: limit the size of this

impl PreferredHttpVersion {
    pub fn new() -> Self {
        PreferredHttpVersion {
            versions: RwLock::default(),
        }
    }

    pub fn add(&self, peer: &impl Peer, version: u8) {
        let key = peer.reuse_hash();
        let mut v = self.versions.write();
        v.insert(key, version);
    }

    pub fn get(&self, peer: &impl Peer) -> Option<ALPN> {
        let key = peer.reuse_hash();
        let v = self.versions.read();
        v.get(&key)
            .copied()
            .map(|v| if v == 1 { ALPN::H1 } else { ALPN::H2H1 })
    }
}

use futures::future::FutureExt;
use tokio::io::AsyncReadExt;

/// Test whether a stream is already closed or not reusable (server sent unexpected data)
fn test_reusable_stream(stream: &mut Stream) -> bool {
    let mut buf = [0; 1];
    let result = stream.read(&mut buf[..]).now_or_never();
    if let Some(data_result) = result {
        match data_result {
            Ok(n) => {
                if n == 0 {
                    debug!("Idle connection is closed");
                } else {
                    warn!("Unexpected data read in idle connection");
                }
            }
            Err(e) => {
                debug!("Idle connection is broken: {e:?}");
            }
        }
        false
    } else {
        true
    }
}

#[cfg(test)]
mod tests {
    use pingora_error::ErrorType;

    use super::*;
    use crate::tls::ssl::SslMethod;
    use crate::upstreams::peer::BasicPeer;
    use tokio::io::AsyncWriteExt;
    #[cfg(unix)]
    use tokio::net::UnixListener;

    // 192.0.2.1 is effectively a black hole
    const BLACK_HOLE: &str = "192.0.2.1:79";

    #[tokio::test]
    async fn test_connect() {
        let connector = TransportConnector::new(None);
        let peer = BasicPeer::new("1.1.1.1:80");
        // make a new connection to 1.1.1.1
        let stream = connector.new_stream(&peer).await.unwrap();
        connector.release_stream(stream, peer.reuse_hash(), None);

        let (_, reused) = connector.get_stream(&peer).await.unwrap();
        assert!(reused);
    }

    #[tokio::test]
    async fn test_connect_tls() {
        let connector = TransportConnector::new(None);
        let mut peer = BasicPeer::new("1.1.1.1:443");
        // BasicPeer will use tls when SNI is set
        peer.sni = "one.one.one.one".to_string();
        // make a new connection to https://1.1.1.1
        let stream = connector.new_stream(&peer).await.unwrap();
        connector.release_stream(stream, peer.reuse_hash(), None);

        let (_, reused) = connector.get_stream(&peer).await.unwrap();
        assert!(reused);
    }

    #[cfg(unix)]
    const MOCK_UDS_PATH: &str = "/tmp/test_unix_transport_connector.sock";

    // one-off mock server
    #[cfg(unix)]
    async fn mock_connect_server() {
        let _ = std::fs::remove_file(MOCK_UDS_PATH);
        let listener = UnixListener::bind(MOCK_UDS_PATH).unwrap();
        if let Ok((mut stream, _addr)) = listener.accept().await {
            stream.write_all(b"it works!").await.unwrap();
            // wait a bit so that the client can read
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        let _ = std::fs::remove_file(MOCK_UDS_PATH);
    }
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread")]
    async fn test_connect_uds() {
        tokio::spawn(async {
            mock_connect_server().await;
        });
        // create a new service at /tmp
        let connector = TransportConnector::new(None);
        let peer = BasicPeer::new_uds(MOCK_UDS_PATH).unwrap();
        // make a new connection to mock uds
        let mut stream = connector.new_stream(&peer).await.unwrap();
        let mut buf = [0; 9];
        let _ = stream.read(&mut buf).await.unwrap();
        assert_eq!(&buf, b"it works!");
        connector.release_stream(stream, peer.reuse_hash(), None);

        let (_, reused) = connector.get_stream(&peer).await.unwrap();
        assert!(reused);
    }

    async fn do_test_conn_timeout(conf: Option<ConnectorOptions>) {
        let connector = TransportConnector::new(conf);
        let mut peer = BasicPeer::new(BLACK_HOLE);
        peer.options.connection_timeout = Some(std::time::Duration::from_millis(1));
        let stream = connector.new_stream(&peer).await;
        match stream {
            Ok(_) => panic!("should throw an error"),
            Err(e) => assert_eq!(e.etype(), &ConnectTimedout),
        }
    }

    #[tokio::test]
    async fn test_conn_timeout() {
        do_test_conn_timeout(None).await;
    }

    #[tokio::test]
    async fn test_conn_timeout_with_offload() {
        let mut conf = ConnectorOptions::new(8);
        conf.offload_threadpool = Some((2, 2));
        do_test_conn_timeout(Some(conf)).await;
    }

    #[tokio::test]
    async fn test_connector_bind_to() {
        // connect to remote while bind to localhost will fail
        let peer = BasicPeer::new("240.0.0.1:80");
        let mut conf = ConnectorOptions::new(1);
        conf.bind_to_v4.push("127.0.0.1:0".parse().unwrap());
        let connector = TransportConnector::new(Some(conf));

        let stream = connector.new_stream(&peer).await;
        let error = stream.unwrap_err();
        // XXX: some systems will allow the socket to bind and connect without error, only to timeout
        assert!(error.etype() == &ConnectError || error.etype() == &ConnectTimedout)
    }

    /// Helper function for testing error handling in the `do_connect` function.
    /// This assumes that the connection will fail to on the peer and returns
    /// the decomposed error type and message
    async fn get_do_connect_failure_with_peer(peer: &BasicPeer) -> (ErrorType, String) {
        let ssl_connector = SslConnector::builder(SslMethod::tls()).unwrap().build();
        let stream = do_connect(peer, None, None, &ssl_connector).await;
        match stream {
            Ok(_) => panic!("should throw an error"),
            Err(e) => (
                e.etype().clone(),
                e.context
                    .as_ref()
                    .map(|ctx| ctx.as_str().to_owned())
                    .unwrap_or_default(),
            ),
        }
    }

    #[tokio::test]
    async fn test_do_connect_with_total_timeout() {
        let mut peer = BasicPeer::new(BLACK_HOLE);
        peer.options.total_connection_timeout = Some(std::time::Duration::from_millis(1));
        let (etype, context) = get_do_connect_failure_with_peer(&peer).await;
        assert_eq!(etype, ConnectTimedout);
        assert!(context.contains("total-connection timeout"));
    }

    #[tokio::test]
    async fn test_tls_connect_timeout_supersedes_total() {
        let mut peer = BasicPeer::new(BLACK_HOLE);
        peer.options.total_connection_timeout = Some(std::time::Duration::from_millis(10));
        peer.options.connection_timeout = Some(std::time::Duration::from_millis(1));
        let (etype, context) = get_do_connect_failure_with_peer(&peer).await;
        assert_eq!(etype, ConnectTimedout);
        assert!(!context.contains("total-connection timeout"));
    }

    #[tokio::test]
    async fn test_do_connect_without_total_timeout() {
        let peer = BasicPeer::new(BLACK_HOLE);
        let (etype, context) = get_do_connect_failure_with_peer(&peer).await;
        assert!(etype != ConnectTimedout || !context.contains("total-connection timeout"));
    }
}
