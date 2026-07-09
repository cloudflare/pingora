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
use std::time::{Duration, Instant};

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
        Self::new_with_weight(addr, 1)
    }

    /// Creates a new [Backend] with the specified `weight`. The function will try to parse
    /// `addr` into a [std::net::SocketAddr].
    pub fn new_with_weight(addr: &str, weight: usize) -> Result<Self> {
        let addr = addr
            .parse()
            .or_err(ErrorType::InternalError, "invalid socket addr")?;
        Ok(Backend {
            addr: SocketAddr::Inet(addr),
            weight,
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
    backend_keys: ArcSwap<HashMap<SocketAddr, Option<u64>>>,
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
            backend_keys: Default::default(),
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
            let mut backend_keys = HashMap::with_capacity(new_backends.len());
            for backend in new_backends.iter() {
                let hash_key = backend.hash_key();
                // use the default health if the backend is new
                let backend_health = old_health.get(&hash_key).cloned().unwrap_or_default();

                // override enablement
                if let Some(backend_enabled) = enablement.get(&hash_key) {
                    backend_health.enable(*backend_enabled);
                }
                health.insert(hash_key, backend_health);
                backend_keys
                    .entry(backend.addr.clone())
                    .and_modify(|existing| *existing = None)
                    .or_insert(Some(hash_key));
            }

            // TODO: put this all under 1 ArcSwap so the update is atomic
            // It's important the `callback()` executes first since computing selector backends might
            // be expensive. For example, if a caller checks `backends` to see if any are available
            // they may encounter false positives if the selector isn't ready yet.
            let new_backends = Arc::new(new_backends);
            callback(new_backends.clone());
            self.backends.store(new_backends.clone());
            self.health.store(Arc::new(health));
            // Publish address membership last so readiness never resolves a new
            // address against the previous health table.
            self.backend_keys.store(Arc::new(backend_keys));
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
    ///
    /// Backends are matched by their full identity first. If that identity is stale, the
    /// current backend with the same address is used only when the address uniquely identifies
    /// one discovered backend. An unknown or ambiguous address fails closed.
    pub fn ready(&self, backend: &Backend) -> bool {
        let health = self.health.load();
        if let Some(health) = health.get(&backend.hash_key()) {
            return health.ready();
        }

        let Some(Some(hash_key)) = self.backend_keys.load().get(&backend.addr).copied() else {
            return false;
        };
        health.get(&hash_key).is_some_and(Health::ready)
    }

    /// Manually set if a [Backend] is ready to serve traffic.
    ///
    /// This method does not override the health of the backend. It is meant to be used
    /// to stop a backend from accepting traffic when it is still healthy.
    ///
    /// This method is noop when the given backend doesn't exist in the service discovery.
    /// A stale backend identity is resolved by address only when that address uniquely
    /// identifies one currently discovered backend.
    pub fn set_enable(&self, backend: &Backend, enabled: bool) {
        let health = self.health.load();
        if let Some(health) = health.get(&backend.hash_key()) {
            health.enable(enabled);
            return;
        }

        let Some(Some(hash_key)) = self.backend_keys.load().get(&backend.addr).copied() else {
            return;
        };
        if let Some(health) = health.get(&hash_key) {
            health.enable(enabled)
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
                    check.health_status_change(backend, errored.is_none()).await;
                    let summary = check.backend_summary(backend);
                    if let Some(e) = errored {
                        warn!("{summary} becomes unhealthy, {e}");
                    } else {
                        info!("{summary} becomes healthy");
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

/// Timing information from the most recent [`LoadBalancer::update`] or
/// [`LoadBalancerGroup::update`] call.
#[derive(Debug, Clone, Copy)]
pub struct UpdateTimings {
    /// Time spent in [`ServiceDiscovery::discover`].
    pub discovery_duration: Duration,
    /// Time spent building the selection algorithm and storing the updated backends.
    pub build_duration: Duration,
}

/// A [LoadBalancer] instance contains the service discovery, health check and backend selection
/// all together.
///
/// In order to run service discovery and health check at the designated frequencies, the [LoadBalancer]
/// needs to be run as a [pingora_core::services::background::BackgroundService].
pub struct LoadBalancer<S>
where
    S: BackendSelection,
{
    backends: Backends,
    selector: ArcSwap<S>,

    config: Option<S::Config>,

    /// Timing information from the most recent [`update`](Self::update) call.
    ///
    /// `None` until the first successful update completes.
    last_update_timing: ArcSwap<Option<UpdateTimings>>,

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

/// One selector configuration and its currently published selection data.
struct SelectorSlot<S>
where
    S: BackendSelection,
{
    selector: ArcSwap<S>,
    config: Option<S::Config>,
}

/// A collection of load-balancing selectors that share one backend pool.
///
/// Service discovery, health checks, enablement, and backend membership are
/// shared. When membership changes, all selectors are rebuilt before the new
/// membership is published.
pub struct LoadBalancerGroup<S>
where
    S: BackendSelection,
{
    backends: Backends,
    selectors: Box<[SelectorSlot<S>]>,

    /// Timing information from the most recent [`update`](Self::update) call.
    ///
    /// `None` until the first successful update completes.
    last_update_timing: ArcSwap<Option<UpdateTimings>>,

    /// How frequently the health check logic (if set) should run.
    ///
    /// If `None`, the health check logic will only run once at the beginning.
    pub health_check_frequency: Option<Duration>,
    /// How frequently service discovery should run.
    ///
    /// If `None`, service discovery will only run once at the beginning.
    pub update_frequency: Option<Duration>,
    /// Whether to run health checks for all backends in parallel. Default is false.
    pub parallel_health_check: bool,
}

/// Build one selector with either its explicit configuration or the algorithm default.
fn build_selector<S>(backends: &BTreeSet<Backend>, config: Option<&S::Config>) -> S
where
    S: BackendSelection,
{
    if let Some(config) = config {
        S::build_with_config(backends, config)
    } else {
        S::build(backends)
    }
}

impl<S> LoadBalancer<S>
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

    /// Build a [LoadBalancer] with the given [Backends] and the config.
    pub fn from_backends_with_config(backends: Backends, config_opt: Option<S::Config>) -> Self {
        let selector_raw = build_selector::<S>(&backends.get_backend(), config_opt.as_ref());

        let selector = ArcSwap::new(Arc::new(selector_raw));

        LoadBalancer {
            backends,
            selector,
            config: config_opt,
            last_update_timing: ArcSwap::new(Arc::new(None)),
            health_check_frequency: None,
            update_frequency: None,
            parallel_health_check: false,
        }
    }

    /// Build a [LoadBalancer] with the given [Backends].
    pub fn from_backends(backends: Backends) -> Self {
        Self::from_backends_with_config(backends, None)
    }

    /// Run the service discovery and update the selection algorithm.
    ///
    /// This function will be called every `update_frequency` if this [LoadBalancer] instance
    /// is running as a background service.
    ///
    /// On success, the timing information from this call is stored and can be
    /// retrieved via [`last_update_timing`](Self::last_update_timing).
    pub async fn update(&self) -> Result<()> {
        use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

        let build_nanos = AtomicU64::new(0);
        let total_start = Instant::now();

        self.backends
            .update(|backends| {
                let build_start = Instant::now();
                let selector = build_selector::<S>(&backends, self.config.as_ref());
                self.selector.store(Arc::new(selector));
                build_nanos.store(build_start.elapsed().as_nanos() as u64, Relaxed);
            })
            .await?;

        let total = total_start.elapsed();
        let build = Duration::from_nanos(build_nanos.load(Relaxed));

        self.last_update_timing.store(Arc::new(Some(UpdateTimings {
            discovery_duration: total.saturating_sub(build),
            build_duration: build,
        })));

        Ok(())
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

    /// Return the timing information from the most recent successful [`update`](Self::update) call.
    ///
    /// Returns `None` if [`update`](Self::update) has never completed successfully.
    pub fn last_update_timing(&self) -> Option<UpdateTimings> {
        **self.last_update_timing.load()
    }
}

impl<S> LoadBalancerGroup<S>
where
    S: BackendSelection + 'static,
    S::Iter: BackendIter,
{
    /// Build a group of selectors over one shared backend pool.
    ///
    /// Each item in `configs` creates one selector. `None` uses
    /// [`BackendSelection::build`], while `Some(config)` uses
    /// [`BackendSelection::build_with_config`].
    pub fn from_backends_with_configs(
        backends: Backends,
        configs: impl IntoIterator<Item = Option<S::Config>>,
    ) -> Self {
        let current_backends = backends.get_backend();
        let selectors = configs
            .into_iter()
            .map(|config| SelectorSlot {
                selector: ArcSwap::new(Arc::new(build_selector::<S>(
                    &current_backends,
                    config.as_ref(),
                ))),
                config,
            })
            .collect();

        Self {
            backends,
            selectors,
            last_update_timing: ArcSwap::new(Arc::new(None)),
            health_check_frequency: None,
            update_frequency: None,
            parallel_health_check: false,
        }
    }

    /// Return the number of selectors in this group.
    pub fn selector_count(&self) -> usize {
        self.selectors.len()
    }

    /// Run service discovery and rebuild all selectors when membership changes.
    ///
    /// On success, the timing information from this call is stored and can be
    /// retrieved via [`last_update_timing`](Self::last_update_timing).
    pub async fn update(&self) -> Result<()> {
        use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

        let build_nanos = AtomicU64::new(0);
        let total_start = Instant::now();

        self.backends
            .update(|backends| {
                let build_start = Instant::now();
                let selectors: Vec<S> = self
                    .selectors
                    .iter()
                    .map(|slot| build_selector::<S>(&backends, slot.config.as_ref()))
                    .collect();

                for (slot, selector) in self.selectors.iter().zip(selectors) {
                    slot.selector.store(Arc::new(selector));
                }
                build_nanos.store(build_start.elapsed().as_nanos() as u64, Relaxed);
            })
            .await?;

        let total = total_start.elapsed();
        let build = Duration::from_nanos(build_nanos.load(Relaxed));
        self.last_update_timing.store(Arc::new(Some(UpdateTimings {
            discovery_duration: total.saturating_sub(build),
            build_duration: build,
        })));

        Ok(())
    }

    /// Return the first healthy backend from the selected load-balancing configuration.
    ///
    /// Returns `None` when `selector_index` is out of bounds.
    pub fn select(
        &self,
        selector_index: usize,
        key: &[u8],
        max_iterations: usize,
    ) -> Option<Backend> {
        self.select_with(selector_index, key, max_iterations, |_, health| health)
    }

    /// Select a backend using one selector and an additional acceptance function.
    ///
    /// All selectors consult the same backend readiness state. Returns `None`
    /// when `selector_index` is out of bounds.
    pub fn select_with<F>(
        &self,
        selector_index: usize,
        key: &[u8],
        max_iterations: usize,
        accept: F,
    ) -> Option<Backend>
    where
        F: Fn(&Backend, bool) -> bool,
    {
        let selection = self.selectors.get(selector_index)?.selector.load();
        let mut iter = UniqueIterator::new(selection.iter(key), max_iterations);
        while let Some(backend) = iter.get_next() {
            if accept(&backend, self.backends.ready(&backend)) {
                return Some(backend);
            }
        }
        None
    }

    /// Set the health check implementation shared by every selector.
    pub fn set_health_check(
        &mut self,
        hc: Box<dyn health_check::HealthCheck + Send + Sync + 'static>,
    ) {
        self.backends.set_health_check(hc);
    }

    /// Access the shared backend pool.
    pub fn backends(&self) -> &Backends {
        &self.backends
    }

    /// Return timing information from the most recent successful [`update`](Self::update) call.
    ///
    /// Returns `None` if [`update`](Self::update) has never completed successfully.
    pub fn last_update_timing(&self) -> Option<UpdateTimings> {
        **self.last_update_timing.load()
    }
}

#[cfg(test)]
mod test {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed};
    use std::sync::{Condvar, Mutex};

    use super::*;
    use async_trait::async_trait;
    use pingora_core::services::ServiceReadyNotifier;

    #[derive(Default)]
    struct BuildTracker {
        blocked: Mutex<bool>,
        unblocked: Condvar,
        active: AtomicUsize,
    }

    impl BuildTracker {
        fn run_build(&self) {
            self.active.fetch_add(1, Relaxed);

            let mut blocked = self
                .blocked
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            while *blocked {
                blocked = self
                    .unblocked
                    .wait(blocked)
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
            }
            self.active.fetch_sub(1, Relaxed);
        }

        fn block(&self) {
            *self
                .blocked
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = true;
        }

        fn unblock(&self) {
            *self
                .blocked
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = false;
            self.unblocked.notify_all();
        }

        fn reset(&self) {
            self.active.store(0, Relaxed);
        }
    }

    #[derive(Clone)]
    struct TestSelectionConfig {
        reverse: bool,
        tracker: Option<Arc<BuildTracker>>,
    }

    struct TestSelection {
        backends: Vec<Backend>,
    }

    struct TestSelectionIter {
        selection: Arc<TestSelection>,
        index: usize,
    }

    impl BackendIter for TestSelectionIter {
        fn next(&mut self) -> Option<&Backend> {
            let backend = self.selection.backends.get(self.index);
            self.index += 1;
            backend
        }
    }

    impl BackendSelection for TestSelection {
        type Iter = TestSelectionIter;
        type Config = TestSelectionConfig;

        fn build_with_config(backends: &BTreeSet<Backend>, config: &Self::Config) -> Self {
            if let Some(tracker) = &config.tracker {
                tracker.run_build();
            }
            let mut backends: Vec<_> = backends.iter().cloned().collect();
            if config.reverse {
                backends.reverse();
            }
            Self { backends }
        }

        fn build(backends: &BTreeSet<Backend>) -> Self {
            Self {
                backends: backends.iter().cloned().collect(),
            }
        }

        fn iter(self: &Arc<Self>, _key: &[u8]) -> Self::Iter {
            TestSelectionIter {
                selection: self.clone(),
                index: 0,
            }
        }
    }

    struct MutableDiscovery {
        backends: ArcSwap<BTreeSet<Backend>>,
    }

    impl MutableDiscovery {
        fn new(backends: BTreeSet<Backend>) -> Self {
            Self {
                backends: ArcSwap::new(Arc::new(backends)),
            }
        }

        fn add(&self, backend: Backend) {
            let mut backends = BTreeSet::clone(&self.backends.load());
            backends.insert(backend);
            self.backends.store(Arc::new(backends));
        }

        fn replace(&self, backends: BTreeSet<Backend>) {
            self.backends.store(Arc::new(backends));
        }
    }

    #[async_trait]
    impl ServiceDiscovery for Arc<MutableDiscovery> {
        async fn discover(&self) -> Result<(BTreeSet<Backend>, HashMap<u64, bool>)> {
            Ok((BTreeSet::clone(&self.backends.load()), HashMap::new()))
        }
    }

    struct FailThenWaitDiscovery {
        backend: Backend,
        attempts: AtomicUsize,
        allow_success: tokio::sync::Notify,
    }

    #[async_trait]
    impl ServiceDiscovery for Arc<FailThenWaitDiscovery> {
        async fn discover(&self) -> Result<(BTreeSet<Backend>, HashMap<u64, bool>)> {
            let attempt = self.attempts.fetch_add(1, Relaxed);
            if attempt == 0 {
                return Err(pingora_error::Error::explain(
                    ErrorType::InternalError,
                    "intentional discovery failure",
                ));
            }
            if attempt == 1 {
                self.allow_success.notified().await;
            }
            Ok((BTreeSet::from([self.backend.clone()]), HashMap::new()))
        }
    }

    struct CountingHealthCheck {
        checks: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl health_check::HealthCheck for CountingHealthCheck {
        async fn check(&self, _target: &Backend) -> Result<()> {
            self.checks.fetch_add(1, Relaxed);
            Ok(())
        }

        fn health_threshold(&self, _success: bool) -> usize {
            1
        }
    }

    async fn wait_for_active_builds(tracker: &BuildTracker, expected: usize) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while tracker.active.load(Relaxed) != expected {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("selector builds did not start");
    }

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

    #[tokio::test]
    async fn test_lb_update_stores_timing() {
        let discovery = discovery::Static::default();
        let b1 = Backend::new("1.1.1.1:80").unwrap();
        let b2 = Backend::new("1.0.0.1:80").unwrap();
        discovery.add(b1.clone());
        discovery.add(b2.clone());

        let lb = LoadBalancer::<selection::RoundRobin>::from_backends(Backends::new(Box::new(
            discovery,
        )));

        // Before first update, timing should be None
        assert!(lb.last_update_timing().is_none());

        lb.update().await.unwrap();

        // After update, timing should be populated
        let timing = lb
            .last_update_timing()
            .expect("timing should be Some after update");
        assert!(timing.discovery_duration > Duration::ZERO);
        assert!(timing.build_duration > Duration::ZERO);

        // Backends should be populated
        let backend = lb.backends().get_backend();
        assert!(backend.contains(&b1));
        assert!(backend.contains(&b2));

        // Selection should work
        assert!(lb.select(b"test", 10).is_some());
    }

    #[tokio::test]
    async fn test_load_balancer_group_rebuilds_all_selectors() {
        let b1 = Backend::new("1.0.0.1:80").unwrap();
        let b2 = Backend::new("1.1.1.1:80").unwrap();
        let b3 = Backend::new("1.1.1.2:80").unwrap();
        let discovery = Arc::new(MutableDiscovery::new(BTreeSet::from([
            b1.clone(),
            b2.clone(),
        ])));

        let group = LoadBalancerGroup::<TestSelection>::from_backends_with_configs(
            Backends::new(Box::new(discovery.clone())),
            [
                Some(TestSelectionConfig {
                    reverse: false,
                    tracker: None,
                }),
                Some(TestSelectionConfig {
                    reverse: true,
                    tracker: None,
                }),
            ],
        );

        assert_eq!(group.selector_count(), 2);
        group.update().await.unwrap();
        assert_eq!(group.select(0, b"", 10), Some(b1.clone()));
        assert_eq!(group.select(1, b"", 10), Some(b2));

        discovery.add(b3.clone());
        group.update().await.unwrap();
        assert_eq!(group.select(0, b"", 10), Some(b1));
        assert_eq!(group.select(1, b"", 10), Some(b3));
        let timing = group
            .last_update_timing()
            .expect("timing should be Some after update");
        assert!(timing.build_duration > Duration::ZERO);
        assert_eq!(group.select(2, b"", 10), None);
    }

    #[tokio::test]
    async fn test_load_balancer_group_shares_health_checks() {
        let b1 = Backend::new("1.0.0.1:80").unwrap();
        let b2 = Backend::new("1.1.1.1:80").unwrap();
        let discovery = Arc::new(MutableDiscovery::new(BTreeSet::from([
            b1.clone(),
            b2.clone(),
        ])));
        let checks = Arc::new(AtomicUsize::new(0));

        let mut group = LoadBalancerGroup::<TestSelection>::from_backends_with_configs(
            Backends::new(Box::new(discovery)),
            [
                Some(TestSelectionConfig {
                    reverse: false,
                    tracker: None,
                }),
                Some(TestSelectionConfig {
                    reverse: false,
                    tracker: None,
                }),
            ],
        );
        group.set_health_check(Box::new(CountingHealthCheck {
            checks: checks.clone(),
        }));

        group.update().await.unwrap();
        group.backends().run_health_check(false).await;

        assert_eq!(checks.load(Relaxed), 2);
        group.backends().set_enable(&b1, false);
        assert_eq!(group.select(0, b"", 10), Some(b2.clone()));
        assert_eq!(group.select(1, b"", 10), Some(b2));
    }

    #[tokio::test]
    async fn test_readiness_uses_current_backend_for_stale_selector_entries() {
        let old_backend = Backend::new_with_weight("1.0.0.1:80", 1).unwrap();
        let new_backend = Backend::new_with_weight("1.0.0.1:80", 2).unwrap();
        let discovery = Arc::new(MutableDiscovery::new(BTreeSet::from([old_backend.clone()])));
        let backends = Backends::new(Box::new(discovery.clone()));

        backends.update(|_| {}).await.unwrap();
        assert!(backends.ready(&old_backend));

        discovery.replace(BTreeSet::from([new_backend.clone()]));
        backends.update(|_| {}).await.unwrap();
        assert!(backends.ready(&old_backend));
        assert!(backends.ready(&new_backend));

        discovery.replace(BTreeSet::new());
        backends.update(|_| {}).await.unwrap();
        assert!(!backends.ready(&old_backend));
        assert!(!backends.ready(&new_backend));
    }

    #[tokio::test]
    async fn test_stale_backend_address_fallback_fails_closed_when_ambiguous() {
        let backend_v1 = Backend::new_with_weight("1.0.0.1:80", 1).unwrap();
        let backend_v2 = Backend::new_with_weight("1.0.0.1:80", 2).unwrap();
        let stale_backend = Backend::new_with_weight("1.0.0.1:80", 3).unwrap();
        let discovery = Arc::new(MutableDiscovery::new(BTreeSet::from([
            backend_v1.clone(),
            backend_v2.clone(),
        ])));
        let backends = Backends::new(Box::new(discovery));

        backends.update(|_| {}).await.unwrap();
        assert!(backends.ready(&backend_v1));
        assert!(backends.ready(&backend_v2));
        assert!(!backends.ready(&stale_backend));

        backends.set_enable(&stale_backend, false);
        assert!(backends.ready(&backend_v1));
        assert!(backends.ready(&backend_v2));

        backends.set_enable(&backend_v1, false);
        assert!(!backends.ready(&backend_v1));
        assert!(backends.ready(&backend_v2));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_load_balancer_group_waits_for_initial_selectors_before_ready() {
        let backend = Backend::new("1.0.0.1:80").unwrap();
        let discovery = Arc::new(MutableDiscovery::new(BTreeSet::from([backend])));
        let tracker = Arc::new(BuildTracker::default());
        let group = Arc::new(
            LoadBalancerGroup::<TestSelection>::from_backends_with_configs(
                Backends::new(Box::new(discovery)),
                [Some(TestSelectionConfig {
                    reverse: false,
                    tracker: Some(Arc::clone(&tracker)),
                })],
            ),
        );
        tracker.reset();
        tracker.block();

        let (_shutdown_tx, shutdown) = tokio::sync::watch::channel(false);
        let (ready_tx, mut ready_rx) = tokio::sync::watch::channel(false);
        let run_group = Arc::clone(&group);
        let run_task = tokio::spawn(async move {
            run_group
                .run(shutdown, Some(ServiceReadyNotifier::new(ready_tx)))
                .await;
        });

        wait_for_active_builds(&tracker, 1).await;
        assert!(!*ready_rx.borrow());

        tracker.unblock();
        tokio::time::timeout(Duration::from_secs(5), ready_rx.changed())
            .await
            .expect("ready notification timed out")
            .expect("ready notifier was dropped");
        assert!(*ready_rx.borrow());
        run_task.await.expect("group background task failed");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_load_balancer_group_waits_for_successful_update_before_ready() {
        let discovery = Arc::new(FailThenWaitDiscovery {
            backend: Backend::new("1.0.0.1:80").unwrap(),
            attempts: AtomicUsize::new(0),
            allow_success: tokio::sync::Notify::new(),
        });
        let mut group = LoadBalancerGroup::<TestSelection>::from_backends_with_configs(
            Backends::new(Box::new(Arc::clone(&discovery))),
            [Some(TestSelectionConfig {
                reverse: false,
                tracker: None,
            })],
        );
        group.update_frequency = Some(Duration::from_millis(1));
        let group = Arc::new(group);

        let (shutdown_tx, shutdown) = tokio::sync::watch::channel(false);
        let (ready_tx, mut ready_rx) = tokio::sync::watch::channel(false);
        let run_group = Arc::clone(&group);
        let run_task = tokio::spawn(async move {
            run_group
                .run(shutdown, Some(ServiceReadyNotifier::new(ready_tx)))
                .await;
        });

        tokio::time::timeout(Duration::from_secs(5), async {
            while discovery.attempts.load(Relaxed) < 2 {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("second discovery attempt did not start");
        assert!(!*ready_rx.borrow());

        discovery.allow_success.notify_one();
        tokio::time::timeout(Duration::from_secs(5), ready_rx.changed())
            .await
            .expect("ready notification timed out")
            .expect("ready notifier was dropped");
        assert!(*ready_rx.borrow());

        shutdown_tx.send(true).unwrap();
        tokio::time::timeout(Duration::from_secs(5), run_task)
            .await
            .expect("group background task did not stop")
            .expect("group background task failed");
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
