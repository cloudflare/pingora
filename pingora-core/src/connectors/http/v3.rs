// FIXME: implement request spawning
// ConnectorOptions contains CA file path from ServerConfig

use crate::connectors::http::v2::{ConnectionRef, InUsePool};
use crate::connectors::{ConnectorOptions, TransportConnector};
use crate::protocols::http::v2::client::Http2Session;
use crate::protocols::http::v3::client::Http3Session;
use crate::upstreams::peer::Peer;
use pingora_pool::ConnectionPool;
use std::sync::Arc;
use std::time::Duration;

/// Http3 connector
pub struct Connector {
    // just for creating connections, the Stream of h2 should be reused
    transport: TransportConnector,
    // the h2 connection idle pool
    //idle_pool: Arc<ConnectionPool<ConnectionRef>>,
    // the pool of h2 connections that have ongoing streams
    //in_use_pool: crate::connectors::http::v2::InUsePool,
    in_use_pool: InUsePool,
    // the h3 connection idle pool
    idle_pool: Arc<ConnectionPool<ConnectionRef>>,
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
        }
    }

    /// Try to create a new http3 stream from any existing H3 connection.
    ///
    /// None means there is no "free" connection left.
    pub async fn reused_http_session<P: Peer + Send + Sync + 'static>(
        &self,
        peer: &P,
    ) -> pingora_error::Result<Option<Http2Session>> {
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
            // FIXME: fix types, ConnectionRef = H2 only
            let h2_stream = conn.spawn_stream().await?;
            if conn.more_streams_allowed() {
                self.in_use_pool.insert(reuse_hash, conn);
            }
            Ok(h2_stream)
        } else {
            Ok(None)
        }
    }

    /// Release a finished h3 stream.
    ///
    /// This function will terminate the [Http3Session]. The corresponding h3 connection will now
    /// have one more free stream to use.
    ///
    /// The h2 connection will be closed after `idle_timeout` if it has no active streams.
    pub fn release_http_session<P: Peer + Send + Sync + 'static>(
        &self,
        session: Http3Session,
        peer: &P,
        idle_timeout: Option<Duration>,
    ) {
        todo!()
    }
}
