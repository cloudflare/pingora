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

//! # Pingora Load Balancing utilities
//! This crate provides common service discovery, health check and load balancing
//! algorithms for proxies to use.

// https://github.com/mcarton/rust-derivative/issues/112
// False positive for macro generated code
#![allow(clippy::non_canonical_partial_ord_impl)]

use arc_swap::ArcSwap;
use derivative::Derivative;
use futures::FutureExt;
pub use http::Extensions;
use pingora_core::protocols::l4::socket::SocketAddr;
use pingora_error::{ErrorType, OrErr, Result};
use std::collections::hash_map::DefaultHasher;
use std::collections::{BTreeSet, HashMap};
use std::hash::{Hash, Hasher};
use std::io::Result as IoResult;
use std::net::ToSocketAddrs;
use std::sync::Arc;
use std::time::Duration;

mod background;
pub mod discovery;
pub mod health_check;
pub mod selection;

use discovery::ServiceDiscovery;
use health_check::Health;
use selection::UniqueIterator;
use selection::{BackendIter, BackendSelection};

pub mod prelude {
    pub use crate::health_check::TcpHealthCheck;
    pub use crate::selection::RoundRobin;
    pub use crate::LoadBalancer;
}

/// [Backend] represents a server to proxy or connect to.
#[derive(Derivative)]
#[derivative(Clone, Hash, PartialEq, PartialOrd, Eq, Ord, Debug)]
pub struct Backend {
    /// The address to the backend server.
    pub addr: SocketAddr,
    /// The relative weight of the server. Load balancing algorithms will
    /// proportionally distributed traffic according to this value.
    pub weight: usize,

    /// The extension field to put arbitrary data to annotate the Backend.
    /// The data added here is opaque to this crate hence the data is ignored by
    /// functionalities of this crate. For example, two backends with the same
    /// [SocketAddr] and the same weight but different `ext` data are considered
    /// identical.
    /// See [Extensions] for how to add and read the data.
    #[derivative(PartialEq = "ignore")]
    #[derivative(PartialOrd = "ignore")]
    #[derivative(Hash = "ignore")]
    #[derivative(Ord = "ignore")]
    pub ext: Extensions,
}

impl Backend {
    /// Create a new [Backend] with `weight` 1. The function will try to parse
    ///  `addr` into a [std::net::SocketAddr].
    pub fn new(addr: &str) -> Result<Self> {
        let addr = addr
            .parse()
            .or_err(ErrorType::InternalError, "invalid socket addr")?;
        Ok(Backend {
            addr: SocketAddr::Inet(addr),
            weight: 1,
            ext: Extensions::new(),
        })
        // TODO: UDS
    }

    pub(crate) fn hash_key(&self) -> u64 {
        let mut hasher = DefaultHasher::new();
        self.hash(&mut hasher);
        hasher.finish()
    }
}

impl std::ops::Deref for Backend {
    type Target = SocketAddr;

    fn deref(&self) -> &Self::Target {
        &self.addr
    }
}

impl std::ops::DerefMut for Backend {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.addr
    }
}

impl std::net::ToSocketAddrs for Backend {
    type Iter = std::iter::Once<std::net::SocketAddr>;

    fn to_socket_addrs(&self) -> std::io::Result<Self::Iter> {
        self.addr.to_socket_addrs()
    }
}

/// [Backends] is a collection of [Backend]s.
///
/// It includes a service discovery method (static or dynamic) to discover all
/// the available backends as well as an optional health check method to probe the liveness
/// of each backend.
pub struct Backends {
    discovery: Box<dyn ServiceDiscovery + Send + Sync + 'static>,
    health_check: Option<Arc<dyn health_check::HealthCheck + Send + Sync + 'static>>,
    backends: ArcSwap<BTreeSet<Backend>>,
    health: ArcSwap<HashMap<u64, Health>>,
}

