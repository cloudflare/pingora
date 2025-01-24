use super::HttpSession;

use crate::connectors::http::InUsePool;
use crate::connectors::{ConnectorOptions, TransportConnector};
use crate::protocols::http::v3::client::{Http3Poll, Http3Session};
use crate::protocols::http::v3::ConnectionIo;
use crate::protocols::l4::quic::{Connection, Crypto};
use crate::protocols::{Digest, Stream, UniqueID, UniqueIDType};
use crate::upstreams::peer::{Peer, ALPN};
use log::debug;
use parking_lot::Mutex;
use pingora_error::ErrorType::{H3Error, HandshakeError, InternalError};
use pingora_error::{Error, OrErr, Result};
use pingora_pool::{ConnectionMeta, ConnectionPool};
use quiche::h3::Event;
use quiche::ConnectionId;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
// FIXME: ConnectorOptions contains CA file path from ServerConfig

#[derive(Clone)]
pub(crate) struct ConnectionRef(Arc<ConnectionRefInner>);

impl ConnectionRef {
    pub fn new(
        l4_stream: Stream,
        conn_io: ConnectionIo,
        digest: Digest,
        add_sessions: Arc<Mutex<VecDeque<(u64, mpsc::Sender<Event>)>>>,
        drop_sessions: Arc<Mutex<VecDeque<u64>>>,
        idle_close: watch::Receiver<bool>,
        max_streams: usize,
        h3poll_task: JoinHandle<Result<()>>,
    ) -> Self {
        Self(Arc::new(ConnectionRefInner {
            l4_stream,
            conn_io,

            digest,
            max_streams,
            current_streams: AtomicUsize::new(0),
            release_lock: Arc::new(Default::default()),

            add_sessions,
            drop_sessions,
            idle_close,
            h3poll_task,
        }))
    }
}

impl ConnectionRef {
    pub(crate) fn conn_id(&self) -> &ConnectionId<'_> {
        &self.0.conn_io.conn_id
    }

    pub(crate) fn conn_io(&self) -> &ConnectionIo {
        &self.0.conn_io
    }

    pub(crate) fn digest(&self) -> &Digest {
        &self.0.digest
    }

    pub(crate) fn add_session(&self, stream_id: u64, tx: mpsc::Sender<Event>) {
        let mut add_sessions = self.0.add_sessions.lock();
        add_sessions.push_back((stream_id, tx))
    }

    pub(crate) fn drop_session(&self, stream_id: u64) {
        self.0.current_streams.fetch_sub(1, Ordering::SeqCst);
        let mut drop_sessions = self.0.drop_sessions.lock();
        drop_sessions.push_back(stream_id);
    }

    fn is_closed(&self) -> bool {
        *self.0.idle_close.borrow()
    }

    fn is_shutting_down(&self) -> bool {
        self.conn_io().is_shutting_down()
    }

    fn is_idle(&self) -> bool {
        self.0.current_streams.load(Ordering::Relaxed) == 0
    }

    pub(crate) fn release_stream(&self) {
        self.0.current_streams.fetch_sub(1, Ordering::SeqCst);
    }

    pub fn digest_mut(&mut self) -> Option<&mut Digest> {
        Arc::get_mut(&mut self.0).map(|inner| &mut inner.digest)
    }
}

pub(crate) struct ConnectionRefInner {
    // avoid dropping stream, & used for UniqueIDType
    l4_stream: Stream,

    // resources required for Http3, Quic & network IO
    conn_io: ConnectionIo,

    // connection digest
    digest: Digest,

    // max concurrent streams this connection is allowed to create
    max_streams: usize,

    // how many concurrent streams already active
    current_streams: AtomicUsize,

    // lock is used during moving the connection across pools
    release_lock: Arc<Mutex<()>>,

    // add session to active sessions in Http3Poll task
    add_sessions: Arc<Mutex<VecDeque<(u64, mpsc::Sender<Event>)>>>,
    // remove session from active sessions in Http3Poll task
    drop_sessions: Arc<Mutex<VecDeque<u64>>>,
    // watch for idle pool timeouts
    idle_close: watch::Receiver<bool>,

    h3poll_task: JoinHandle<Result<()>>,
}

