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

use super::HttpSession;
use crate::connectors::{ConnectorOptions, TransportConnector};
use crate::protocols::http::custom::client::Session;
use crate::protocols::http::v1::client::HttpSession as Http1Session;
use crate::protocols::http::v2::client::{drive_connection, Http2Session};
use crate::protocols::{Digest, Stream, UniqueIDType};
use crate::upstreams::peer::{Peer, ALPN};

use bytes::Bytes;
use h2::client::SendRequest;
use log::debug;
use parking_lot::{Mutex, RwLock};
use pingora_error::{Error, ErrorType::*, OrErr, Result};
use pingora_pool::{ConnectionMeta, ConnectionPool, PoolNode};
use std::collections::HashMap;
use std::io::ErrorKind;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;

struct Stub(SendRequest<Bytes>);

impl Stub {
    async fn new_stream(&self) -> Result<SendRequest<Bytes>> {
        let send_req = self.0.clone();
        send_req
            .ready()
            .await
            .or_err(H2Error, "while creating new stream")
    }
}

pub(crate) struct ConnectionRefInner {
    connection_stub: Stub,
    closed: watch::Receiver<bool>,
    ping_timeout_occurred: Arc<AtomicBool>,
    id: UniqueIDType,
    // max concurrent streams this connection is allowed to create
    max_streams: usize,
    // how many concurrent streams already active
    current_streams: AtomicUsize,
    // The connection is gracefully shutting down, no more stream is allowed
    shutting_down: AtomicBool,
    // because `SendRequest` doesn't actually have access to the underlying Stream,
    // we log info about timing and tcp info here.
    pub(crate) digest: Digest,
    // To serialize certain operations when trying to release the connect back to the pool,
    pub(crate) release_lock: Arc<Mutex<()>>,
}

#[derive(Clone)]
pub struct ConnectionRef(Arc<ConnectionRefInner>);

impl ConnectionRef {
    pub fn new(
        send_req: SendRequest<Bytes>,
        closed: watch::Receiver<bool>,
        ping_timeout_occurred: Arc<AtomicBool>,
        id: UniqueIDType,
        max_streams: usize,
        digest: Digest,
    ) -> Self {
        ConnectionRef(Arc::new(ConnectionRefInner {
            connection_stub: Stub(send_req),
            closed,
            ping_timeout_occurred,
            id,
            max_streams,
            current_streams: AtomicUsize::new(0),
            shutting_down: false.into(),
            digest,
            release_lock: Arc::new(Mutex::new(())),
        }))
    }

    pub fn more_streams_allowed(&self) -> bool {
        let current = self.0.current_streams.load(Ordering::Relaxed);
        !self.is_shutting_down()
            && self.0.max_streams > current
            && self.0.connection_stub.0.current_max_send_streams() > current
    }

    pub fn is_idle(&self) -> bool {
        self.0.current_streams.load(Ordering::Relaxed) == 0
    }

    pub fn release_stream(&self) {
        self.0.current_streams.fetch_sub(1, Ordering::SeqCst);
    }

    pub fn id(&self) -> UniqueIDType {
        self.0.id
    }

    pub fn digest(&self) -> &Digest {
        &self.0.digest
    }

    pub fn digest_mut(&mut self) -> Option<&mut Digest> {
        Arc::get_mut(&mut self.0).map(|inner| &mut inner.digest)
    }

    pub fn ping_timedout(&self) -> bool {
        self.0.ping_timeout_occurred.load(Ordering::Relaxed)
    }

    pub fn is_closed(&self) -> bool {
        *self.0.closed.borrow()
    }

    // different from is_closed, existing streams can still be processed but can no longer create
    // new stream.
    pub fn is_shutting_down(&self) -> bool {
        self.0.shutting_down.load(Ordering::Relaxed)
    }

    /// Mark this connection for shutdown.
    ///
    /// No new streams will be created on this connection and
    /// it will be discarded once all active streams are released.
    pub fn mark_shutdown(&self) {
        self.0.shutting_down.store(true, Ordering::Relaxed);
    }