impl Backends {
    /// Create a new [Backends] with the given [ServiceDiscovery] implementation.
    ///
    /// The health check method is by default empty.
    pub fn new(discovery: Box<dyn ServiceDiscovery + Send + Sync + 'static>) -> Self {
        Self {
            discovery,
            health_check: None,
            backends: Default::default(),
            health: Default::default(),
        }
    }

    /// Set the health check method. See [health_check] for the methods provided.
    pub fn set_health_check(
        &mut self,
        hc: Box<dyn health_check::HealthCheck + Send + Sync + 'static>,
    ) {
        self.health_check = Some(hc.into())
    }

    /// Updates backends when the new is different from the current set,
    /// the callback will be invoked when the new set of backend is different
    /// from the current one so that the caller can update the selector accordingly.
    fn do_update<F>(
        &self,
        new_backends: BTreeSet<Backend>,
        enablement: HashMap<u64, bool>,
        callback: F,
    ) where
        F: Fn(Arc<BTreeSet<Backend>>),
    {
        if (**self.backends.load()) != new_backends {
            let old_health = self.health.load();
            let mut health = HashMap::with_capacity(new_backends.len());
            for backend in new_backends.iter() {
                let hash_key = backend.hash_key();
                // use the default health if the backend is new
                let backend_health = old_health.get(&hash_key).cloned().unwrap_or_default();

                // override enablement
                if let Some(backend_enabled) = enablement.get(&hash_key) {
                    backend_health.enable(*backend_enabled);
                }
                health.insert(hash_key, backend_health);
            }

            // TODO: put this all under 1 ArcSwap so the update is atomic
            // It's important the `callback()` executes first since computing selector backends might
            // be expensive. For example, if a caller checks `backends` to see if any are available
            // they may encounter false positives if the selector isn't ready yet.
            let new_backends = Arc::new(new_backends);
            callback(new_backends.clone());
            self.backends.store(new_backends);
            self.health.store(Arc::new(health));
        } else {
            // no backend change, just check enablement
            for (hash_key, backend_enabled) in enablement.iter() {
                // override enablement if set
                // this get should always be Some(_) because we already populate `health`` for all known backends
                if let Some(backend_health) = self.health.load().get(hash_key) {
                    backend_health.enable(*backend_enabled);
                }
            }
        }
    }

    /// Whether a certain [Backend] is ready to serve traffic.
    ///
    /// This function returns true when the backend is both healthy and enabled.
    /// This function returns true when the health check is unset but the backend is enabled.
    /// When the health check is set, this function will return false for the `backend` it
    /// doesn't know.
    pub fn ready(&self, backend: &Backend) -> bool {
        self.health
            .load()
            .get(&backend.hash_key())
            // Racing: return `None` when this function is called between the
            // backend store and the health store
            .map_or(self.health_check.is_none(), |h| h.ready())
    }

    /// Manually set if a [Backend] is ready to serve traffic.
    ///
    /// This method does not override the health of the backend. It is meant to be used
    /// to stop a backend from accepting traffic when it is still healthy.
    ///
    /// This method is noop when the given backend doesn't exist in the service discovery.
    pub fn set_enable(&self, backend: &Backend, enabled: bool) {
        // this should always be Some(_) because health is always populated during update
        if let Some(h) = self.health.load().get(&backend.hash_key()) {
            h.enable(enabled)
        };
    }

    /// Return the collection of the backends.
    pub fn get_backend(&self) -> Arc<BTreeSet<Backend>> {
        self.backends.load_full()
    }

    /// Call the service discovery method to update the collection of backends.
    ///
    /// The callback will be invoked when the new set of backend is different
    /// from the current one so that the caller can update the selector accordingly.
    pub async fn update<F>(&self, callback: F) -> Result<()>
    where
        F: Fn(Arc<BTreeSet<Backend>>),
    {
        let (new_backends, enablement) = self.discovery.discover().await?;
        self.do_update(new_backends, enablement, callback);
        Ok(())
    }

