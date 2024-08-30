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

//! Generic connection pooling

use log::{debug, warn};
use parking_lot::{Mutex, RwLock};
use pingora_timeout::{sleep, timeout};
use std::collections::HashMap;
use std::io;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::sync::{oneshot, watch, Notify, OwnedMutexGuard};

use super::lru::Lru;

type GroupKey = u64;
#[cfg(unix)]
type ID = i32;
#[cfg(windows)]
type ID = usize;

/// the metadata of a connection
#[derive(Clone, Debug)]
pub struct ConnectionMeta {
    /// The group key. All connections under the same key are considered the same for connection reuse.
    pub key: GroupKey,
    /// The unique ID of a connection.
    pub id: ID,
}

impl ConnectionMeta {
    /// Create a new [ConnectionMeta]
    pub fn new(key: GroupKey, id: ID) -> Self {
        ConnectionMeta { key, id }
    }
}

struct PoolConnection<S> {
    pub notify_use: oneshot::Sender<bool>,
    pub connection: S,
}

impl<S> PoolConnection<S> {
    pub fn new(notify_use: oneshot::Sender<bool>, connection: S) -> Self {
        PoolConnection {
            notify_use,
            connection,
        }
    }

    pub fn release(self) -> S {
        // notify the idle watcher to release the connection
        let _ = self.notify_use.send(true);
        // wait for the watcher to release
        self.connection
    }
}

use crossbeam_queue::ArrayQueue;

/// A pool of exchangeable items
pub struct PoolNode<T> {
    connections: Mutex<HashMap<ID, T>>,
    // a small lock free queue to avoid lock contention
    hot_queue: ArrayQueue<(ID, T)>,
    // to avoid race between 2 evictions on the queue
    hot_queue_remove_lock: Mutex<()>,
    // TODO: store the GroupKey to avoid hash collision?
}

// Keep the queue size small because eviction is O(n) in the queue
const HOT_QUEUE_SIZE: usize = 16;

impl<T> PoolNode<T> {
    /// Create a new [PoolNode]
    pub fn new() -> Self {
        PoolNode {
            connections: Mutex::new(HashMap::new()),
            hot_queue: ArrayQueue::new(HOT_QUEUE_SIZE),
            hot_queue_remove_lock: Mutex::new(()),
        }
    }

    /// Get any item from the pool
    pub fn get_any(&self) -> Option<(ID, T)> {
        let hot_conn = self.hot_queue.pop();
        if hot_conn.is_some() {
            return hot_conn;
        }
        let mut connections = self.connections.lock();
        // find one connection, any connection will do
        let id = match connections.iter().next() {
            Some((k, _)) => *k, // OK to copy i32
            None => return None,
        };
        // unwrap is safe since we just found it
        let connection = connections.remove(&id).unwrap();
        /* NOTE: we don't resize or drop empty connections hashmap
         * We may want to do it if they consume too much memory
         * maybe we should use trees to save memory */
        Some((id, connection))
        // connections.lock released here
    }

    /// Insert an item with the given unique ID into the pool
    pub fn insert(&self, id: ID, conn: T) {
        if let Err(node) = self.hot_queue.push((id, conn)) {
            // hot queue is full
            let mut connections = self.connections.lock();
            connections.insert(node.0, node.1); // TODO: check dup
        }
    }

    // This function acquires 2 locks and iterates over the entire hot queue.
    // But it should be fine because remove() rarely happens on a busy PoolNode.
    /// Remove the item associated with the id from the pool. The item is returned
    /// if it is found and removed.
    pub fn remove(&self, id: ID) -> Option<T> {
        // check the table first as least recent used ones are likely there
        let removed = self.connections.lock().remove(&id);
        if removed.is_some() {
            return removed;
        } // lock drops here

        let _queue_lock = self.hot_queue_remove_lock.lock();
        // check the hot queue, note that the queue can be accessed in parallel by insert and get
        let max_len = self.hot_queue.len();
        for _ in 0..max_len {
            if let Some((conn_id, conn)) = self.hot_queue.pop() {
                if conn_id == id {
                    // this is the item, it is already popped
                    return Some(conn);
                } else {
                    // not this item, put back to hot queue, but it could also be full
                    self.insert(conn_id, conn);
                }
            } else {
                // other threads grab all the connections
                return None;
            }
        }
        None
        // _queue_lock drops here
    }
}

/// Connection pool
///
/// [ConnectionPool] holds reusable connections. A reusable connection is released to this pool to
/// be picked up by another user/request.
pub struct ConnectionPool<S> {
    // TODO: n-way pools to reduce lock contention
    pool: RwLock<HashMap<GroupKey, Arc<PoolNode<PoolConnection<S>>>>>,
    lru: Lru<ID, ConnectionMeta>,
}