    // spawn a stream if more stream is allowed, otherwise return Ok(None)
    pub async fn spawn_stream(&self) -> Result<Option<Http2Session>> {
        // Atomically check if the current_stream is over the limit
        // load(), compare and then fetch_add() cannot guarantee the same
        let current_streams = self.0.current_streams.fetch_add(1, Ordering::SeqCst);
        if current_streams >= self.0.max_streams {
            // already over the limit, reset the counter to the previous value
            self.0.current_streams.fetch_sub(1, Ordering::SeqCst);
            return Ok(None);
        }

        match self.0.connection_stub.new_stream().await {
            Ok(send_req) => Ok(Some(Http2Session::new(send_req, self.clone()))),
            Err(e) => {
                // fail to create the stream, reset the counter
                self.0.current_streams.fetch_sub(1, Ordering::SeqCst);

                // Check for graceful shutdown conditions where we can retry with a new connection
                let is_graceful_shutdown = e
                    .root_cause()
                    .downcast_ref::<h2::Error>()
                    .map(|e| {
                        // Remote sends GOAWAY(NO_ERROR): graceful shutdown
                        (e.is_go_away() && e.is_remote() && e.reason() == Some(h2::Reason::NO_ERROR))
                        // Or broken pipe wrapped inside an h2::Error: stream closed unexpectedly
                        || (e.is_io()
                            && e.get_io()
                                .map(|io| io.kind() == ErrorKind::BrokenPipe)
                                .unwrap_or(false))
                    })
                    .unwrap_or(false);

                if is_graceful_shutdown {
                    self.mark_shutdown();
                    Ok(None)
                } else {
                    Err(e)
                }
            }
        }
    }
}

pub struct InUsePool {
    // TODO: use pingora hashmap to shard the lock contention
    pools: RwLock<HashMap<u64, PoolNode<ConnectionRef>>>,
}

impl InUsePool {
    fn new() -> Self {
        InUsePool {
            pools: RwLock::new(HashMap::new()),
        }
    }

    /// Attempt to remove an empty [`PoolNode`] entry from the pools `HashMap`.
    ///
    /// Same rationale as [`ConnectionPool::try_remove_empty_node`]: prevents
    /// unbounded growth when many unique reuse hashes are seen over time.
    /// The write lock + re-check ensures we never remove a node that was
    /// concurrently repopulated.
    fn try_remove_empty_node(&self, reuse_hash: u64) {
        let mut pools = self.pools.write();
        if let Some(pool) = pools.get(&reuse_hash) {
            if pool.is_empty() {
                pools.remove(&reuse_hash);
            }
        }
    }

    pub fn insert(&self, reuse_hash: u64, conn: ConnectionRef) {
        {
            let pools = self.pools.read();
            if let Some(pool) = pools.get(&reuse_hash) {
                pool.insert(conn.id(), conn);
                return;
            }
        } // drop read lock

        let mut pools = self.pools.write();
        // Double-check: another thread may have inserted a node between
        // dropping the read lock and acquiring this write lock.
        if let Some(pool) = pools.get(&reuse_hash) {
            pool.insert(conn.id(), conn);
            return;
        }
        let pool = PoolNode::new();
        pool.insert(conn.id(), conn);
        pools.insert(reuse_hash, pool);
    }

    // retrieve a h2 conn ref to create a new stream
    // the caller should return the conn ref to this pool if there are still
    // capacity left for more streams
    pub fn get(&self, reuse_hash: u64) -> Option<ConnectionRef> {
        let (result, maybe_empty) = {
            let pools = self.pools.read();
            match pools.get(&reuse_hash) {
                Some(pool) => match pool.get_any() {
                    Some((_, conn)) => (Some(conn), pool.is_empty()),
                    None => (None, true),
                },
                None => (None, false),
            }
        }; // read lock released here

        if maybe_empty {
            self.try_remove_empty_node(reuse_hash);
        }

        result
    }

    // release a h2_stream, this functional will cause an ConnectionRef to be returned (if exist)
    // the caller should update the ref and then decide where to put it (in use pool or idle)
    pub fn release(&self, reuse_hash: u64, id: UniqueIDType) -> Option<ConnectionRef> {
        let (result, maybe_empty) = {
            let pools = self.pools.read();
            if let Some(pool) = pools.get(&reuse_hash) {
                let removed = pool.remove(id);
                (removed, pool.is_empty())
            } else {
                (None, false)
            }
        }; // read lock released here

        if maybe_empty {
            self.try_remove_empty_node(reuse_hash);
        }

        result
    }
}

const DEFAULT_POOL_SIZE: usize = 128;