    /// Run health check on all backends if it is set.
    ///
    /// When `parallel: true`, all backends are checked in parallel instead of sequentially
    pub async fn run_health_check(&self, parallel: bool) {
        use crate::health_check::HealthCheck;
        use log::{info, warn};
        use pingora_runtime::current_handle;

        async fn check_and_report(
            backend: &Backend,
            check: &Arc<dyn HealthCheck + Send + Sync>,
            health_table: &HashMap<u64, Health>,
        ) {
            let errored = check.check(backend).await.err();
            if let Some(h) = health_table.get(&backend.hash_key()) {
                let flipped =
                    h.observe_health(errored.is_none(), check.health_threshold(errored.is_none()));
                if flipped {
                    if let Some(e) = errored {
                        warn!("{backend:?} becomes unhealthy, {e}");
                    } else {
                        info!("{backend:?} becomes healthy");
                    }
                }
            }
        }

        let Some(health_check) = self.health_check.as_ref() else {
            return;
        };

        let backends = self.backends.load();
        if parallel {
            let health_table = self.health.load_full();
            let runtime = current_handle();
            let jobs = backends.iter().map(|backend| {
                let backend = backend.clone();
                let check = health_check.clone();
                let ht = health_table.clone();
                runtime.spawn(async move {
                    check_and_report(&backend, &check, &ht).await;
                })
            });

            futures::future::join_all(jobs).await;
        } else {
            for backend in backends.iter() {
                check_and_report(backend, health_check, &self.health.load()).await;
            }
        }
    }
}

/// A [LoadBalancer] instance contains the service discovery, health check and backend selection
/// all together.
///
/// In order to run service discovery and health check at the designated frequencies, the [LoadBalancer]
/// needs to be run as a [pingora_core::services::background::BackgroundService].
pub struct LoadBalancer<S> {
    backends: Backends,
    selector: ArcSwap<S>,
    /// How frequent the health check logic (if set) should run.
    ///
    /// If `None`, the health check logic will only run once at the beginning.
    pub health_check_frequency: Option<Duration>,
    /// How frequent the service discovery should run.
    ///
    /// If `None`, the service discovery will only run once at the beginning.
    pub update_frequency: Option<Duration>,
    /// Whether to run health check to all backends in parallel. Default is false.
    pub parallel_health_check: bool,
}

impl<'a, S: BackendSelection> LoadBalancer<S>
where
    S: BackendSelection + 'static,
    S::Iter: BackendIter,
{
    /// Build a [LoadBalancer] with static backends created from the iter.
    ///
    /// Note: [ToSocketAddrs] will invoke blocking network IO for DNS lookup if
    /// the input cannot be directly parsed as [SocketAddr].
    pub fn try_from_iter<A, T: IntoIterator<Item = A>>(iter: T) -> IoResult<Self>
    where
        A: ToSocketAddrs,
    {
        let discovery = discovery::Static::try_from_iter(iter)?;
        let backends = Backends::new(discovery);
        let lb = Self::from_backends(backends);
        lb.update()
            .now_or_never()
            .expect("static should not block")
            .expect("static should not error");
        Ok(lb)
    }

    /// Build a [LoadBalancer] with the given [Backends].
    pub fn from_backends(backends: Backends) -> Self {
        let selector = ArcSwap::new(Arc::new(S::build(&backends.get_backend())));
        LoadBalancer {
            backends,
            selector,
            health_check_frequency: None,
            update_frequency: None,
            parallel_health_check: false,
        }
    }

    /// Run the service discovery and update the selection algorithm.
    ///
    /// This function will be called every `update_frequency` if this [LoadBalancer] instance
    /// is running as a background service.
    pub async fn update(&self) -> Result<()> {
        self.backends
            .update(|backends| self.selector.store(Arc::new(S::build(&backends))))
            .await
    }

    /// Return the first healthy [Backend] according to the selection algorithm and the
    /// health check results.
    ///
    /// The `key` is used for hash based selection and is ignored if the selection is random or
    /// round robin.
    ///
    /// the `max_iterations` is there to bound the search time for the next Backend. In certain
    /// algorithm like Ketama hashing, the search for the next backend is linear and could take
    /// a lot steps.
    // TODO: consider remove `max_iterations` as users have no idea how to set it.
    pub fn select(&self, key: &[u8], max_iterations: usize) -> Option<Backend> {
        self.select_with(key, max_iterations, |_, health| health)
    }

    /// Similar to [Self::select], return the first healthy [Backend] according to the selection algorithm
    /// and the user defined `accept` function.
    ///
    /// The `accept` function takes two inputs, the backend being selected and the internal health of that
    /// backend. The function can do things like ignoring the internal health checks or skipping this backend
    /// because it failed before. The `accept` function is called multiple times iterating over backends
    /// until it returns `true`.
    pub fn select_with<F>(&self, key: &[u8], max_iterations: usize, accept: F) -> Option<Backend>
    where
        F: Fn(&Backend, bool) -> bool,
    {
        let selection = self.selector.load();
        let mut iter = UniqueIterator::new(selection.iter(key), max_iterations);
        while let Some(b) = iter.get_next() {
            if accept(&b, self.backends.ready(&b)) {
                return Some(b);
            }
        }
        None
    }

    /// Set the health check method. See [health_check].
    pub fn set_health_check(
        &mut self,
        hc: Box<dyn health_check::HealthCheck + Send + Sync + 'static>,
    ) {
        self.backends.set_health_check(hc);
    }

    /// Access the [Backends] of this [LoadBalancer]
    pub fn backends(&self) -> &Backends {
        &self.backends
    }
}