impl Drop for ConnectionRefInner {
    fn drop(&mut self) {
        if !self.h3poll_task.is_finished() {
            self.h3poll_task.abort();
            debug!(
                "connection {:?} stopped Http3Poll task",
                self.conn_io.conn_id
            )
        }
    }
}

impl UniqueID for ConnectionRef {
    fn id(&self) -> UniqueIDType {
        self.0.l4_stream.id()
    }
}

/// Http3 connector
pub struct Connector {
    // for creating connections, the Stream for h3 should be reused
    transport: TransportConnector,
    // the h3 connection idle pool
    idle_pool: Arc<ConnectionPool<ConnectionRef>>,
    // the pool of h3 connections that have ongoing streams
    in_use_pool: InUsePool<ConnectionRef>,
    crypto: Option<Crypto>,
}

const DEFAULT_POOL_SIZE: usize = 128;

impl Connector {
    pub fn new(options: Option<ConnectorOptions>) -> Self {
        let pool_size = options
            .as_ref()
            .map_or(DEFAULT_POOL_SIZE, |o| o.keepalive_pool_size);

        // connection offload is handled by the [TransportConnector]
        Self {
            transport: TransportConnector::new(options),
            idle_pool: Arc::new(ConnectionPool::new(pool_size)),
            in_use_pool: InUsePool::new(),
            crypto: Crypto::new().ok(),
        }
    }

    /// Try to create a new http3 stream from any existing H3 connection.
    ///
    /// None means there is no "free" connection left.
    pub async fn reused_http_session<P: Peer + Send + Sync + 'static>(
        &self,
        peer: &P,
    ) -> Result<Option<Http3Session>> {
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
            // lock the connection before adding a stream
            // ensures that moving between pools and e.g. idle() checks is guarded
            let _release_lock = conn.0.release_lock.lock_arc();
            let h3_stream = conn.spawn_stream().await?;
            if conn.more_streams_allowed() {
                self.in_use_pool.insert(reuse_hash, conn);
            }
            Ok(h3_stream)
        } else {
            Ok(None)
        }
    }

    /// Release a finished h3 stream.
    ///
    /// This function will terminate the [Http3Session]. The corresponding h3 connection will now
    /// have one more free stream to use.
    ///
    /// The h3 connection will be closed after `idle_timeout` if it has no active streams.
    pub fn release_http_session<P: Peer + Send + Sync + 'static>(
        &self,
        session: Http3Session,
        peer: &P,
        idle_timeout: Option<Duration>,
    ) {
        let id = session.conn().id();
        let reuse_hash = peer.reuse_hash();
        // get a ref to the connection, which we might need below, before dropping the h3
        let conn = session.conn();

        // The lock here is to make sure that in_use_pool.insert() below cannot be called after
        // in_use_pool.release(), which would have put the conn entry in both pools.
        // It also makes sure that only one conn will trigger the conn.is_idle() condition, which
        // avoids putting the same conn into the idle_pool more than once.
        let locked = conn.0.release_lock.lock_arc();
        // TODO: should a stream_reset be called during drop?
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
            let meta = ConnectionMeta {
                key: reuse_hash,
                id,
            };
            let idle_closed = conn.0.idle_close.clone();
            let (notify_evicted, watch_use) = self.idle_pool.put(&meta, conn);
            drop(locked);
            if let Some(to) = idle_timeout {
                let pool = self.idle_pool.clone(); // clone the arc
                let rt = pingora_runtime::current_handle();
                rt.spawn(async move {
                    pool.idle_timeout(&meta, to, notify_evicted, idle_closed, watch_use)
                        .await;
                });
            }
        } else {
            self.in_use_pool.insert(reuse_hash, conn);
            drop(locked);
        }
    }

    /// Create a new Http3 connection to the given server
    pub async fn new_http_session<P: Peer + Send + Sync + 'static>(
        &self,
        peer: &P,
    ) -> Result<HttpSession> {
        let stream = self.transport.new_stream(peer).await?;

        // check alpn
        match stream.selected_alpn_proto() {
            Some(ALPN::H3) => { /* continue */ }
            _ => return Err(Error::explain(InternalError, "peer ALPN is not H3")),
        }

        let max_h3_stream = peer.get_peer_options().map_or(1, |o| o.max_h3_streams);
        let conn = handshake(stream, max_h3_stream).await?;

        let h3_stream = conn
            .spawn_stream()
            .await?
            .expect("newly created connections should have at least one free stream");

        if conn.more_streams_allowed() {
            self.in_use_pool.insert(peer.reuse_hash(), conn);
        }

        Ok(HttpSession::H3(h3_stream))
    }
}