impl<S> ConnectionPool<S> {
    /// Create a new [ConnectionPool] with a size limit.
    ///
    /// When a connection is released to this pool, the least recently used connection will be dropped.
    pub fn new(size: usize) -> Self {
        ConnectionPool {
            pool: RwLock::new(HashMap::with_capacity(size)), // this is oversized since some connections will have the same key
            lru: Lru::new(size),
        }
    }

    /* get or create and insert a pool node for the hash key */
    fn get_pool_node(&self, key: GroupKey) -> Arc<PoolNode<PoolConnection<S>>> {
        {
            let pool = self.pool.read();
            if let Some(v) = pool.get(&key) {
                return (*v).clone();
            }
        } // read lock released here

        {
            // write lock section
            let mut pool = self.pool.write();
            // check again since another task might have already added it
            if let Some(v) = pool.get(&key) {
                return (*v).clone();
            }
            let node = Arc::new(PoolNode::new());
            let node_ret = node.clone();
            pool.insert(key, node); // TODO: check dup
            node_ret
        }
    }

    // only remove from the pool because lru already removed it
    fn pop_evicted(&self, meta: &ConnectionMeta) {
        let pool_node = {
            let pool = self.pool.read();
            match pool.get(&meta.key) {
                Some(v) => (*v).clone(),
                None => {
                    warn!("Fail to get pool node for {:?}", meta);
                    return;
                } // nothing to pop, should return error?
            }
        }; // read lock released here

        pool_node.remove(meta.id);
        debug!("evict fd: {} from key {}", meta.id, meta.key);
    }

    pub fn pop_closed(&self, meta: &ConnectionMeta) {
        // NOTE: which of these should be done first?
        self.pop_evicted(meta);
        self.lru.pop(&meta.id);
    }

    /// Get a connection from this pool under the same group key
    pub fn get(&self, key: &GroupKey) -> Option<S> {
        let pool_node = {
            let pool = self.pool.read();
            match pool.get(key) {
                Some(v) => (*v).clone(),
                None => return None,
            }
        }; // read lock released here

        if let Some((id, connection)) = pool_node.get_any() {
            self.lru.pop(&id); // the notified is not needed
            Some(connection.release())
        } else {
            None
        }
    }

    /// Release a connection to this pool for reuse
    ///
    /// - The returned [`Arc<Notify>`] will notify any listen when the connection is evicted from the pool.
    /// - The returned [`oneshot::Receiver<bool>`] will notify when the connection is being picked up by [Self::get()].
    pub fn put(
        &self,
        meta: &ConnectionMeta,
        connection: S,
    ) -> (Arc<Notify>, oneshot::Receiver<bool>) {
        let (notify_close, replaced) = self.lru.add(meta.id, meta.clone());
        if let Some(meta) = replaced {
            self.pop_evicted(&meta);
        };
        let pool_node = self.get_pool_node(meta.key);
        let (notify_use, watch_use) = oneshot::channel();
        let connection = PoolConnection::new(notify_use, connection);
        pool_node.insert(meta.id, connection);
        (notify_close, watch_use)
    }

    /// Actively monitor the health of a connection that is already released to this pool
    ///
    /// When the connection breaks, or the optional `timeout` is reached this function will
    /// remove it from the pool and drop the connection.
    ///
    /// If the connection is reused via [Self::get()] or being evicted, this function will just exit.
    pub async fn idle_poll<Stream>(
        &self,
        connection: OwnedMutexGuard<Stream>,
        meta: &ConnectionMeta,
        timeout: Option<Duration>,
        notify_evicted: Arc<Notify>,
        watch_use: oneshot::Receiver<bool>,
    ) where
        Stream: AsyncRead + Unpin + Send,
    {
        let read_result = tokio::select! {
            biased;
            _ = watch_use => {
                debug!("idle connection is being picked up");
                return
            },
            _ = notify_evicted.notified() => {
                debug!("idle connection is being evicted");
                // TODO: gracefully close the connection?
                return
            }
            read_result = read_with_timeout(connection , timeout) => read_result
        };

        match read_result {
            Ok(n) => {
                if n > 0 {
                    warn!("Data received on idle client connection, close it")
                } else {
                    debug!("Peer closed the idle connection or timeout")
                }
            }

            Err(e) => {
                debug!("error with the idle connection, close it {:?}", e);
            }
        }
        // connection terminated from either peer or timer
        self.pop_closed(meta);
    }