#[cfg(test)]
mod test {
    use std::sync::atomic::{AtomicBool, Ordering::Relaxed};

    use super::*;
    use async_trait::async_trait;

    #[tokio::test]
    async fn test_static_backends() {
        let backends: LoadBalancer<selection::RoundRobin> =
            LoadBalancer::try_from_iter(["1.1.1.1:80", "1.0.0.1:80"]).unwrap();

        let backend1 = Backend::new("1.1.1.1:80").unwrap();
        let backend2 = Backend::new("1.0.0.1:80").unwrap();
        let backend = backends.backends().get_backend();
        assert!(backend.contains(&backend1));
        assert!(backend.contains(&backend2));
    }

    #[tokio::test]
    async fn test_backends() {
        let discovery = discovery::Static::default();
        let good1 = Backend::new("1.1.1.1:80").unwrap();
        discovery.add(good1.clone());
        let good2 = Backend::new("1.0.0.1:80").unwrap();
        discovery.add(good2.clone());
        let bad = Backend::new("127.0.0.1:79").unwrap();
        discovery.add(bad.clone());

        let mut backends = Backends::new(Box::new(discovery));
        let check = health_check::TcpHealthCheck::new();
        backends.set_health_check(check);

        // true: new backend discovered
        let updated = AtomicBool::new(false);
        backends
            .update(|_| updated.store(true, Relaxed))
            .await
            .unwrap();
        assert!(updated.load(Relaxed));

        // false: no new backend discovered
        let updated = AtomicBool::new(false);
        backends
            .update(|_| updated.store(true, Relaxed))
            .await
            .unwrap();
        assert!(!updated.load(Relaxed));

        backends.run_health_check(false).await;

        let backend = backends.get_backend();
        assert!(backend.contains(&good1));
        assert!(backend.contains(&good2));
        assert!(backend.contains(&bad));

        assert!(backends.ready(&good1));
        assert!(backends.ready(&good2));
        assert!(!backends.ready(&bad));
    }
    #[tokio::test]
    async fn test_backends_with_ext() {
        let discovery = discovery::Static::default();
        let mut b1 = Backend::new("1.1.1.1:80").unwrap();
        b1.ext.insert(true);
        let mut b2 = Backend::new("1.0.0.1:80").unwrap();
        b2.ext.insert(1u8);
        discovery.add(b1.clone());
        discovery.add(b2.clone());

        let backends = Backends::new(Box::new(discovery));

        // fill in the backends
        backends.update(|_| {}).await.unwrap();

        let backend = backends.get_backend();
        assert!(backend.contains(&b1));
        assert!(backend.contains(&b2));

        let b2 = backend.first().unwrap();
        assert_eq!(b2.ext.get::<u8>(), Some(&1));

        let b1 = backend.last().unwrap();
        assert_eq!(b1.ext.get::<bool>(), Some(&true));
    }

