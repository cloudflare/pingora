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

use arc_swap::ArcSwap;
use futures::FutureExt;
use pingora_core::protocols::l4::socket::SocketAddr;
use pingora_error::{ErrorType, OrErr, Result};
use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
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
#[derive(Clone, Hash, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct Backend<M> {
    /// The address to the backend server.
    pub addr: SocketAddr,
    /// The relative weight of the server. Load balancing algorithms will
    /// proportionally distributed traffic according to this value.
    pub weight: usize,
    /// Additional, optional metadata
    pub metadata: M,
}

impl<M> Backend<M>
where
    M: Hash,
{
    pub(crate) fn hash_key(&self) -> u64 {
        let mut hasher = DefaultHasher::new();
        self.hash(&mut hasher);
        hasher.finish()
    }
}

impl<M> Backend<M> {
    /// Create a new [Backend] with `weight` 1. The function will try to parse
    ///  `addr` into a [std::net::SocketAddr].
    pub fn new_with_meta(addr: &str, meta: M) -> Result<Self> {
        let addr = addr
            .parse()
            .or_err(ErrorType::InternalError, "invalid socket addr")?;

        Ok(Backend {
            addr: SocketAddr::Inet(addr),
            weight: 1,
            metadata: meta,
        })
        // TODO: UDS
    }

    pub fn addr(&self) -> &SocketAddr {
        &self.addr
    }

    pub fn addr_mut(&mut self) -> &mut SocketAddr {
        &mut self.addr
    }

    pub fn metadata(&self) -> &M {
        &self.metadata
    }

    pub fn metadata_mut(&mut self) -> &mut M {
        &mut self.metadata
    }
}

impl<M> std::net::ToSocketAddrs for Backend<M> {
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
pub struct Backends<M> {
    discovery: Box<dyn ServiceDiscovery<Metadata = M> + Send + Sync + 'static>,
    health_check: Option<Arc<dyn health_check::HealthCheck<Metadata = M> + Send + Sync + 'static>>,
    backends: ArcSwap<HashSet<Backend<M>>>,
    health: ArcSwap<HashMap<u64, Health>>,
}

impl<M> Backends<M> {
    /// Create a new [Backends] with the given [ServiceDiscovery] implementation.
    ///
    /// The health check method is by default empty.
    pub fn new(discovery: Box<dyn ServiceDiscovery<Metadata = M> + Send + Sync + 'static>) -> Self {
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
        hc: Box<dyn health_check::HealthCheck<Metadata = M> + Send + Sync + 'static>,
    ) {
        self.health_check = Some(hc.into())
    }
}

impl<M> Backends<M>
where
    M: Eq + Hash,
{
    /// Return true when the new is different from the current set of backends
    fn do_update(&self, new_backends: HashSet<Backend<M>>, enablement: HashMap<u64, bool>) -> bool {
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

            // TODO: put backend and health under 1 ArcSwap so that this update is atomic
            self.backends.store(Arc::new(new_backends));
            self.health.store(Arc::new(health));
            true
        } else {
            // no backend change, just check enablement
            for (hash_key, backend_enabled) in enablement.iter() {
                // override enablement if set
                // this get should always be Some(_) because we already populate `health`` for all known backends
                if let Some(backend_health) = self.health.load().get(hash_key) {
                    backend_health.enable(*backend_enabled);
                }
            }
            false
        }
    }

    /// Call the service discovery method to update the collection of backends.
    ///
    /// Return `true` when the new collection is different from the current set of backends.
    /// This return value is useful to tell the caller when to rebuild things that are expensive to
    /// update, such as consistent hashing rings.
    pub async fn update(&self) -> Result<bool> {
        let (new_backends, enablement) = self.discovery.discover().await?;
        Ok(self.do_update(new_backends, enablement))
    }
}

