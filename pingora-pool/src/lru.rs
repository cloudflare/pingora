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

use core::hash::Hash;
use lru::LruCache;
use parking_lot::RwLock;
use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::Arc;
use thread_local::ThreadLocal;
use tokio::sync::Notify;

pub struct Node<T> {
    pub close_notifier: Arc<Notify>,
    pub meta: T,
}

impl<T> Node<T> {
    pub fn new(meta: T) -> Self {
        Node {
            close_notifier: Arc::new(Notify::new()),
            meta,
        }
    }

    pub fn notify_close(&self) {
        self.close_notifier.notify_one();
    }
}

pub struct Lru<K, T>
where
    K: Send,
    T: Send,
{
    lru: RwLock<ThreadLocal<RefCell<LruCache<K, Node<T>>>>>,
    size: usize,
    drain: AtomicBool,
}

impl<K, T> Lru<K, T>
where
    K: Hash + Eq + Send,
    T: Send,
{
    pub fn new(size: usize) -> Self {
        Lru {
            lru: RwLock::new(ThreadLocal::new()),
            size,
            drain: AtomicBool::new(false),
        }
    }

    // put a node in and return the meta of the replaced node
    pub fn put(&self, key: K, value: Node<T>) -> Option<T> {
        if self.drain.load(Relaxed) {
            value.notify_close(); // sort of hack to simulate being evicted right away
            return None;
        }
        let lru = self.lru.read(); /* read lock */
        let lru_cache = &mut *(lru
            .get_or(|| RefCell::new(LruCache::unbounded()))
            .borrow_mut());
        lru_cache.put(key, value);
        if lru_cache.len() > self.size {
            match lru_cache.pop_lru() {
                Some((_, v)) => {
                    // TODO: drop the lock here?
                    v.notify_close();
                    return Some(v.meta);
                }
                None => return None,
            }
        }
        None
        /* read lock dropped */
    }

    pub fn add(&self, key: K, meta: T) -> (Arc<Notify>, Option<T>) {
        let node = Node::new(meta);
        let notifier = node.close_notifier.clone();
        // TODO: check if the key is already in it
        (notifier, self.put(key, node))
    }

    pub fn pop(&self, key: &K) -> Option<Node<T>> {
        let lru = self.lru.read(); /* read lock */
        let lru_cache = &mut *(lru
            .get_or(|| RefCell::new(LruCache::unbounded()))
            .borrow_mut());
        lru_cache.pop(key)
        /* read lock dropped */
    }

    #[allow(dead_code)]
    pub fn drain(&self) {
        self.drain.store(true, Relaxed);

        /* drain need to go through all the local lru cache objects
         * acquire an exclusive write lock to make it safe */
        let mut lru = self.lru.write(); /* write lock */
        let lru_cache_iter = lru.iter_mut();
        for lru_cache_rc in lru_cache_iter {
            let mut lru_cache = lru_cache_rc.borrow_mut();
            for (_, item) in lru_cache.iter() {
                item.notify_close();
            }
            lru_cache.clear();
        }
        /* write lock dropped */
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use log::debug;

    #[tokio::test]
    async fn test_evict_close() {
        let pool: Lru<i32, ()> = Lru::new(2);
        let (notifier1, _) = pool.add(1, ());
        let (notifier2, _) = pool.add(2, ());
        let (notifier3, _) = pool.add(3, ());
        let closed_item = tokio::select! {
            _ = notifier1.notified() => {debug!("notifier1"); 1},
            _ = notifier2.notified() => {debug!("notifier2"); 2},
            _ = notifier3.notified() => {debug!("notifier3"); 3},
        };
        assert_eq!(closed_item, 1);
    }

    #[tokio::test]
    async fn test_evict_close_with_pop() {
        let pool: Lru<i32, ()> = Lru::new(2);
        let (notifier1, _) = pool.add(1, ());
        let (notifier2, _) = pool.add(2, ());
        pool.pop(&1);
        let (notifier3, _) = pool.add(3, ());
        let (notifier4, _) = pool.add(4, ());
        let closed_item = tokio::select! {
            _ = notifier1.notified() => {debug!("notifier1"); 1},
            _ = notifier2.notified() => {debug!("notifier2"); 2},
            _ = notifier3.notified() => {debug!("notifier3"); 3},
            _ = notifier4.notified() => {debug!("notifier4"); 4},
        };
        assert_eq!(closed_item, 2);
    }

    #[tokio::test]
    async fn test_drain() {
        let pool: Lru<i32, ()> = Lru::new(4);
        let (notifier1, _) = pool.add(1, ());
        let (notifier2, _) = pool.add(2, ());
        let (notifier3, _) = pool.add(3, ());
        pool.drain();
        let (notifier4, _) = pool.add(4, ());

        tokio::join!(
            notifier1.notified(),
            notifier2.notified(),
            notifier3.notified(),
            notifier4.notified()
        );
    }
}
