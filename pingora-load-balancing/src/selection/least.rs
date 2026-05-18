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

//! Least connections backend selection.

use super::{Backend, BackendIter, BackendSelection};
use std::collections::{BTreeSet, HashMap};
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering::Relaxed};
use std::sync::{Arc, Mutex, Weak};

/// Shared store for per backend inflight counters.
#[derive(Debug, Default)]
pub struct LeastConnStateStore {
    // Mutex is locked only during rebuild!!
    handles: Mutex<HashMap<u64, Weak<AtomicU32>>>,
}

impl LeastConnStateStore {
    fn get_or_create(&self, hash_key: u64) -> Arc<AtomicU32> {
        let mut map = self.handles.lock().expect("state store lock poisoned");
        if let Some(existing) = map.get(&hash_key).and_then(Weak::upgrade) {
            return existing;
        }
        let handle = Arc::new(AtomicU32::new(0));
        map.insert(hash_key, Arc::downgrade(&handle));
        handle
    }

    fn cleanup_stale(&self) {
        let mut map = self.handles.lock().expect("state store lock poisoned");
        map.retain(|_, w| w.strong_count() > 0);
    }
}

/// Configuration for [`LeastConnections`] selection.
#[derive(Clone, Debug, Default)]
pub struct LeastConnectionsConfig {
    state_store: Arc<LeastConnStateStore>,
}

impl LeastConnectionsConfig {
    pub fn new() -> Self {
        Self::default()
    }

    pub(crate) fn state_store(&self) -> &LeastConnStateStore {
        &self.state_store
    }
}

/// Least connections backend selectr.
pub struct LeastConnections {
    backends: Box<[Backend]>,
    handles: Box<[Arc<AtomicU32>]>,
    index_by_hash: HashMap<u64, usize>,
    /// Monotonic counter for round robin tie-breaking among equally loaded backends.
    tie_breaker: AtomicUsize,
}

impl LeastConnections {
    /// Increment the inflight counter and return a lease.
    pub(crate) fn acquire_lease(&self, backend: &Backend) -> Option<LeastConnLease> {
        let &idx = self.index_by_hash.get(&backend.hash_key())?;
        let handle = self.handles[idx].clone();
        handle.fetch_add(1, Relaxed);
        Some(LeastConnLease { handle })
    }

    #[allow(dead_code)]
    pub(crate) fn active_connections(&self, backend: &Backend) -> Option<u32> {
        let &idx = self.index_by_hash.get(&backend.hash_key())?;
        Some(self.handles[idx].load(Relaxed))
    }

    #[allow(dead_code)]
    pub(crate) fn handle_for_backend(&self, backend: &Backend) -> Option<Arc<AtomicU32>> {
        let &idx = self.index_by_hash.get(&backend.hash_key())?;
        Some(self.handles[idx].clone())
    }
}

impl BackendSelection for LeastConnections {
    type Iter = LeastConnectionsIterator;
    type Config = LeastConnectionsConfig;

    fn build(backends: &BTreeSet<Backend>) -> Self {
        Self::build_with_config(backends, &LeastConnectionsConfig::default())
    }

    fn build_with_config(backends: &BTreeSet<Backend>, config: &Self::Config) -> Self {
        let store = config.state_store();
        let mut backend = Vec::with_capacity(backends.len());
        let mut handles = Vec::with_capacity(backends.len());
        let mut index_by_hash = HashMap::with_capacity(backends.len());

        for (idx, b) in backends.iter().enumerate() {
            index_by_hash.insert(b.hash_key(), idx);
            handles.push(store.get_or_create(b.hash_key()));
            backend.push(b.clone());
        }

        store.cleanup_stale();

        Self {
            backends: backend.into_boxed_slice(),
            handles: handles.into_boxed_slice(),
            index_by_hash,
            tie_breaker: AtomicUsize::new(0),
        }
    }

    fn iter(self: &Arc<Self>, _key: &[u8]) -> Self::Iter {
        LeastConnectionsIterator::new(self.clone())
    }
}

/// Iterator that yields backends in ascending weighted load order.
///
/// ## algorithm
///
/// - Snapshot: read all inflight counters.
/// - Sort: order backend indices by normalised load
///   (`a_active * b_weight`  cmp with  `b_active * a_weight`).
/// - Tie-break: when normalised loads are equal, a rotating offset picks
///   which backend comes first. The offset increments on every call,
///   giving round robin fairness among equally loaded backends. Weight is not
///   involved in tie-breaking - it is already factored into the load comparison.
pub struct LeastConnectionsIterator {
    selector: Arc<LeastConnections>,
    sorted: Box<[usize]>,
    cursor: usize,
}

impl LeastConnectionsIterator {
    fn new(selector: Arc<LeastConnections>) -> Self {
        let len = selector.backends.len();
        let mut indices: Vec<usize> = (0..len).collect();

        if len > 1 {
            let inflight_counters: Vec<u32> =
                selector.handles.iter().map(|h| h.load(Relaxed)).collect();

            let rotation = selector.tie_breaker.fetch_add(1, Relaxed) % len;

            indices.sort_by(|&left, &right| {
                let l_weight = selector.backends[left].weight.max(1) as u64;
                let r_weight = selector.backends[right].weight.max(1) as u64;
                let l_load = inflight_counters[left] as u64 * r_weight;
                let r_load = inflight_counters[right] as u64 * l_weight;

                l_load.cmp(&r_load).then_with(|| {
                    let l_rank = (left + len - rotation) % len;
                    let r_rank = (right + len - rotation) % len;
                    l_rank.cmp(&r_rank)
                })
            });
        }

        Self {
            selector,
            sorted: indices.into_boxed_slice(),
            cursor: 0,
        }
    }
}