/// Http2 connector
pub struct Connector {
    // just for creating connections, the Stream of h2 should be reused
    transport: TransportConnector,
    // the h2 connection idle pool
    idle_pool: Arc<ConnectionPool<ConnectionRef>>,
    // the pool of h2 connections that have ongoing streams
    in_use_pool: InUsePool,
}

impl Connector {
    /// Create a new [Connector] from the given [ConnectorOptions]
    pub fn new(options: Option<ConnectorOptions>) -> Self {
        let pool_size = options
            .as_ref()
            .map_or(DEFAULT_POOL_SIZE, |o| o.keepalive_pool_size);
        // connection offload is handled by the [TransportConnector]
        Connector {
            transport: TransportConnector::new(options),
            idle_pool: Arc::new(ConnectionPool::new(pool_size)),
            in_use_pool: InUsePool::new(),
        }
    }

    pub fn transport(&self) -> &TransportConnector {
        &self.transport
    }

    pub fn idle_pool(&self) -> &Arc<ConnectionPool<ConnectionRef>> {
        &self.idle_pool
    }

    pub fn in_use_pool(&self) -> &InUsePool {
        &self.in_use_pool
    }

    /// Create a new Http2 connection to the given server
    ///
    /// Either an Http2 or Http1 session can be returned depending on the server's preference.
    pub async fn new_http_session<P: Peer + Send + Sync + 'static, C: Session>(
        &self,
        peer: &P,
    ) -> Result<HttpSession<C>> {
        let stream = self.transport.new_stream(peer).await?;

        // check alpn
        match stream.selected_alpn_proto() {
            Some(ALPN::H2) => { /* continue */ }
            Some(_) => {
                // H2 not supported
                return Ok(HttpSession::H1(Http1Session::new_with_options(
                    stream, peer,
                )));
            }
            None => {
                // if tls but no ALPN, default to h1
                // else if plaintext and min http version is 1, this is most likely h1
                if peer.tls()
                    || peer
                        .get_peer_options()
                        .is_none_or(|o| o.alpn.get_min_http_version() == 1)
                {
                    return Ok(HttpSession::H1(Http1Session::new_with_options(
                        stream, peer,
                    )));
                }
                // else: min http version=H2 over plaintext, there is no ALPN anyways, we trust
                // the caller that the server speaks h2c
            }
        }
        let peer_options = peer.get_peer_options();
        let mut settings = H2HandshakeSettings::new();
        settings.max_streams = peer_options.map_or(1, |o| o.max_h2_streams);
        settings.ping_interval = peer.h2_ping_interval();
        settings.stream_window_size = peer_options.and_then(|o| o.h2_stream_window_size);
        settings.connection_window_size = peer_options.and_then(|o| o.h2_connection_window_size);
        let conn = handshake(stream, settings).await?;
        let h2_stream = conn
            .spawn_stream()
            .await?
            .expect("newly created connections should have at least one free stream");
        if conn.more_streams_allowed() {
            self.in_use_pool.insert(peer.reuse_hash(), conn);
        }
        Ok(HttpSession::H2(h2_stream))
    }

    /// Try to create a new http2 stream from any existing H2 connection.
    ///
    /// None means there is no "free" connection left.
    pub async fn reused_http_session<P: Peer + Send + Sync + 'static>(
        &self,
        peer: &P,
    ) -> Result<Option<Http2Session>> {
        // check in use pool first so that we use fewer total connections
        // then idle pool
        let reuse_hash = peer.reuse_hash();

        // NOTE: We grab a conn from the pools, create a new stream and put the conn back if the
        // conn has more free streams. During this process another caller could arrive but is not
        // able to find the conn even the conn has free stream to use.
        // We accept this false negative to keep the implementation simple. This false negative
        // makes an actual impact when there are only a few connection.
        // Alternative design 1. given each free stream a conn object: a lot of Arc<>
        // Alternative design 2. mutex the pool, which creates lock contention when concurrency is high
        // Alternative design 3. do not pop conn from the pool so that multiple callers can grab it
        // which will cause issue where spawn_stream() could return None because others call it
        // first. Thus a caller might have to retry or give up. This issue is more likely to happen
        // when concurrency is high.
        let maybe_conn = self
            .in_use_pool
            .get(reuse_hash)
            // filter out closed, InUsePool does not have notify closed eviction like the idle pool
            // and it's possible we get an in use connection that is closed and not yet released
            .filter(|c| !c.is_closed())
            .or_else(|| self.idle_pool.get(&reuse_hash));
        if let Some(conn) = maybe_conn {
            #[cfg(unix)]
            if !peer.matches_fd(conn.id()) {
                return Ok(None);
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
                if !peer.matches_sock(WrappedRawSocket(conn.id() as RawSocket)) {
                    return Ok(None);
                }
            }
            let h2_stream = conn.spawn_stream().await?;
            if conn.more_streams_allowed() {
                self.in_use_pool.insert(reuse_hash, conn);
            }
            Ok(h2_stream)
        } else {
            Ok(None)
        }
    }

    /// Release a finished h2 stream.
    ///
    /// This function will terminate the [Http2Session]. The corresponding h2 connection will now
    /// have one more free stream to use.
    ///
    /// The h2 connection will be closed after `idle_timeout` if it has no active streams.
    pub fn release_http_session<P: Peer + Send + Sync + 'static>(
        &self,
        session: Http2Session,
        peer: &P,
        idle_timeout: Option<Duration>,
    ) {
        let id = session.conn.id();
        let reuse_hash = peer.reuse_hash();
        // get a ref to the connection, which we might need below, before dropping the h2
        let conn = session.conn();

        // The lock here is to make sure that in_use_pool.insert() below cannot be called after
        // in_use_pool.release(), which would have put the conn entry in both pools.
        // It also makes sure that only one conn will trigger the conn.is_idle() condition, which
        // avoids putting the same conn into the idle_pool more than once.
        let locked = conn.0.release_lock.lock_arc();
        // this drop() will both drop the actual stream and call the conn.release_stream()
        drop(session);
        // find and remove the conn stored in in_use_pool so that it could be put in the idle pool
        // if necessary
        let conn = self.in_use_pool.release(reuse_hash, id).unwrap_or(conn);
        if conn.is_closed() || conn.is_shutting_down() {
            // should never be put back to the pool
            return;
        }
        if conn.is_idle() {
            drop(locked);
            let meta = ConnectionMeta {
                key: reuse_hash,
                id,
            };
            let closed = conn.0.closed.clone();
            let (notify_evicted, watch_use) = self.idle_pool.put(&meta, conn);
            let pool = self.idle_pool.clone(); //clone the arc
            let rt = pingora_runtime::current_handle();
            rt.spawn(async move {
                pool.idle_timeout(&meta, idle_timeout, notify_evicted, closed, watch_use)
                    .await;
            });
        } else {
            self.in_use_pool.insert(reuse_hash, conn);
            drop(locked);
        }
    }

    /// Tell the connector to always send h1 for ALPN for the given peer in the future.
    pub fn prefer_h1(&self, peer: &impl Peer) {
        self.transport.prefer_h1(peer);
    }

    pub(crate) fn h1_is_preferred(&self, peer: &impl Peer) -> bool {
        self.transport
            .preferred_http_version
            .get(peer)
            .is_some_and(|v| matches!(v, ALPN::H1))
    }
}

