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

use super::HttpSession;
use crate::connectors::{ConnectorOptions, TransportConnector};
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
pub(crate) struct ConnectionRef(Arc<ConnectionRefInner>);

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
        !self.is_shutting_down()
            && self.0.max_streams > self.0.current_streams.load(Ordering::Relaxed)
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
                // Remote sends GOAWAY(NO_ERROR): graceful shutdown: this connection no longer
                // accepts new streams. We can still try to create new connection.
                if e.root_cause()
                    .downcast_ref::<h2::Error>()
                    .map(|e| {
                        e.is_go_away() && e.is_remote() && e.reason() == Some(h2::Reason::NO_ERROR)
                    })
                    .unwrap_or(false)
                {
                    self.0.shutting_down.store(true, Ordering::Relaxed);
                    Ok(None)
                } else {
                    Err(e)
                }
            }
        }
    }
}

struct InUsePool {
    // TODO: use pingora hashmap to shard the lock contention
    pools: RwLock<HashMap<u64, PoolNode<ConnectionRef>>>,
}

impl InUsePool {
    fn new() -> Self {
        InUsePool {
            pools: RwLock::new(HashMap::new()),
        }
    }

    fn insert(&self, reuse_hash: u64, conn: ConnectionRef) {
        {
            let pools = self.pools.read();
            if let Some(pool) = pools.get(&reuse_hash) {
                pool.insert(conn.id(), conn);
                return;
            }
        } // drop read lock

        let pool = PoolNode::new();
        pool.insert(conn.id(), conn);
        let mut pools = self.pools.write();
        pools.insert(reuse_hash, pool);
    }

    // retrieve a h2 conn ref to create a new stream
    // the caller should return the conn ref to this pool if there are still
    // capacity left for more streams
    fn get(&self, reuse_hash: u64) -> Option<ConnectionRef> {
        let pools = self.pools.read();
        pools.get(&reuse_hash)?.get_any().map(|v| v.1)
    }