impl ConnectionRef {
    // spawn a stream if more stream is allowed, otherwise return Ok(None)
    pub async fn spawn_stream(&self) -> Result<Option<Http3Session>> {
        // Atomically check if the current_stream is over the limit
        // load(), compare and then fetch_add() cannot guarantee the same
        let current_streams = self.0.current_streams.fetch_add(1, Ordering::SeqCst);
        if current_streams >= self.0.max_streams {
            // already over the limit, reset the counter to the previous value
            self.0.current_streams.fetch_sub(1, Ordering::SeqCst);
            return Ok(None);
        }

        let h3_session = Http3Session::new(self.clone())?;
        Ok(Some(h3_session))
    }

    pub fn more_streams_allowed(&self) -> bool {
        let qconn = self.0.conn_io.quic.lock();
        qconn.is_established()
            && !qconn.is_closed()
            && !qconn.is_draining()
            && qconn.peer_streams_left_bidi() > 0
    }
}

async fn handshake(mut stream: Stream, max_streams: usize) -> Result<ConnectionRef> {
    // Safe guard: new_http_session() assumes there should be at least one free stream
    if max_streams == 0 {
        return Error::e_explain(H3Error, "zero max_stream configured");
    }

    let digest = Digest {
        // NOTE: this field is always false because the digest is shared across all streams
        // The streams should log their own reuse info
        ssl_digest: stream.get_ssl_digest(),
        // TODO: log h3 handshake time
        timing_digest: stream.get_timing_digest(),
        proxy_digest: stream.get_proxy_digest(),
        socket_digest: stream.get_socket_digest(),
    };
    let Some(quic_state) = stream.quic_connection_state() else {
        return Err(Error::explain(InternalError, "stream is not a Quic stream"));
    };

    let conn_io = match quic_state {
        Connection::IncomingHandshake(_)
        | Connection::IncomingEstablished(_)
        | Connection::OutgoingHandshake(_) => {
            return Err(Error::explain(InternalError, "invalid Quic stream state"))
        }
        Connection::OutgoingEstablished(e_state) => {
            let hconn = {
                let mut conn = e_state.connection.lock();
                quiche::h3::Connection::with_transport(&mut conn, &e_state.http3_config)
                    .explain_err(HandshakeError, |_| "during H3 handshake")
            }?;
            e_state.tx_notify.notify_waiters();

            ConnectionIo {
                conn_id: e_state.connection_id.clone(),
                quic: e_state.connection.clone(),
                http3: Arc::new(Mutex::new(hconn)),
                rx_notify: e_state.rx_notify.clone(),
                tx_notify: e_state.tx_notify.clone(),
            }
        }
    };
    debug!("H3 handshake to server done.");

    let add_sessions = Arc::new(Mutex::new(VecDeque::default()));
    let drop_sessions = Arc::new(Mutex::new(VecDeque::default()));
    let (idle_close_tx, idle_close_rx) = watch::channel::<bool>(false);

    let h3poll = Http3Poll {
        conn_io: conn_io.clone(),
        sessions: Default::default(),
        add_sessions: add_sessions.clone(),
        drop_sessions: drop_sessions.clone(),
        idle_close: idle_close_tx,
    };
    let h3poll_task = pingora_runtime::current_handle().spawn(h3poll.start());

    Ok(ConnectionRef::new(
        stream,
        conn_io,
        digest,
        add_sessions,
        drop_sessions,
        idle_close_rx,
        max_streams,
        h3poll_task,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connectors::quic_tests::quic_listener_peer;
    use crate::protocols::l4::quic::MAX_IPV6_QUIC_DATAGRAM_SIZE;
    use crate::upstreams::peer::HttpPeer;
    use bytes::{BufMut, BytesMut};
    use http::Version;
    use pingora_error::Result;
    use pingora_http::RequestHeader;

    #[tokio::test]
    async fn test_listener_connector_quic_http3() -> Result<()> {
        let (_server_handle, peer) = quic_listener_peer()?;

        let connector = Connector::new(None);
        let mut session = connector.new_http_session(&peer).await?;

        let mut req = RequestHeader::build("GET", b"/", Some(3))?;
        req.insert_header(http::header::HOST, "openresty.org")?;

        let body_base = "hello world\n";
        let body_string = body_base.repeat(MAX_IPV6_QUIC_DATAGRAM_SIZE * 128 / body_base.len());
        let mut body_send = BytesMut::new();
        body_send.put(body_string.as_bytes());

        session.write_request_header(Box::new(req)).await?;
        session
            .write_request_body(body_send.freeze(), false)
            .await?;
        session.finish_request_body().await?;
        session.read_response_header().await?;

        let resp = session.response_header();
        assert!(resp.is_some());
        if let Some(resp) = resp {
            assert_eq!(resp.status.as_str(), "200");
            assert_eq!(resp.version, Version::HTTP_3);
        }

        let mut resp_body = BytesMut::new();
        while let Some(body) = session.read_response_body().await? {
            assert!(body.len() < MAX_IPV6_QUIC_DATAGRAM_SIZE * 64);
            resp_body.put(body)
        }
        assert_eq!(resp_body.as_ref(), body_string.as_bytes());
        Ok(())
    }

    #[tokio::test]
    #[cfg(feature = "any_tls")]
    async fn test_connect_h3() {
        let connector = Connector::new(None);
        let mut peer = HttpPeer::new(("1.1.1.1", 443), true, "one.one.one.one".into());
        peer.options.set_http_version(3, 3);
        let h3 = connector.new_http_session(&peer).await.unwrap();
        match h3 {
            HttpSession::H1(_) | HttpSession::H2(_) => panic!("expect h3"),
            HttpSession::H3(_h3_session) => assert!(true),
        }
    }

    #[tokio::test]
    #[cfg(feature = "any_tls")]
    async fn test_h3_single_stream() {
        let connector = Connector::new(None);
        let mut peer = HttpPeer::new(("1.1.1.1", 443), true, "one.one.one.one".into());
        peer.options.set_http_version(3, 3);
        peer.options.max_h3_streams = 1;
        let h3 = connector.new_http_session(&peer).await.unwrap();
        let h3_1 = match h3 {
            HttpSession::H3(h3_stream) => h3_stream,
            _ => panic!("expect h3"),
        };

        let id = h3_1.conn().id();

        assert!(connector
            .reused_http_session(&peer)
            .await
            .unwrap()
            .is_none());

        connector.release_http_session(h3_1, &peer, None);

        let h3_2 = connector.reused_http_session(&peer).await.unwrap().unwrap();
        assert_eq!(id, h3_2.conn().id());

        connector.release_http_session(h3_2, &peer, None);

        let h3_3 = connector.reused_http_session(&peer).await.unwrap().unwrap();
        assert_eq!(id, h3_3.conn().id());
    }

    #[tokio::test]
    #[cfg(feature = "any_tls")]
    async fn test_h3_multiple_stream() {
        let connector = Connector::new(None);
        let mut peer = HttpPeer::new(("1.1.1.1", 443), true, "one.one.one.one".into());
        peer.options.set_http_version(3, 3);
        peer.options.max_h3_streams = 3;
        let h3 = connector.new_http_session(&peer).await.unwrap();
        let h3_1 = match h3 {
            HttpSession::H3(h3_stream) => h3_stream,
            _ => panic!("expect h3"),
        };

        let id = h3_1.conn().id();

        let h3_2 = connector.reused_http_session(&peer).await.unwrap().unwrap();
        assert_eq!(id, h3_2.conn().id());
        let h3_3 = connector.reused_http_session(&peer).await.unwrap().unwrap();
        assert_eq!(id, h3_3.conn().id());

        // max stream is 3 for now
        assert!(connector
            .reused_http_session(&peer)
            .await
            .unwrap()
            .is_none());

        connector.release_http_session(h3_1, &peer, None);

        let h3_4 = connector.reused_http_session(&peer).await.unwrap().unwrap();
        assert_eq!(id, h3_4.conn().id());

        connector.release_http_session(h3_2, &peer, None);
        connector.release_http_session(h3_3, &peer, None);
        connector.release_http_session(h3_4, &peer, None);

        // all streams are released, now the connection is idle
        let h3_5 = connector.reused_http_session(&peer).await.unwrap().unwrap();
        assert_eq!(id, h3_5.conn().id());
    }
}