// The h2 library we use has unbounded internal buffering, which will cause excessive memory
// consumption when the downstream is slower than upstream. This window size caps the buffering by
// limiting how much data can be inflight. However, setting this value will also cap the max
// download speed by limiting the bandwidth-delay product of a link.
// Long term, we should advertising large window but shrink it when a small buffer is full.
// 8 Mbytes = 80 Mbytes X 100ms, which should be enough for most links.
const H2_WINDOW_SIZE: u32 = 1 << 23;

/// Maximum allowed H2 window size per [RFC 9113 §6.9.1](https://datatracker.ietf.org/doc/html/rfc9113#section-6.9.1-7).
const H2_MAX_WINDOW_SIZE: u32 = (1u32 << 31) - 1;

/// Settings for HTTP/2 handshake.
///
/// # Example
///
/// ```rust,ignore
/// use pingora_core::connectors::http::v2::{handshake, H2HandshakeSettings};
///
/// // With custom window sizes
/// let mut settings = H2HandshakeSettings::new();
/// settings.max_streams = 100;
/// settings.stream_window_size = Some(1 << 20);  // 1MiB
/// settings.connection_window_size = Some(1 << 24);  // 16MiB
/// let conn = handshake(stream, settings).await?;
/// ```
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct H2HandshakeSettings {
    /// The maximum number of concurrent streams allowed on this connection.
    pub max_streams: usize,
    /// Optional interval for sending H2 ping frames to keep the connection alive.
    pub ping_interval: Option<Duration>,
    /// Optional initial per-stream receive window size in bytes.
    /// If `None`, the default of 8MB is used.
    pub stream_window_size: Option<u32>,
    /// Optional initial connection-level receive window size in bytes.
    /// If `None`, the default of 8MB is used.
    pub connection_window_size: Option<u32>,
}