    // release a h2_stream, this functional will cause an ConnectionRef to be returned (if exist)
    // the caller should update the ref and then decide where to put it (in use pool or idle)
    fn release(&self, reuse_hash: u64, id: UniqueIDType) -> Option<ConnectionRef> {
        let pools = self.pools.read();
        if let Some(pool) = pools.get(&reuse_hash) {
            pool.remove(id)
        } else {
            None
        }
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

    /// Create a new Http2 connection to the given server
    ///
    /// Either an Http2 or Http1 session can be returned depending on the server's preference.
    pub async fn new_http_session<P: Peer + Send + Sync + 'static>(
        &self,
        peer: &P,
    ) -> Result<HttpSession> {
        let stream = self.transport.new_stream(peer).await?;

        // check alpn
        match stream.selected_alpn_proto() {
            Some(ALPN::H2) => { /* continue */ }
            Some(_) => {
                // H2 not supported
                return Ok(HttpSession::H1(Http1Session::new(stream)));
            }
            None => {
                // if tls but no ALPN, default to h1
                // else if plaintext and min http version is 1, this is most likely h1
                if peer.tls()
                    || peer
                        .get_peer_options()
                        .map_or(true, |o| o.alpn.get_min_http_version() == 1)
                {
                    return Ok(HttpSession::H1(Http1Session::new(stream)));
                }
                // else: min http version=H2 over plaintext, there is no ALPN anyways, we trust
                // the caller that the server speaks h2c
            }
        }
        let max_h2_stream = peer.get_peer_options().map_or(1, |o| o.max_h2_streams);
        let conn = handshake(stream, max_h2_stream, peer.h2_ping_interval()).await?;
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
            .or_else(|| self.idle_pool.get(&reuse_hash));
        if let Some(conn) = maybe_conn {
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
            if let Some(to) = idle_timeout {
                let pool = self.idle_pool.clone(); //clone the arc
                let rt = pingora_runtime::current_handle();
                rt.spawn(async move {
                    pool.idle_timeout(&meta, to, notify_evicted, closed, watch_use)
                        .await;
                });
            }
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
            .map_or(false, |v| matches!(v, ALPN::H1))
    }
}

// The h2 library we use has unbounded internal buffering, which will cause excessive memory
// consumption when the downstream is slower than upstream. This window size caps the buffering by
// limiting how much data can be inflight. However, setting this value will also cap the max
// download speed by limiting the bandwidth-delay product of a link.
// Long term, we should advertising large window but shrink it when a small buffer is full.
// 8 Mbytes = 80 Mbytes X 100ms, which should be enough for most links.
const H2_WINDOW_SIZE: u32 = 1 << 23;

async fn handshake(
    stream: Stream,
    max_streams: usize,
    h2_ping_interval: Option<Duration>,
) -> Result<ConnectionRef> {
    use h2::client::Builder;
    use pingora_runtime::current_handle;

    // Safe guard: new_http_session() assumes there should be at least one free stream
    if max_streams == 0 {
        return Error::e_explain(H2Error, "zero max_stream configured");
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
    // TODO: make these configurable
    let (send_req, connection) = Builder::new()
        .enable_push(false)
        .initial_max_send_streams(max_streams)
        // The limit for the server. Server push is not allowed, so this value doesn't matter
        .max_concurrent_streams(1)
        .max_frame_size(64 * 1024) // advise server to send larger frames
        .initial_window_size(H2_WINDOW_SIZE)
        // should this be max_streams * H2_WINDOW_SIZE?
        .initial_connection_window_size(H2_WINDOW_SIZE)
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
            h2_ping_interval,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::upstreams::peer::HttpPeer;

    #[tokio::test]
    async fn test_connect_h2() {
        let connector = Connector::new(None);
        let mut peer = HttpPeer::new(("1.1.1.1", 443), true, "one.one.one.one".into());
        peer.options.set_http_version(2, 2);
        let h2 = connector.new_http_session(&peer).await.unwrap();
        match h2 {
            HttpSession::H1(_) => panic!("expect h2"),
            HttpSession::H2(h2_stream) => assert!(!h2_stream.ping_timedout()),
        }
    }

    #[tokio::test]
    async fn test_connect_h1() {
        let connector = Connector::new(None);
        let mut peer = HttpPeer::new(("1.1.1.1", 443), true, "one.one.one.one".into());
        // a hack to force h1, new_http_session() in the future might validate this setting
        peer.options.set_http_version(1, 1);
        let h2 = connector.new_http_session(&peer).await.unwrap();
        match h2 {
            HttpSession::H1(_) => {}
            HttpSession::H2(_) => panic!("expect h1"),
        }
    }

    #[tokio::test]
    async fn test_connect_h1_plaintext() {
        let connector = Connector::new(None);
        let mut peer = HttpPeer::new(("1.1.1.1", 80), false, "".into());
        peer.options.set_http_version(2, 1);
        let h2 = connector.new_http_session(&peer).await.unwrap();
        match h2 {
            HttpSession::H1(_) => {}
            HttpSession::H2(_) => panic!("expect h1"),
        }
    }

    #[tokio::test]
    async fn test_h2_single_stream() {
        let connector = Connector::new(None);
        let mut peer = HttpPeer::new(("1.1.1.1", 443), true, "one.one.one.one".into());
        peer.options.set_http_version(2, 2);
        peer.options.max_h2_streams = 1;
        let h2 = connector.new_http_session(&peer).await.unwrap();
        let h2_1 = match h2 {
            HttpSession::H1(_) => panic!("expect h2"),
            HttpSession::H2(h2_stream) => h2_stream,
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
    async fn test_h2_multiple_stream() {
        let connector = Connector::new(None);
        let mut peer = HttpPeer::new(("1.1.1.1", 443), true, "one.one.one.one".into());
        peer.options.set_http_version(2, 2);
        peer.options.max_h2_streams = 3;
        let h2 = connector.new_http_session(&peer).await.unwrap();
        let h2_1 = match h2 {
            HttpSession::H1(_) => panic!("expect h2"),
            HttpSession::H2(h2_stream) => h2_stream,
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
}