impl<M> Backends<M>
where
    M: Hash,
{
    /// Whether a certain [Backend] is ready to serve traffic.
    ///
    /// This function returns true when the backend is both healthy and enabled.
    /// This function returns true when the health check is unset but the backend is enabled.
    /// When the health check is set, this function will return false for the `backend` it
    /// doesn't know.
    pub fn ready(&self, backend: &Backend<M>) -> bool {
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
    pub fn set_enable(&self, backend: &Backend<M>, enabled: bool) {
        // this should always be Some(_) because health is always populated during update
        if let Some(h) = self.health.load().get(&backend.hash_key()) {
            h.enable(enabled)
        };
    }
}

impl<M> Backends<M> {
    /// Return the collection of the backends.
    pub fn get_backend(&self) -> Arc<HashSet<Backend<M>>> {
        self.backends.load_full()
    }
}

use core::fmt::Debug;

impl<M> Backends<M>
where
    M: Debug + Hash + Send + Clone + Sync + 'static,
{
    /// Run health check on all backends if it is set.
    ///
    /// When `parallel: true`, all backends are checked in parallel instead of sequentially
    pub async fn run_health_check(&self, parallel: bool) {
        use crate::health_check::HealthCheck;
        use log::{info, warn};
        use pingora_runtime::current_handle;

        async fn check_and_report<M: Debug + Hash>(
            backend: &Backend<M>,
            check: &Arc<dyn HealthCheck<Metadata = M> + Send + Sync>,
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
                    check_and_report::<M>(&backend, &check, &ht).await;
                })
            });

            futures::future::join_all(jobs).await;
        } else {
            for backend in backends.iter() {
                check_and_report::<M>(backend, health_check, &self.health.load()).await;
            }
        }
    }
}