impl BackendIter for LeastConnectionsIterator {
    fn next(&mut self) -> Option<&Backend> {
        let &idx = self.sorted.get(self.cursor)?;
        self.cursor += 1;
        self.selector.backends.get(idx)
    }
}

/// RAII lease for least connections selection.
#[derive(Debug)]
pub struct LeastConnLease {
    handle: Arc<AtomicU32>,
}

impl LeastConnLease {
    pub fn inflight(&self) -> u32 {
        self.handle.load(Relaxed)
    }
}

impl Drop for LeastConnLease {
    fn drop(&mut self) {
        self.handle.fetch_sub(1, Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn backend(addr: &str, weight: usize) -> Backend {
        Backend::new_with_weight(addr, weight).unwrap()
    }

    #[test]
    fn test_prefers_backend_with_smallest_load() {
        let b1 = backend("1.1.1.1:80", 1);
        let b2 = backend("1.0.0.1:80", 1);
        let set = BTreeSet::from_iter([b1.clone(), b2.clone()]);

        let selector = Arc::new(LeastConnections::build(&set));
        let _lease = selector.acquire_lease(&b1).unwrap();

        let mut iter = selector.iter(b"");
        assert_eq!(iter.next(), Some(&b2));
        assert_eq!(iter.next(), Some(&b1));
    }

    #[test]
    fn test_weighted_comparison() {
        // to avoid dropping leases
        let mut leases = vec![];
        let b0 = backend("1.1.1.0:80", 1);
        let b1 = backend("1.1.1.1:80", 2);
        let b2 = backend("1.1.1.2:80", 1);
        let b3 = backend("1.1.1.3:80", 1);
        let set = BTreeSet::from_iter([b0.clone(), b1.clone(), b2.clone(), b3.clone()]);

        let selector = Arc::new(LeastConnections::build(&set));

        // setup loads: [5,3,7,2]
        for _ in 0..5 {
            leases.push(selector.acquire_lease(&b0).unwrap());
        }
        for _ in 0..3 {
            leases.push(selector.acquire_lease(&b1).unwrap());
        }
        for _ in 0..7 {
            leases.push(selector.acquire_lease(&b2).unwrap());
        }
        for _ in 0..2 {
            leases.push(selector.acquire_lease(&b3).unwrap());
        }

        // normalized load is active/weight (compared without division as:
        // left_active * right_weight  vs  right_active * left_weight)
        //
        // with loads [5,3,7,2] and weights [1,2,1,1]:
        // b1=3/2=1.5 < b3=2/1=2 < b0=5/1=5 < b2=7/1=7

        let mut iter = selector.iter(b"");

        assert_eq!(iter.next(), Some(&b1));
        assert_eq!(iter.next(), Some(&b3));
        assert_eq!(iter.next(), Some(&b0));
        assert_eq!(iter.next(), Some(&b2));
    }

    #[test]
    fn test_fair_tie_break() {
        let b1 = backend("1.1.1.1:80", 1);
        let b2 = backend("1.0.0.1:80", 1);
        let set = BTreeSet::from_iter([b1.clone(), b2.clone()]);
        let selector = Arc::new(LeastConnections::build(&set));

        let mut first_is_b1 = 0usize;
        let mut first_is_b2 = 0usize;
        for _ in 0..8 {
            match selector.iter(b"").next() {
                Some(chosen) if *chosen == b1 => first_is_b1 += 1,
                Some(chosen) if *chosen == b2 => first_is_b2 += 1,
                _ => unreachable!(),
            }
        }

        assert_eq!(first_is_b1, 4);
        assert_eq!(first_is_b2, 4);
    }

    #[test]
    fn test_lease_drop_decrements_counter() {
        let b1 = backend("1.1.1.1:80", 1);
        let set = BTreeSet::from_iter([b1.clone()]);
        let selector = Arc::new(LeastConnections::build(&set));

        assert_eq!(selector.active_connections(&b1), Some(0));
        let lease = selector.acquire_lease(&b1).unwrap();
        assert_eq!(selector.active_connections(&b1), Some(1));
        drop(lease);
        assert_eq!(selector.active_connections(&b1), Some(0));
    }

    #[test]
    fn test_state_store_preserves_handles_across_rebuilds() {
        let b1 = backend("1.1.1.1:80", 1);
        let config = LeastConnectionsConfig::default();

        let set_v1 = BTreeSet::from_iter([b1.clone()]);
        let selector_v1 = LeastConnections::build_with_config(&set_v1, &config);
        let handle_v1 = selector_v1.handle_for_backend(&b1).unwrap();

        let b1_fresh = backend("1.1.1.1:80", 1);
        let set_v2 = BTreeSet::from_iter([b1_fresh.clone()]);
        let selector_v2 = LeastConnections::build_with_config(&set_v2, &config);
        let handle_v2 = selector_v2.handle_for_backend(&b1_fresh).unwrap();

        assert!(Arc::ptr_eq(&handle_v1, &handle_v2));
    }
}