impl H2HandshakeSettings {
    /// Create a new `H2HandshakeSettings` with all defaults.
    pub fn new() -> Self {
        Self::default()
    }
}

/// Perform an HTTP/2 handshake on the given stream with the given settings.
pub async fn handshake(stream: Stream, settings: H2HandshakeSettings) -> Result<ConnectionRef> {
    use h2::client::Builder;
    use pingora_runtime::current_handle;

    let max_streams = settings.max_streams;

    // Safe guard: new_http_session() assumes there should be at least one free stream
    if max_streams == 0 {
        return Error::e_explain(H2Error, "zero max_stream configured");
    }

    // Validate window sizes against RFC 9113 §6.9.1 limit
    // https://datatracker.ietf.org/doc/html/rfc9113#section-6.9.1-7
    if settings
        .stream_window_size
        .is_some_and(|w| w == 0 || w > H2_MAX_WINDOW_SIZE)
    {
        return Error::e_explain(
            H2Error,
            format!(
                "stream_window_size must be between 1 and {} (2^31-1)",
                H2_MAX_WINDOW_SIZE
            ),
        );
    }
    if settings
        .connection_window_size
        .is_some_and(|w| w == 0 || w > H2_MAX_WINDOW_SIZE)
    {
        return Error::e_explain(
            H2Error,
            format!(
                "connection_window_size must be between 1 and {} (2^31-1)",
                H2_MAX_WINDOW_SIZE
            ),
        );
    }

    let id = stream.id();
    let digest = Digest {
        // NOTE: this field is always false because the digest is shared across all streams
        // The streams should log their own reuse info
        ssl_digest: stream.get_ssl_digest(),
        // TODO: log h2 handshake time
        timing_digest: stream.get_timing_digest(),
        proxy_digest: stream.get_proxy_digest(),
        socket_digest: stream.get_socket_digest(),
    };
    let stream_window = settings.stream_window_size.unwrap_or(H2_WINDOW_SIZE);
    let conn_window = settings.connection_window_size.unwrap_or(H2_WINDOW_SIZE);
    let (send_req, connection) = Builder::new()
        .enable_push(false)
        .initial_max_send_streams(max_streams)
        // The limit for the server. Server push is not allowed, so this value doesn't matter
        .max_concurrent_streams(1)
        .max_frame_size(64 * 1024) // advise server to send larger frames
        .initial_window_size(stream_window)
        .initial_connection_window_size(conn_window)
        .handshake(stream)
        .await
        .or_err(HandshakeError, "during H2 handshake")?;
    debug!("H2 handshake to server done.");
    let ping_timeout_occurred = Arc::new(AtomicBool::new(false));
    let ping_timeout_clone = ping_timeout_occurred.clone();
    let max_allowed_streams = std::cmp::min(max_streams, connection.max_concurrent_send_streams());

    // Safe guard: new_http_session() assumes there should be at least one free stream
    // The server won't commonly advertise 0 max stream.
    if max_allowed_streams == 0 {
        return Error::e_explain(H2Error, "zero max_concurrent_send_streams received");
    }

    let (closed_tx, closed_rx) = watch::channel(false);

    current_handle().spawn(async move {
        drive_connection(
            connection,
            id,
            closed_tx,
            settings.ping_interval,
            ping_timeout_clone,
        )
        .await;
    });
    Ok(ConnectionRef::new(
        send_req,
        closed_rx,
        ping_timeout_occurred,
        id,
        max_allowed_streams,
        digest,
    ))
}

// TODO(slava): add custom unit tests
#[cfg(test)]
mod tests {
    use super::*;
    use crate::upstreams::peer::HttpPeer;
    use bytes::Bytes;
    use http::{Response, StatusCode};
    use pingora_http::RequestHeader;