    /// Passively wait to close the connection after the timeout
    ///
    /// If this connection is not being picked up or evicted before the timeout is reach, this
    /// function will remove it from the pool and close the connection.
    pub async fn idle_timeout(
        &self,
        meta: &ConnectionMeta,
        timeout: Duration,
        notify_evicted: Arc<Notify>,
        mut notify_closed: watch::Receiver<bool>,
        watch_use: oneshot::Receiver<bool>,
    ) {
        tokio::select! {
            biased;
            _ = watch_use => {
                debug!("idle connection is being picked up");
            },
            _ = notify_evicted.notified() => {
                debug!("idle connection is being evicted");
                // TODO: gracefully close the connection?
            }
            _ = notify_closed.changed() => {
                // assume always changed from false to true
                debug!("idle connection is being closed");
                self.pop_closed(meta);
            }
            _ = sleep(timeout) => {
                debug!("idle connection is being evicted");
                self.pop_closed(meta);
            }
        };
    }
}

async fn read_with_timeout<S>(
    mut connection: OwnedMutexGuard<S>,
    timeout_duration: Option<Duration>,
) -> io::Result<usize>
where
    S: AsyncRead + Unpin + Send,
{
    let mut buf = [0; 1];
    let read_event = connection.read(&mut buf[..]);
    match timeout_duration {
        Some(d) => match timeout(d, read_event).await {
            Ok(res) => res,
            Err(e) => {
                debug!("keepalive timeout {:?} reached, {:?}", d, e);
                Ok(0)
            }
        },
        _ => read_event.await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use log::debug;
    use tokio::sync::Mutex as AsyncMutex;
    use tokio_test::io::{Builder, Mock};

    #[tokio::test]
    async fn test_lookup() {
        let meta1 = ConnectionMeta::new(101, 1);
        let value1 = "v1".to_string();
        let meta2 = ConnectionMeta::new(102, 2);
        let value2 = "v2".to_string();
        let meta3 = ConnectionMeta::new(101, 3);
        let value3 = "v3".to_string();
        let cp: ConnectionPool<String> = ConnectionPool::new(3); //#CP3
        cp.put(&meta1, value1.clone());
        cp.put(&meta2, value2.clone());
        cp.put(&meta3, value3.clone());

        let found_b = cp.get(&meta2.key).unwrap();
        assert_eq!(found_b, value2);

        let found_a1 = cp.get(&meta1.key).unwrap();
        let found_a2 = cp.get(&meta1.key).unwrap();

        assert!(
            found_a1 == value1 && found_a2 == value3 || found_a2 == value1 && found_a1 == value3
        );
    }

    #[tokio::test]
    async fn test_pop() {
        let meta1 = ConnectionMeta::new(101, 1);
        let value1 = "v1".to_string();
        let meta2 = ConnectionMeta::new(102, 2);
        let value2 = "v2".to_string();
        let meta3 = ConnectionMeta::new(101, 3);
        let value3 = "v3".to_string();
        let cp: ConnectionPool<String> = ConnectionPool::new(3); //#CP3
        cp.put(&meta1, value1);
        cp.put(&meta2, value2);
        cp.put(&meta3, value3.clone());

        cp.pop_closed(&meta1);

        let found_a1 = cp.get(&meta1.key).unwrap();
        assert_eq!(found_a1, value3);

        cp.pop_closed(&meta1);
        assert!(cp.get(&meta1.key).is_none())
    }

    #[tokio::test]
    async fn test_eviction() {
        let meta1 = ConnectionMeta::new(101, 1);
        let value1 = "v1".to_string();
        let meta2 = ConnectionMeta::new(102, 2);
        let value2 = "v2".to_string();
        let meta3 = ConnectionMeta::new(101, 3);
        let value3 = "v3".to_string();
        let cp: ConnectionPool<String> = ConnectionPool::new(2);
        let (notify_close1, _) = cp.put(&meta1, value1.clone());
        let (notify_close2, _) = cp.put(&meta2, value2.clone());
        let (notify_close3, _) = cp.put(&meta3, value3.clone()); // meta 1 should be evicted

        let closed_item = tokio::select! {
            _ = notify_close1.notified() => {debug!("notifier1"); 1},
            _ = notify_close2.notified() => {debug!("notifier2"); 2},
            _ = notify_close3.notified() => {debug!("notifier3"); 3},
        };
        assert_eq!(closed_item, 1);

        let found_a1 = cp.get(&meta1.key).unwrap();
        assert_eq!(found_a1, value3);
        assert_eq!(cp.get(&meta1.key), None)
    }

    #[tokio::test]
    #[should_panic(expected = "There is still data left to read.")]
    async fn test_read_close() {
        let meta1 = ConnectionMeta::new(101, 1);
        let mock_io1 = Arc::new(AsyncMutex::new(Builder::new().read(b"garbage").build()));
        let meta2 = ConnectionMeta::new(102, 2);
        let mock_io2 = Arc::new(AsyncMutex::new(
            Builder::new().wait(Duration::from_secs(99)).build(),
        ));
        let meta3 = ConnectionMeta::new(101, 3);
        let mock_io3 = Arc::new(AsyncMutex::new(
            Builder::new().wait(Duration::from_secs(99)).build(),
        ));
        let cp: ConnectionPool<Arc<AsyncMutex<Mock>>> = ConnectionPool::new(3);
        let (c1, u1) = cp.put(&meta1, mock_io1.clone());
        let (c2, u2) = cp.put(&meta2, mock_io2.clone());
        let (c3, u3) = cp.put(&meta3, mock_io3.clone());

        let closed_item = tokio::select! {
            _ = cp.idle_poll(mock_io1.try_lock_owned().unwrap(), &meta1, None, c1, u1) => {debug!("notifier1"); 1},
            _ = cp.idle_poll(mock_io2.try_lock_owned().unwrap(), &meta1, None, c2, u2) => {debug!("notifier2"); 2},
            _ = cp.idle_poll(mock_io3.try_lock_owned().unwrap(), &meta1, None, c3, u3) => {debug!("notifier3"); 3},
        };
        assert_eq!(closed_item, 1);

        let _ = cp.get(&meta1.key).unwrap(); // mock_io3 should be selected
        assert!(cp.get(&meta1.key).is_none()) // mock_io1 should already be removed by idle_poll
    }

    #[tokio::test]
    async fn test_read_timeout() {
        let meta1 = ConnectionMeta::new(101, 1);
        let mock_io1 = Arc::new(AsyncMutex::new(
            Builder::new().wait(Duration::from_secs(99)).build(),
        ));
        let meta2 = ConnectionMeta::new(102, 2);
        let mock_io2 = Arc::new(AsyncMutex::new(
            Builder::new().wait(Duration::from_secs(99)).build(),
        ));
        let meta3 = ConnectionMeta::new(101, 3);
        let mock_io3 = Arc::new(AsyncMutex::new(
            Builder::new().wait(Duration::from_secs(99)).build(),
        ));
        let cp: ConnectionPool<Arc<AsyncMutex<Mock>>> = ConnectionPool::new(3);
        let (c1, u1) = cp.put(&meta1, mock_io1.clone());
        let (c2, u2) = cp.put(&meta2, mock_io2.clone());
        let (c3, u3) = cp.put(&meta3, mock_io3.clone());

        let closed_item = tokio::select! {
            _ = cp.idle_poll(mock_io1.try_lock_owned().unwrap(), &meta1, Some(Duration::from_secs(1)), c1, u1) => {debug!("notifier1"); 1},
            _ = cp.idle_poll(mock_io2.try_lock_owned().unwrap(), &meta1, Some(Duration::from_secs(2)), c2, u2) => {debug!("notifier2"); 2},
            _ = cp.idle_poll(mock_io3.try_lock_owned().unwrap(), &meta1, Some(Duration::from_secs(3)), c3, u3) => {debug!("notifier3"); 3},
        };
        assert_eq!(closed_item, 1);

        let _ = cp.get(&meta1.key).unwrap(); // mock_io3 should be selected
        assert!(cp.get(&meta1.key).is_none()) // mock_io1 should already be removed by idle_poll
    }

    #[tokio::test]
    async fn test_evict_poll() {
        let meta1 = ConnectionMeta::new(101, 1);
        let mock_io1 = Arc::new(AsyncMutex::new(
            Builder::new().wait(Duration::from_secs(99)).build(),
        ));
        let meta2 = ConnectionMeta::new(102, 2);
        let mock_io2 = Arc::new(AsyncMutex::new(
            Builder::new().wait(Duration::from_secs(99)).build(),
        ));
        let meta3 = ConnectionMeta::new(101, 3);
        let mock_io3 = Arc::new(AsyncMutex::new(
            Builder::new().wait(Duration::from_secs(99)).build(),
        ));
        let cp: ConnectionPool<Arc<AsyncMutex<Mock>>> = ConnectionPool::new(2);
        let (c1, u1) = cp.put(&meta1, mock_io1.clone());
        let (c2, u2) = cp.put(&meta2, mock_io2.clone());
        let (c3, u3) = cp.put(&meta3, mock_io3.clone()); // 1 should be evicted at this point

        let closed_item = tokio::select! {
            _ = cp.idle_poll(mock_io1.try_lock_owned().unwrap(), &meta1, None, c1, u1) => {debug!("notifier1"); 1},
            _ = cp.idle_poll(mock_io2.try_lock_owned().unwrap(), &meta1, None, c2, u2) => {debug!("notifier2"); 2},
            _ = cp.idle_poll(mock_io3.try_lock_owned().unwrap(), &meta1, None, c3, u3) => {debug!("notifier3"); 3},
        };
        assert_eq!(closed_item, 1);

        let _ = cp.get(&meta1.key).unwrap(); // mock_io3 should be selected
        assert!(cp.get(&meta1.key).is_none()) // mock_io1 should already be removed by idle_poll
    }
}