/// A [LoadBalancer] instance contains the service discovery, health check and backend selection
/// all together.
///
/// In order to run service discovery and health check at the designated frequencies, the [LoadBalancer]
/// needs to be run as a [pingora_core::services::background::BackgroundService].
pub struct LoadBalancer<S, M> {
    backends: Backends<M>,
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

impl<S, M> LoadBalancer<S, M>
where
    S: BackendSelection<Metadata = M> + 'static,
    S::Iter: BackendIter<Metadata = M>,
    M: Default + Send + Sync + Hash + Eq + Clone + 'static,
{
    /// Build a [LoadBalancer] with static backends created from the iter.
    ///
    /// Note: [ToSocketAddrs] will invoke blocking network IO for DNS lookup if
    /// the input cannot be directly parsed as [SocketAddr].
    pub fn try_from_iter_default_meta<A, T: IntoIterator<Item = A>>(iter: T) -> IoResult<Self>
    where
        A: ToSocketAddrs,
    {
        let iter = iter.into_iter().map(|a| (a, M::default()));
        let discovery = discovery::Static::<M>::try_from_iter(iter)?;
        let backends = Backends::<M>::new(discovery);
        let lb = Self::from_backends(backends);
        lb.update()
            .now_or_never()
            .expect("static should not block")
            .expect("static should not error");
        Ok(lb)
    }
}

impl<S, M> LoadBalancer<S, M>
where
    S: BackendSelection<Metadata = M> + 'static,
    S::Iter: BackendIter<Metadata = M>,
    M: Send + Sync + Hash + Eq + Clone + 'static,
{
    /// Build a [LoadBalancer] with static backends created from the iter.
    ///
    /// Note: [ToSocketAddrs] will invoke blocking network IO for DNS lookup if
    /// the input cannot be directly parsed as [SocketAddr].
    pub fn try_from_iter<A, T: IntoIterator<Item = (A, M)>>(iter: T) -> IoResult<Self>
    where
        A: ToSocketAddrs,
    {
        let discovery = discovery::Static::<M>::try_from_iter(iter)?;
        let backends = Backends::<M>::new(discovery);
        let lb = Self::from_backends(backends);
        lb.update()
            .now_or_never()
            .expect("static should not block")
            .expect("static should not error");
        Ok(lb)
    }
}

impl<S, M> LoadBalancer<S, M>
where
    S: BackendSelection<Metadata = M> + 'static,
    S::Iter: BackendIter<Metadata = M>,
    // M: Clone,
{
    /// Build a [LoadBalancer] with the given [Backends].
    pub fn from_backends(backends: Backends<M>) -> Self {
        let selector = ArcSwap::new(Arc::new(S::build(&backends.get_backend())));
        LoadBalancer {
            backends,
            selector,
            health_check_frequency: None,
            update_frequency: None,
            parallel_health_check: false,
        }
    }
}

impl<'a, S, M> LoadBalancer<S, M>
where
    S: BackendSelection<Metadata = M> + 'static,
    S::Iter: BackendIter<Metadata = M>,
    M: Hash + Eq,
{
    /// Run the service discovery and update the selection algorithm.
    ///
    /// This function will be called every `update_frequency` if this [LoadBalancer] instance
    /// is running as a background service.
    pub async fn update(&self) -> Result<()> {
        if self.backends.update().await? {
            self.selector
                .store(Arc::new(S::build(&self.backends.get_backend())))
        }
        Ok(())
    }
}

impl<S, M> LoadBalancer<S, M>
where
    S: BackendSelection<Metadata = M> + 'static,
    S::Iter: BackendIter<Metadata = M>,
    M: Hash + Clone,
{
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
    pub fn select(&self, key: &[u8], max_iterations: usize) -> Option<Backend<M>> {
        self.select_with(key, max_iterations, |_, health| health)
    }

    /// Similar to [Self::select], return the first healthy [Backend] according to the selection algorithm
    /// and the user defined `accept` function.
    ///
    /// The `accept` function takes two inputs, the backend being selected and the internal health of that
    /// backend. The function can do things like ignoring the internal health checks or skipping this backend
    /// because it failed before. The `accept` function is called multiple times iterating over backends
    /// until it returns `true`.
    pub fn select_with<F>(&self, key: &[u8], max_iterations: usize, accept: F) -> Option<Backend<M>>
    where
        F: Fn(&Backend<M>, bool) -> bool,
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
}

impl<S, M> LoadBalancer<S, M>
where
    S: BackendSelection<Metadata = M> + 'static,
    S::Iter: BackendIter<Metadata = M>,
{
    /// Set the health check method. See [health_check].
    pub fn set_health_check(
        &mut self,
        hc: Box<dyn health_check::HealthCheck<Metadata = M> + Send + Sync + 'static>,
    ) {
        self.backends.set_health_check(hc);
    }

    /// Access the [Backends] of this [LoadBalancer]
    pub fn backends(&self) -> &Backends<M> {
        &self.backends
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use async_trait::async_trait;

    type TestMetadata = u32;

    #[tokio::test]
    async fn test_static_backends() {
        let backends: LoadBalancer<selection::RoundRobin<_>, _> =
            LoadBalancer::try_from_iter_default_meta(["1.1.1.1:80", "1.0.0.1:80"]).unwrap();

        let backend1 = Backend::new_with_meta("1.1.1.1:80", u32::default()).unwrap();
        let backend2 = Backend::new_with_meta("1.0.0.1:80", u32::default()).unwrap();
        let backend = backends.backends().get_backend();
        assert!(backend.contains(&backend1));
        assert!(backend.contains(&backend2));
    }

    #[tokio::test]
    async fn test_backends() {
        let discovery = discovery::Static::default();
        let good1 = Backend::new_with_meta("1.1.1.1:80", 101u32).unwrap();
        discovery.add(good1.clone());
        let good2 = Backend::new_with_meta("1.0.0.1:80", 102u32).unwrap();
        discovery.add(good2.clone());
        let bad = Backend::new_with_meta("127.0.0.1:79", 404u32).unwrap();
        discovery.add(bad.clone());

        let mut backends = Backends::new(Box::new(discovery));
        let check = health_check::TcpHealthCheck::new();
        backends.set_health_check(check);

        // true: new backend discovered
        assert!(backends.update().await.unwrap());

        // false: no new backend discovered
        assert!(!backends.update().await.unwrap());

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
    async fn test_discovery_readiness() {
        use discovery::Static;

        struct TestDiscovery(Static<TestMetadata>);
        #[async_trait]
        impl ServiceDiscovery for TestDiscovery {
            type Metadata = TestMetadata;

            async fn discover(
                &self,
            ) -> Result<(HashSet<Backend<TestMetadata>>, HashMap<u64, bool>)> {
                let bad = Backend::new_with_meta("127.0.0.1:79", 3u32).unwrap();
                let (backends, mut readiness) = self.0.discover().await?;
                readiness.insert(bad.hash_key(), false);
                Ok((backends, readiness))
            }
        }
        let discovery = Static::<TestMetadata>::default();
        let good1 = Backend::new_with_meta("1.1.1.1:80", 1u32).unwrap();
        discovery.add(good1.clone());
        let good2 = Backend::new_with_meta("1.0.0.1:80", 2u32).unwrap();
        discovery.add(good2.clone());
        let bad = Backend::new_with_meta("127.0.0.1:79", 3u32).unwrap();
        discovery.add(bad.clone());
        let discovery = TestDiscovery(discovery);

        let backends = Backends::new(Box::new(discovery));
        assert!(backends.update().await.unwrap());

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
        let good1 = Backend::new_with_meta("1.1.1.1:80", 100u32).unwrap();
        discovery.add(good1.clone());
        let good2 = Backend::new_with_meta("1.0.0.1:80", 200u32).unwrap();
        discovery.add(good2.clone());
        let bad = Backend::new_with_meta("127.0.0.1:79", 404u32).unwrap();
        discovery.add(bad.clone());

        let mut backends = Backends::new(Box::new(discovery));
        let check = health_check::TcpHealthCheck::new();
        backends.set_health_check(check);

        // true: new backend discovered
        assert!(backends.update().await.unwrap());

        backends.run_health_check(true).await;

        assert!(backends.ready(&good1));
        assert!(backends.ready(&good2));
        assert!(!backends.ready(&bad));
    }
}