    #[tokio::test]
    #[cfg(feature = "any_tls")]
    async fn test_connect_h2() {
        let connector = Connector::new(None);
        let mut peer = HttpPeer::new(("1.1.1.1", 443), true, "one.one.one.one".into());
        peer.options.set_http_version(2, 2);
        let h2 = connector
            .new_http_session::<HttpPeer, ()>(&peer)
            .await
            .unwrap();
        match h2 {
            HttpSession::H1(_) => panic!("expect h2"),
            HttpSession::H2(h2_stream) => assert!(!h2_stream.ping_timedout()),
            HttpSession::Custom(_) => panic!("expect h2"),
        }
    }

    #[tokio::test]
    #[cfg(feature = "any_tls")]
    async fn test_connect_h1() {
        let connector = Connector::new(None);
        let mut peer = HttpPeer::new(("1.1.1.1", 443), true, "one.one.one.one".into());
        // a hack to force h1, new_http_session() in the future might validate this setting
        peer.options.set_http_version(1, 1);
        let h2 = connector
            .new_http_session::<HttpPeer, ()>(&peer)
            .await
            .unwrap();
        match h2 {
            HttpSession::H1(_) => {}
            HttpSession::H2(_) => panic!("expect h1"),
            HttpSession::Custom(_) => panic!("expect h1"),
        }
    }

    #[tokio::test]
    async fn test_connect_h1_plaintext() {
        let connector = Connector::new(None);
        let mut peer = HttpPeer::new(("1.1.1.1", 80), false, "".into());
        peer.options.set_http_version(2, 1);
        let h2 = connector
            .new_http_session::<HttpPeer, ()>(&peer)
            .await
            .unwrap();
        match h2 {
            HttpSession::H1(_) => {}
            HttpSession::H2(_) => panic!("expect h1"),
            HttpSession::Custom(_) => panic!("expect h1"),
        }
    }

    #[tokio::test]
    #[cfg(feature = "any_tls")]
    async fn test_h2_single_stream() {
        let connector = Connector::new(None);
        let mut peer = HttpPeer::new(("1.1.1.1", 443), true, "one.one.one.one".into());
        peer.options.set_http_version(2, 2);
        peer.options.max_h2_streams = 1;
        let h2 = connector
            .new_http_session::<HttpPeer, ()>(&peer)
            .await
            .unwrap();
        let h2_1 = match h2 {
            HttpSession::H1(_) => panic!("expect h2"),
            HttpSession::H2(h2_stream) => h2_stream,
            HttpSession::Custom(_) => panic!("expect h2"),
        };

        let id = h2_1.conn.id();

        assert!(connector
            .reused_http_session(&peer)
            .await
            .unwrap()
            .is_none());

        connector.release_http_session(h2_1, &peer, None);

        let h2_2 = connector.reused_http_session(&peer).await.unwrap().unwrap();
        assert_eq!(id, h2_2.conn.id());

        connector.release_http_session(h2_2, &peer, None);

        let h2_3 = connector.reused_http_session(&peer).await.unwrap().unwrap();
        assert_eq!(id, h2_3.conn.id());
    }

    #[tokio::test]
    #[cfg(feature = "any_tls")]
    async fn test_h2_multiple_stream() {
        let connector = Connector::new(None);
        let mut peer = HttpPeer::new(("1.1.1.1", 443), true, "one.one.one.one".into());
        peer.options.set_http_version(2, 2);
        peer.options.max_h2_streams = 3;
        let h2 = connector
            .new_http_session::<HttpPeer, ()>(&peer)
            .await
            .unwrap();
        let h2_1 = match h2 {
            HttpSession::H1(_) => panic!("expect h2"),
            HttpSession::H2(h2_stream) => h2_stream,
            HttpSession::Custom(_) => panic!("expect h2"),
        };

        let id = h2_1.conn.id();

        let h2_2 = connector.reused_http_session(&peer).await.unwrap().unwrap();
        assert_eq!(id, h2_2.conn.id());
        let h2_3 = connector.reused_http_session(&peer).await.unwrap().unwrap();
        assert_eq!(id, h2_3.conn.id());

        // max stream is 3 for now
        assert!(connector
            .reused_http_session(&peer)
            .await
            .unwrap()
            .is_none());

        connector.release_http_session(h2_1, &peer, None);

        let h2_4 = connector.reused_http_session(&peer).await.unwrap().unwrap();
        assert_eq!(id, h2_4.conn.id());

        connector.release_http_session(h2_2, &peer, None);
        connector.release_http_session(h2_3, &peer, None);
        connector.release_http_session(h2_4, &peer, None);

        // all streams are released, now the connection is idle
        let h2_5 = connector.reused_http_session(&peer).await.unwrap().unwrap();
        assert_eq!(id, h2_5.conn.id());
    }