    #[tokio::test]
    async fn test_discovery_readiness() {
        use discovery::Static;

        struct TestDiscovery(Static);
        #[async_trait]
        impl ServiceDiscovery for TestDiscovery {
            async fn discover(&self) -> Result<(BTreeSet<Backend>, HashMap<u64, bool>)> {
                let bad = Backend::new("127.0.0.1:79").unwrap();
                let (backends, mut readiness) = self.0.discover().await?;
                readiness.insert(bad.hash_key(), false);
                Ok((backends, readiness))
            }
        }
        let discovery = Static::default();
        let good1 = Backend::new("1.1.1.1:80").unwrap();
        discovery.add(good1.clone());
        let good2 = Backend::new("1.0.0.1:80").unwrap();
        discovery.add(good2.clone());
        let bad = Backend::new("127.0.0.1:79").unwrap();
        discovery.add(bad.clone());
        let discovery = TestDiscovery(discovery);

        let backends = Backends::new(Box::new(discovery));

        // true: new backend discovered
        let updated = AtomicBool::new(false);
        backends
            .update(|_| updated.store(true, Relaxed))
            .await
            .unwrap();
        assert!(updated.load(Relaxed));

        let backend = backends.get_backend();
        assert!(backend.contains(&good1));
        assert!(backend.contains(&good2));
        assert!(backend.contains(&bad));

        assert!(backends.ready(&good1));
        assert!(backends.ready(&good2));
        assert!(!backends.ready(&bad));
    }

    #[tokio::test]
    async fn test_parallel_health_check() {
        let discovery = discovery::Static::default();
        let good1 = Backend::new("1.1.1.1:80").unwrap();
        discovery.add(good1.clone());
        let good2 = Backend::new("1.0.0.1:80").unwrap();
        discovery.add(good2.clone());
        let bad = Backend::new("127.0.0.1:79").unwrap();
        discovery.add(bad.clone());

        let mut backends = Backends::new(Box::new(discovery));
        let check = health_check::TcpHealthCheck::new();
        backends.set_health_check(check);

        // true: new backend discovered
        let updated = AtomicBool::new(false);
        backends
            .update(|_| updated.store(true, Relaxed))
            .await
            .unwrap();
        assert!(updated.load(Relaxed));

        backends.run_health_check(true).await;

        assert!(backends.ready(&good1));
        assert!(backends.ready(&good2));
        assert!(!backends.ready(&bad));
    }

    mod thread_safety {
        use super::*;

        struct MockDiscovery {
            expected: usize,
        }
        #[async_trait]
        impl ServiceDiscovery for MockDiscovery {
            async fn discover(&self) -> Result<(BTreeSet<Backend>, HashMap<u64, bool>)> {
                let mut d = BTreeSet::new();
                let mut m = HashMap::with_capacity(self.expected);
                for i in 0..self.expected {
                    let b = Backend::new(&format!("1.1.1.1:{i}")).unwrap();
                    m.insert(i as u64, true);
                    d.insert(b);
                }
                Ok((d, m))
            }
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn test_consistency() {
            let expected = 3000;
            let discovery = MockDiscovery { expected };
            let lb = Arc::new(LoadBalancer::<selection::Consistent>::from_backends(
                Backends::new(Box::new(discovery)),
            ));
            let lb2 = lb.clone();

            tokio::spawn(async move {
                assert!(lb2.update().await.is_ok());
            });
            let mut backend_count = 0;
            while backend_count == 0 {
                let backends = lb.backends();
                backend_count = backends.backends.load_full().len();
            }
            assert_eq!(backend_count, expected);
            assert!(lb.select_with(b"test", 1, |_, _| true).is_some());
        }
    }
}