    /// `spawn_stream` must return `Ok(None)` and mark the connection as shutting
    /// down when the underlying I/O channel is closed (BrokenPipe).  This
    /// exercises the BrokenPipe branch of `spawn_stream` directly without going
    /// through the full proxy stack.
    #[tokio::test]
    async fn test_spawn_stream_broken_pipe_marks_shutdown() {
        let (client_io, server_io) = tokio::io::duplex(65536);
        let (send_req, connection) = h2::client::handshake(client_io).await.unwrap();
        let (closed_tx, closed_rx) = watch::channel(false);
        let ping_timeout = Arc::new(AtomicBool::new(false));
        let conn = ConnectionRef::new(send_req, closed_rx, ping_timeout, 0, 10, Digest::default());

        // Drive the H2 client connection task in the background.
        // When the connection terminates it will signal via closed_tx.
        let conn_handle = tokio::spawn(async move {
            let _ = connection.await;
            // Signal that the connection task has finished.
            let _ = closed_tx.send(true);
        });

        // Complete the server-side H2 handshake, then drop the server connection.
        // Dropping server_conn closes the write end of the duplex, so the client
        // connection task will read EOF and terminate with BrokenPipe.
        let server_conn = h2::server::handshake(server_io).await.unwrap();
        drop(server_conn);

        // Wait until the client connection task has fully processed the EOF.
        conn_handle.await.unwrap();

        // spawn_stream must detect BrokenPipe, mark shutdown, and return Ok(None)
        // so the caller can retry on a fresh connection rather than propagating the error.
        let result = conn.spawn_stream().await;
        assert!(result.is_ok(), "expected Ok(None), got Err");
        assert!(result.unwrap().is_none(), "expected None stream");
        assert!(
            conn.is_shutting_down(),
            "connection should be marked as shutting down"
        );
    }

    #[tokio::test]
    async fn test_mark_shutdown_prevents_new_streams() {
        let (client_io, _server_io) = tokio::io::duplex(65536);
        let (send_req, _connection) = h2::client::handshake(client_io).await.unwrap();
        let (_closed_tx, closed_rx) = watch::channel(false);
        let ping_timeout = Arc::new(AtomicBool::new(false));
        let conn = ConnectionRef::new(send_req, closed_rx, ping_timeout, 0, 10, Digest::default());

        assert!(conn.more_streams_allowed());
        assert!(!conn.is_shutting_down());

        conn.mark_shutdown();

        assert!(conn.is_shutting_down());
        assert!(!conn.more_streams_allowed());
    }

    #[cfg(all(feature = "any_tls", unix))]
    #[tokio::test]
    async fn test_h2_reuse_rejects_fd_mismatch() {
        use crate::protocols::l4::socket::SocketAddr;
        use crate::upstreams::peer::Peer;
        use std::fmt::{Display, Formatter, Result as FmtResult};
        use std::os::unix::prelude::AsRawFd;

        #[derive(Clone)]
        struct MismatchPeer {
            reuse_hash: u64,
            address: SocketAddr,
        }

        impl Display for MismatchPeer {
            fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
                write!(f, "{:?}", self.address)
            }
        }

        impl Peer for MismatchPeer {
            fn address(&self) -> &SocketAddr {
                &self.address
            }

            fn tls(&self) -> bool {
                true
            }

            fn sni(&self) -> &str {
                ""
            }

            fn reuse_hash(&self) -> u64 {
                self.reuse_hash
            }

            fn matches_fd<V: AsRawFd>(&self, _fd: V) -> bool {
                false
            }
        }

        let connector = Connector::new(None);
        let mut peer = HttpPeer::new(("1.1.1.1", 443), true, "one.one.one.one".into());
        peer.options.set_http_version(2, 2);
        peer.options.max_h2_streams = 1;

        let h2 = connector
            .new_http_session::<HttpPeer, ()>(&peer)
            .await
            .unwrap();
        let h2_stream = match h2 {
            HttpSession::H1(_) => panic!("expect h2"),
            HttpSession::H2(h2_stream) => h2_stream,
            HttpSession::Custom(_) => panic!("expect h2"),
        };

        connector.release_http_session(h2_stream, &peer, None);

        let mismatch_peer = MismatchPeer {
            reuse_hash: peer.reuse_hash(),
            address: peer.address().clone(),
        };

        assert!(connector
            .reused_http_session(&mismatch_peer)
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn test_h2_handshake_settings_validation() {
        use super::H2HandshakeSettings;

        // Test zero stream window size is rejected
        let mut settings = H2HandshakeSettings::new();
        settings.max_streams = 100;
        settings.stream_window_size = Some(0);
        let (client, _server) = tokio::io::duplex(65536);
        match handshake(Box::new(client), settings).await {
            Err(e) => assert!(
                e.to_string()
                    .contains("stream_window_size must be between 1"),
                "Unexpected error: {}",
                e
            ),
            Ok(_) => panic!("Expected error for stream_window_size = 0"),
        }

        // Test zero connection window size is rejected
        let mut settings = H2HandshakeSettings::new();
        settings.max_streams = 100;
        settings.connection_window_size = Some(0);
        let (client, _server) = tokio::io::duplex(65536);
        match handshake(Box::new(client), settings).await {
            Err(e) => assert!(
                e.to_string()
                    .contains("connection_window_size must be between 1"),
                "Unexpected error: {}",
                e
            ),
            Ok(_) => panic!("Expected error for connection_window_size = 0"),
        }

        // Test exceeding max stream window size is rejected
        let mut settings = H2HandshakeSettings::new();
        settings.max_streams = 100;
        settings.stream_window_size = Some(super::H2_MAX_WINDOW_SIZE + 1);
        let (client, _server) = tokio::io::duplex(65536);
        match handshake(Box::new(client), settings).await {
            Err(e) => assert!(
                e.to_string()
                    .contains("stream_window_size must be between 1"),
                "Unexpected error: {}",
                e
            ),
            Ok(_) => panic!("Expected error for stream_window_size > max"),
        }

        // Test exceeding max connection window size is rejected
        let mut settings = H2HandshakeSettings::new();
        settings.max_streams = 100;
        settings.connection_window_size = Some(super::H2_MAX_WINDOW_SIZE + 1);
        let (client, _server) = tokio::io::duplex(65536);
        match handshake(Box::new(client), settings).await {
            Err(e) => assert!(
                e.to_string()
                    .contains("connection_window_size must be between 1"),
                "Unexpected error: {}",
                e
            ),
            Ok(_) => panic!("Expected error for connection_window_size > max"),
        }
    }

    #[tokio::test]
    async fn test_h2_handshake_custom_window_sizes() {
        // Test that valid custom window sizes are accepted and handshake succeeds
        let mut settings = H2HandshakeSettings::new();
        settings.max_streams = 100;
        settings.stream_window_size = Some(1 << 20); // 1MiB
        settings.connection_window_size = Some(1 << 24); // 16MiB

        let (client, server) = tokio::io::duplex(65536);

        // Spawn server side
        tokio::spawn(async move {
            let mut server_conn = h2::server::handshake(server).await.unwrap();
            if let Some(result) = server_conn.accept().await {
                let (_request, mut respond) = result.unwrap();
                let resp = Response::builder().status(StatusCode::OK).body(()).unwrap();
                let mut stream = respond.send_response(resp, false).unwrap();
                stream.send_data(Bytes::from("ok"), true).unwrap();
                server_conn.graceful_shutdown();
            }
            // Drive the server connection until the client closes
            while let Some(_res) = server_conn.accept().await {}
        });

        // Client side - should succeed with custom window sizes
        let conn = handshake(Box::new(client), settings).await.unwrap();

        // Verify we can spawn a stream and complete a request/response cycle
        let mut stream = conn.spawn_stream().await.unwrap().unwrap();
        let mut request = RequestHeader::build("GET", b"/", None).unwrap();
        request
            .insert_header(http::header::HOST, "example.com")
            .unwrap();
        stream
            .write_request_header(Box::new(request), true)
            .unwrap();

        stream.read_response_header().await.unwrap();
        assert_eq!(stream.response_header().unwrap().status, 200);
    }
}
