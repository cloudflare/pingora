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
//!
//! ## Grouped selector internals
//!
//! In `LoadBalancerGroup<S>`, `S` is the selector algorithm and its built data,
//! such as a Ketama ring. The other types manage its configuration, rebuilding,
//! publication, and lifetime.
//!
//! ```text
//! LoadBalancerGroup<S>
//! |-- BackendView (Backends)
//! |   |-- ServiceDiscovery
//! |   `-- Arc<HealthRegistry>
//! |-- SelectorRebuildGate
//! |-- SelectorRebuildCancellation
//! `-- SelectorSlot<S> x N
//!     |-- config
//!     |-- SelectorRebuildState
//!     |   `-- pending SelectorRebuildRequest
//!     `-- ArcSwap<PublishedSelector<S>>
//!         |-- Arc<S>
//!         |-- readiness snapshot
//!         `-- SelectorReleaseGuard
//!             `-- SelectorReleaseSignal
//!
//! Shared mode:
//! HealthCheckService ---> Arc<HealthRegistry> <--- other BackendViews
//! ```
//!
//! The main roles are:
//!
//! - `Backends`, also named `BackendView`, owns discovered membership,
//!   enablement, health references, and the membership generation. Each
//!   published group selector owns the readiness snapshot for its generation,
//!   so older selectors keep serving their own snapshot.
//! - `HealthRegistry` reconciles the targets contributed by its views and owns
//!   one health state and probe target per backend equivalence key.
//! - `HealthCheckService` runs one active health-check loop for a shared
//!   registry. Views with private registries are checked by their load balancer.
//! - `SelectorSlot<S>` owns one selector configuration, its published selector,
//!   generation, pending work, timings, and counters.
//! - `SelectorRebuildRequest` contains the backend membership, its readiness
//!   snapshot, and generation to build.
//! - `SelectorRebuildState` tracks the active rebuild task and newest pending
//!   request.
//! - `SelectorRebuildTaskGuard` clears the running state and restores an
//!   in-flight request if the task exits unexpectedly.
//! - `SelectorRebuildCancellation` stops rebuild tasks when the group is
//!   dropped.
//! - `PublishedSelector<S>` pairs the selector exposed to requests with the
//!   readiness snapshot for its generation and its lifetime tracking.
//! - `SelectorReleaseGuard` and `SelectorReleaseSignal` notify the gate after a
//!   replaced selector and all its readers are gone.
//! - `SelectorRebuildGate` allows one build at a time and prevents another build
//!   while an old selector is still being destroyed.
//!
//! ### Discovery and shared-health flow
//!
//! 1. `LoadBalancerGroup<S>` asks its `BackendView` to update.
//! 2. The view's `ServiceDiscovery` returns its current `Backend` membership and
//!    enablement.
//! 3. `BackendView` publishes that membership and updates its contribution to
//!    `HealthRegistry`.
//! 4. `HealthRegistry` reconciles the targets from all of its views.
//! 5. In shared mode, `HealthCheckService` probes each registry target once.
//! 6. The resulting health state is visible through every contributing view.
//! 7. Each view applies its own membership and enablement. Each rebuilt group
//!    selector is published with the readiness snapshot for its generation.
//!
//! ### Selector rebuild flow
//!
//! 1. `Backends` advances the membership generation and produces an indivisible
//!    membership and readiness update bundle.
//! 2. `LoadBalancerGroup<S>` schedules each selector rebuild from that bundle.
//! 3. `SelectorRebuildState` keeps one active rebuild and coalesces newer work
//!    into its pending request.
//! 4. `SelectorRebuildTaskGuard` tracks the in-flight request while the task
//!    acquires `SelectorRebuildGate`.
//! 5. The task builds the selector `S` from the request's backend snapshot.
//! 6. A new `PublishedSelector<S>` is stored in the slot's `ArcSwap`, replacing
//!    the old published selector atomically.
//! 7. The slot publishes its selector generation and can process its next
//!    pending request.
//!
//! ### Request flow
//!
//! 1. `LoadBalancerGroup<S>::select` loads a `PublishedSelector<S>` from the
//!    chosen `SelectorSlot<S>`.
//! 2. That published selector snapshot is held for the whole selection while it
//!    yields ordered backend candidates.
//! 3. The published selector's own readiness snapshot answers enablement and
//!    health for each candidate.
//! 4. The first accepted backend is returned; otherwise selection returns
//!    `None`.
//! 5. Replacing the selector does not affect this request's iterator.
//! 6. When the final reader releases the old `PublishedSelector<S>`, its
//!    `SelectorReleaseGuard` updates `SelectorReleaseSignal`.
//! 7. `SelectorRebuildGate` observes that signal and permits the next build.
//!
//! ### Cancellation flow
//!
//! 1. Dropping `LoadBalancerGroup<S>` triggers `SelectorRebuildCancellation`.
//! 2. A task waiting for `SelectorRebuildGate` exits without removing the old
//!    selector's `SelectorReleaseSignal`.
//! 3. `SelectorRebuildTaskGuard` restores an in-flight
//!    `SelectorRebuildRequest` unless a newer request already replaced it.
//! 4. A built but unpublished selector is destroyed on a blocking worker.
//! 5. That destruction retains the gate permit, so another group cannot build
//!    at the same time.
//! 6. The task guard clears the slot's running state and notifies waiters.
//! 7. After destruction completes, the gate permit is released.

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
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::future::Future;
use std::hash::{Hash, Hasher};
use std::io::Result as IoResult;
use std::net::ToSocketAddrs;
use std::sync::atomic::{
    AtomicBool, AtomicU64,
    Ordering::{Acquire, Relaxed, Release},
};
use std::sync::{mpsc, Arc, Mutex, MutexGuard, OnceLock, Weak};
use std::time::{Duration, Instant};
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore};

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
    pub use crate::{BackendView, HealthCheckService, HealthRegistry, LoadBalancer};
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

/// The backends to check and their health.
///
/// Both are updated together. If multiple backends have the same health key,
/// only one is checked and they all share one health value.
struct HealthRegistryState {
    targets: Box<[Backend]>,
    /// Health handles keyed by the registry's health key (see
    /// [`HealthRegistry::health_key`]). The default key uses full backend
    /// identity; callers can supply a different key when constructing a
    /// registry.
    health: HashMap<u64, Health>,
}

/// Active health state shared by one or more [`BackendView`]s.
///
/// Registries use full backend identity by default. Callers can supply an
/// equivalence key to intentionally share probes across backend variants.
pub struct HealthRegistry {
    health_check: OnceLock<Arc<dyn health_check::HealthCheck + Send + Sync + 'static>>,
    state: ArcSwap<HealthRegistryState>,
    /// View changes are applied one at a time so concurrent updates are not
    /// lost. Request handling does not read this map.
    views: Mutex<BTreeMap<u64, Arc<BTreeSet<Backend>>>>,
    /// Assigns each view the ID used to update and remove its backend set.
    next_view_id: AtomicU64,
    /// Queues view removals so dropping a view never waits for reconciliation.
    view_removal_tx: mpsc::Sender<u64>,
    /// View removals waiting to be applied during the next registry operation.
    pending_view_removals: Mutex<mpsc::Receiver<u64>>,
    /// Wakes the health service so queued removals are applied promptly.
    view_removed: Notify,
    /// Wakes the health service when the registry gains its first target.
    targets_available: Notify,
    /// Backends with the same key share one health check and health value.
    equivalence_key: Arc<dyn Fn(&Backend) -> u64 + Send + Sync + 'static>,
}

impl Default for HealthRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl HealthRegistry {
    /// Create an empty registry keyed by full backend identity.
    pub fn new() -> Self {
        Self::with_equivalence(|backend| backend.hash_key())
    }

    /// Create an empty registry using `equivalence_key` to group targets.
    ///
    /// Backends that produce the same `u64` key share one health state and
    /// probe. Callers are responsible for ensuring that backends which need
    /// independent health state produce distinct keys.
    pub fn with_equivalence(
        equivalence_key: impl Fn(&Backend) -> u64 + Send + Sync + 'static,
    ) -> Self {
        let (view_removal_tx, pending_view_removals) = mpsc::channel();
        Self {
            health_check: OnceLock::new(),
            state: ArcSwap::new(Arc::new(HealthRegistryState {
                targets: Vec::new().into_boxed_slice(),
                health: HashMap::new(),
            })),
            views: Mutex::new(BTreeMap::new()),
            next_view_id: AtomicU64::new(1),
            view_removal_tx,
            pending_view_removals: Mutex::new(pending_view_removals),
            view_removed: Notify::new(),
            targets_available: Notify::new(),
            equivalence_key: Arc::new(equivalence_key),
        }
    }

    /// The key under which a backend's shared [`Health`] is tracked.
    fn health_key(&self, backend: &Backend) -> u64 {
        (self.equivalence_key)(backend)
    }

    /// Set the health-check implementation used by every view in this registry.
    ///
    /// # Panics
    ///
    /// Panics if a health check has already been set.
    pub fn set_health_check(&self, hc: Box<dyn health_check::HealthCheck + Send + Sync + 'static>) {
        assert!(
            self.health_check.set(hc.into()).is_ok(),
            "health check already configured"
        );
    }

    fn has_health_check(&self) -> bool {
        self.health_check.get().is_some()
    }

    fn register_view(&self) -> u64 {
        self.apply_pending_view_removals();
        let view_id = self.next_view_id.fetch_add(1, Relaxed);
        self.views
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(view_id, Arc::new(BTreeSet::new()));
        view_id
    }

    /// Publish `backends` as `view_id`'s membership and return the reconciled
    /// registry state so the caller can snapshot the shared health handles for
    /// its backends without a second registry load.
    fn update_view(
        &self,
        view_id: u64,
        backends: Arc<BTreeSet<Backend>>,
    ) -> Arc<HealthRegistryState> {
        self.apply_pending_view_removals();
        let mut views = self
            .views
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        views.insert(view_id, backends);
        self.reconcile(&views)
    }

    fn defer_view_removal(&self, view_id: u64) {
        let _ = self.view_removal_tx.send(view_id);
        self.view_removed.notify_one();
    }

    fn apply_pending_view_removals(&self) {
        let view_ids: Vec<_> = self
            .pending_view_removals
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .try_iter()
            .collect();
        if view_ids.is_empty() {
            return;
        }

        let mut views = self
            .views
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut changed = false;
        for view_id in view_ids {
            changed |= views.remove(&view_id).is_some();
        }
        if changed {
            self.reconcile(&views);
        }
    }

    /// Wait until a view removal is queued. The caller must then apply pending removals.
    pub(crate) async fn wait_for_view_removal(&self) {
        self.view_removed.notified().await;
    }

    fn reconcile(&self, views: &BTreeMap<u64, Arc<BTreeSet<Backend>>>) -> Arc<HealthRegistryState> {
        let old_state = self.state.load();
        let mut targets_by_key = HashMap::new();
        for backends in views.values() {
            for backend in backends.iter() {
                targets_by_key
                    .entry(self.health_key(backend))
                    .or_insert_with(|| backend.clone());
            }
        }

        let mut targets: Vec<_> = targets_by_key.into_values().collect();
        targets.sort_unstable();
        let gained_first_target = old_state.targets.is_empty() && !targets.is_empty();
        let mut health = HashMap::with_capacity(targets.len());
        for backend in &targets {
            let key = self.health_key(backend);
            health.insert(key, old_state.health.get(&key).cloned().unwrap_or_default());
        }

        let new_state = Arc::new(HealthRegistryState {
            targets: targets.into_boxed_slice(),
            health,
        });
        self.state.store(Arc::clone(&new_state));
        if gained_first_target {
            self.targets_available.notify_one();
        }
        new_state
    }

    /// Return the number of distinct backend targets currently being tracked.
    ///
    /// Targets that produce the same registry health key count as one target.
    pub fn target_count(&self) -> usize {
        self.apply_pending_view_removals();
        self.state.load().targets.len()
    }

    /// Wait until at least one registered view contributes a backend target.
    pub(crate) async fn wait_for_targets(&self) {
        loop {
            let notified = self.targets_available.notified();
            // `enable()` requires a pinned future because it registers it in
            // `Notify`'s waiter list. Register first so a new target is not missed.
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.target_count() > 0 {
                return;
            }
            notified.await;
        }
    }

    /// Run one active health-check pass over the union of all registered views.
    ///
    /// When `parallel` is true, all targets are checked concurrently.
    pub async fn run_health_check(&self, parallel: bool) {
        use crate::health_check::HealthCheck;
        use log::{info, warn};
        use pingora_runtime::current_handle;

        async fn check_and_report(
            backend: &Backend,
            check: &Arc<dyn HealthCheck + Send + Sync>,
            health: &Health,
        ) {
            let errored = check.check(backend).await.err();
            let healthy = errored.is_none();
            let flipped = health.observe_health(healthy, check.health_threshold(healthy));
            if flipped {
                check.health_status_change(backend, healthy).await;
                let summary = check.backend_summary(backend);
                if let Some(error) = errored {
                    warn!("{summary} becomes unhealthy, {error}");
                } else {
                    info!("{summary} becomes healthy");
                }
            }
        }

        self.apply_pending_view_removals();
        let Some(health_check) = self.health_check.get().cloned() else {
            // nothing to do
            return;
        };

        let state = self.state.load_full();
        if parallel {
            let runtime = current_handle();
            let jobs = state.targets.iter().map(|backend| {
                let backend = backend.clone();
                let key = self.health_key(&backend);
                let check = Arc::clone(&health_check);
                let state = Arc::clone(&state);
                runtime.spawn(async move {
                    if let Some(health) = state.health.get(&key) {
                        check_and_report(&backend, &check, health).await;
                    }
                })
            });
            futures::future::join_all(jobs).await;
        } else {
            for backend in state.targets.iter() {
                if let Some(health) = state.health.get(&self.health_key(backend)) {
                    check_and_report(backend, &health_check, health).await;
                }
            }
        }
    }
}

/// View-local enablement paired with the shared [`Health`] handle for one
/// backend identity.
///
/// Caching the [`Health`] handle in a [`ReadinessSnapshot`] lets readiness be
/// answered with a single map lookup, avoiding a snapshot of the
/// [`HealthRegistry`]. The handle shares its inner state with the registry, so
/// health observations and reconciliation remain visible through it, including
/// through a snapshot retained by an older selector.
#[derive(Clone)]
struct BackendReadiness {
    enabled: Arc<AtomicBool>,
    health: Health,
}

impl BackendReadiness {
    fn ready(&self) -> bool {
        self.enabled.load(Relaxed) && self.health.ready()
    }
}

/// An immutable, readiness-only snapshot of one backend view generation.
///
/// Readiness is indexed by [`Backend::hash_key`]. Cloning shares the underlying
/// map through an [`Arc`]; each [`BackendReadiness`] keeps the shared live
/// [`Health`] handle and the [`struct@AtomicBool`] enablement flag, so health
/// observations and manual enable/disable stay visible through every clone of a
/// generation's snapshot, including one owned by an older selector.
#[derive(Clone)]
struct ReadinessSnapshot(Arc<HashMap<u64, BackendReadiness>>);

impl ReadinessSnapshot {
    /// A snapshot with no known backends.
    fn empty() -> Self {
        Self(Arc::new(HashMap::new()))
    }

    /// Look up readiness by full backend identity.
    fn get(&self, backend: &Backend) -> Option<&BackendReadiness> {
        self.0.get(&backend.hash_key())
    }

    /// Whether the backend is present in this snapshot and both enabled and
    /// healthy.
    fn ready(&self, backend: &Backend) -> bool {
        self.get(backend).is_some_and(BackendReadiness::ready)
    }
}

/// One published snapshot of a backend view: its current membership paired with
/// the readiness snapshot for that generation.
struct BackendViewState {
    /// The backend membership exposed by this snapshot.
    backends: Arc<BTreeSet<Backend>>,
    /// Readiness for the current membership, keyed by full backend identity.
    readiness: ReadinessSnapshot,
}

/// An indivisible membership and readiness update produced by a membership
/// change.
///
/// Groups schedule selector rebuilds from this bundle so each rebuilt selector
/// is published with the exact readiness snapshot for its generation, keeping
/// membership, readiness, and generation consistent across coalescing, retries,
/// and cancellation.
struct BackendUpdate {
    /// Membership generation this update advanced to.
    generation: u64,
    /// Membership captured for this generation.
    backends: Arc<BTreeSet<Backend>>,
    /// Readiness snapshot for this generation.
    readiness: ReadinessSnapshot,
}

/// A discovered backend membership with view-local enablement and shared
/// active health state.
///
/// Readiness is the conjunction of current view membership, view-local
/// enablement, and the health state in the associated [`HealthRegistry`].
pub struct Backends {
    discovery: Box<dyn ServiceDiscovery + Send + Sync + 'static>,
    health_registry: Arc<HealthRegistry>,
    view_id: u64,
    state: ArcSwap<BackendViewState>,
    /// Membership generation, advanced on each change.
    generation: AtomicU64,
    /// Weak enablement handles keyed by full backend identity.
    ///
    /// Lets [`Backends::set_enable`] reach a removed backend whose enablement
    /// flag is still held by an older selector's readiness snapshot, and lets a
    /// re-added backend recover its manual enablement while an older selector
    /// still references it. Never read on the request path, so it takes a lock
    /// only during membership updates and manual enable/disable. Expired entries
    /// are cleaned up opportunistically on each membership change.
    enablement_handles: Mutex<HashMap<u64, Weak<AtomicBool>>>,
    /// Whether the load balancer owning this view owns its active health checks.
    /// Shared-registry views leave them to [`HealthCheckService`].
    owns_health_checks: bool,
}

/// Explicit name for a [`Backends`] instance used as one membership view.
pub type BackendView = Backends;

impl Backends {
    /// Create a backend view with a private health registry.
    ///
    /// Load balancers constructed from this view retain the existing behavior
    /// of scheduling their own health checks.
    pub fn new(discovery: Box<dyn ServiceDiscovery + Send + Sync + 'static>) -> Self {
        Self::new_inner(discovery, Arc::new(HealthRegistry::new()), true)
    }

    /// Create a backend view that contributes targets to `health_registry`.
    ///
    /// Health checks for shared views must be scheduled once through
    /// [`HealthCheckService`] instead of by every load balancer using the view.
    /// The registry must have a health check configured before that service is
    /// started.
    pub fn new_with_health_registry(
        discovery: Box<dyn ServiceDiscovery + Send + Sync + 'static>,
        health_registry: Arc<HealthRegistry>,
    ) -> Self {
        Self::new_inner(discovery, health_registry, false)
    }

    fn new_inner(
        discovery: Box<dyn ServiceDiscovery + Send + Sync + 'static>,
        health_registry: Arc<HealthRegistry>,
        owns_health_checks: bool,
    ) -> Self {
        let view_id = health_registry.register_view();
        Self {
            discovery,
            health_registry,
            view_id,
            state: ArcSwap::new(Arc::new(BackendViewState {
                backends: Arc::new(BTreeSet::new()),
                readiness: ReadinessSnapshot::empty(),
            })),
            generation: AtomicU64::new(0),
            enablement_handles: Mutex::new(HashMap::new()),
            owns_health_checks,
        }
    }

    /// The current membership generation, advanced once per membership change.
    pub(crate) fn generation(&self) -> u64 {
        self.generation.load(Relaxed)
    }

    fn lock_enablement_handles(&self) -> MutexGuard<'_, HashMap<u64, Weak<AtomicBool>>> {
        self.enablement_handles
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The readiness snapshot for the currently published generation.
    fn readiness_snapshot(&self) -> ReadinessSnapshot {
        self.state.load().readiness.clone()
    }

    /// Set the health check used by this view's entire health registry.
    pub fn set_health_check(
        &mut self,
        hc: Box<dyn health_check::HealthCheck + Send + Sync + 'static>,
    ) {
        self.health_registry.set_health_check(hc);
    }

    fn do_update<F>(
        &self,
        new_backends: BTreeSet<Backend>,
        enablement: HashMap<u64, bool>,
        callback: F,
    ) -> Option<BackendUpdate>
    where
        F: FnOnce(Arc<BTreeSet<Backend>>),
    {
        let old_state = self.state.load_full();
        let membership_changed = *old_state.backends != new_backends;
        if membership_changed {
            let generation = self.generation.fetch_add(1, Relaxed) + 1;
            let new_backends = Arc::new(new_backends);
            let registry_state = self
                .health_registry
                .update_view(self.view_id, Arc::clone(&new_backends));

            let mut new_readiness = HashMap::with_capacity(new_backends.len());
            {
                let mut handles = self.lock_enablement_handles();
                // Opportunistically drop enablement handles that no readiness
                // snapshot references anymore.
                handles.retain(|_, weak| weak.strong_count() > 0);
                for backend in new_backends.iter() {
                    let key = backend.hash_key();
                    // Preserve enablement across removal and re-addition while
                    // an older selector's snapshot still holds the flag.
                    let enabled = old_state
                        .readiness
                        .get(backend)
                        .map(|current| Arc::clone(&current.enabled))
                        .or_else(|| handles.get(&key).and_then(Weak::upgrade))
                        .unwrap_or_else(|| Arc::new(AtomicBool::new(true)));
                    if let Some(enabled_override) = enablement.get(&key) {
                        enabled.store(*enabled_override, Relaxed);
                    }
                    handles.insert(key, Arc::downgrade(&enabled));
                    let health = registry_state
                        .health
                        .get(&self.health_registry.health_key(backend))
                        .cloned()
                        .unwrap_or_default();
                    new_readiness.insert(key, BackendReadiness { enabled, health });
                }
            }
            let new_readiness = ReadinessSnapshot(Arc::new(new_readiness));

            // Cover both the old and new readiness during a synchronous selector
            // rebuild so a request in the callback does not lose a backend the
            // about-to-be-replaced selector may still yield. This allocation is
            // linear in old and new membership and only runs on membership changes.
            let mut transition = old_state.readiness.0.as_ref().clone();
            for (key, readiness) in new_readiness.0.iter() {
                transition.insert(*key, readiness.clone());
            }
            self.state.store(Arc::new(BackendViewState {
                backends: Arc::clone(&old_state.backends),
                readiness: ReadinessSnapshot(Arc::new(transition)),
            }));

            callback(Arc::clone(&new_backends));

            self.state.store(Arc::new(BackendViewState {
                backends: Arc::clone(&new_backends),
                readiness: new_readiness.clone(),
            }));
            Some(BackendUpdate {
                generation,
                backends: new_backends,
                readiness: new_readiness,
            })
        } else {
            for (key, enabled) in enablement {
                if let Some(current) = old_state.readiness.0.get(&key) {
                    current.enabled.store(enabled, Relaxed);
                }
            }
            None
        }
    }

    /// Whether `backend` is enabled and healthy in this view's current
    /// membership.
    ///
    /// This is on the hot request path: it takes a single snapshot of the
    /// published view state and performs one map lookup to a `BackendReadiness`
    /// carrying both the view-local enablement flag and the shared `Health`
    /// handle, so no second registry snapshot is required.
    ///
    /// Readiness is keyed by full backend identity. Only the current membership
    /// is reported: a removed backend is not ready here even while an older
    /// group selector can still return it through its own readiness snapshot.
    pub fn ready(&self, backend: &Backend) -> bool {
        self.state.load().readiness.ready(backend)
    }

    /// Manually enable or disable `backend` by full backend identity.
    ///
    /// The current membership is updated in place. A removed backend is reached
    /// through the weak enablement-handle interner, so disabling or re-enabling
    /// a backend still held by an older group selector's readiness snapshot
    /// takes effect for that selector too. Not on the request path, so it may
    /// take the interner lock. If the backend is unknown or no snapshot retains
    /// its enablement flag, this method does nothing.
    pub fn set_enable(&self, backend: &Backend, enabled: bool) {
        let key = backend.hash_key();
        // Current membership shares the same `Arc<AtomicBool>` as the interner
        // and any older snapshot, so flipping it here is sufficient.
        if let Some(readiness) = self.state.load().readiness.0.get(&key) {
            readiness.enabled.store(enabled, Relaxed);
            return;
        }
        // Otherwise reach a removed backend whose flag an older selector's
        // snapshot may still hold.
        if let Some(enabled_flag) = self
            .lock_enablement_handles()
            .get(&key)
            .and_then(Weak::upgrade)
        {
            enabled_flag.store(enabled, Relaxed);
        }
    }

    /// Return this view's current backend membership.
    pub fn get_backend(&self) -> Arc<BTreeSet<Backend>> {
        Arc::clone(&self.state.load().backends)
    }

    /// Run discovery and invoke `callback` when membership changes.
    ///
    /// Calls on the same backend view must not overlap with another update.
    pub async fn update<F>(&self, callback: F) -> Result<()>
    where
        F: FnOnce(Arc<BTreeSet<Backend>>),
    {
        let (new_backends, enablement) = self.discovery.discover().await?;
        self.do_update(new_backends, enablement, callback);
        Ok(())
    }

    /// Run discovery and return the membership and readiness update bundle when
    /// membership changes, for a group to schedule selector rebuilds.
    async fn update_backends(&self) -> Result<Option<BackendUpdate>> {
        let (new_backends, enablement) = self.discovery.discover().await?;
        Ok(self.do_update(new_backends, enablement, |_| {}))
    }

    /// Run one health-check pass for this view's entire registry.
    pub async fn run_health_check(&self, parallel: bool) {
        self.health_registry.run_health_check(parallel).await;
    }

    /// Return whether this view's load balancer owns its active probe loop.
    fn owns_health_checks(&self) -> bool {
        self.owns_health_checks
    }
}

impl Drop for Backends {
    fn drop(&mut self) {
        self.health_registry.defer_view_removal(self.view_id);
    }
}

/// Background service that runs one health-check loop for a shared
/// [`HealthRegistry`].
///
/// Use this service with views created by
/// [`BackendView::new_with_health_registry`]. Load balancers backed by
/// [`Backends::new`] already schedule their private registry and must not also
/// run a `HealthCheckService` for it.
///
/// In periodic mode (`health_check_frequency` set) the service signals
/// readiness after its first health-check pass, including when no views have
/// published targets yet, and publishing the first target wakes it for an
/// immediate pass. In one-shot mode (`health_check_frequency` is `None`) the
/// service instead waits for the first published target before running its
/// single pass and signaling readiness, so it never checks an empty registry
/// and returns before any target exists.
pub struct HealthCheckService {
    registry: Arc<HealthRegistry>,
    /// How frequently to run health checks.
    ///
    /// If `None`, health checks run once when the service starts.
    pub health_check_frequency: Option<Duration>,
    /// Whether to check all targets concurrently.
    pub parallel_health_check: bool,
}

impl HealthCheckService {
    /// Create a health-check service for `registry`.
    ///
    /// A health check must be configured on `registry` before the service is
    /// started. Starting without one fails closed without signaling readiness.
    pub fn new(registry: Arc<HealthRegistry>) -> Self {
        Self {
            registry,
            health_check_frequency: None,
            parallel_health_check: false,
        }
    }
}

/// Timing information from the most recent [`LoadBalancer::update`] call.
#[derive(Debug, Clone, Copy)]
pub struct UpdateTimings {
    /// Time spent in [`ServiceDiscovery::discover`].
    pub discovery_duration: Duration,
    /// Time spent building the selection algorithm and storing the updated backends.
    ///
    /// This is zero for [`LoadBalancerGroup`] because its selectors rebuild
    /// asynchronously. Use
    /// [`LoadBalancerGroup::selector_last_update_timing`] for per-selector
    /// queue and build durations.
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
    /// This setting is ignored for views created with
    /// [`BackendView::new_with_health_registry`]; use [`HealthCheckService`]
    /// for those views.
    pub health_check_frequency: Option<Duration>,
    /// How frequent the service discovery should run.
    ///
    /// If `None`, the service discovery will only run once at the beginning.
    pub update_frequency: Option<Duration>,
    /// Whether to run health check to all backends in parallel. Default is false.
    pub parallel_health_check: bool,
}

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

/// Timing information for one selector rebuild.
#[derive(Debug, Clone, Copy)]
pub struct SelectorUpdateTimings {
    /// The backend generation used to build the selector.
    pub generation: u64,
    /// Time from the discovery update until selector construction started.
    pub queue_duration: Duration,
    /// Time spent constructing and publishing the selector.
    pub build_duration: Duration,
}

/// A request to rebuild one selector for a backend generation.
struct SelectorRebuildRequest {
    /// Backend generation this request will build.
    generation: u64,
    /// Backend membership captured for this generation.
    backends: Arc<BTreeSet<Backend>>,
    /// Readiness snapshot published with the rebuilt selector, kept together
    /// with membership and generation through coalescing, retries, and
    /// cancellation.
    readiness: ReadinessSnapshot,
    /// Time the rebuild was requested, used to measure queue delay.
    requested_at: Instant,
}

/// Per-selector state for one active rebuild and its newest pending request.
#[derive(Default)]
struct SelectorRebuildState {
    /// Whether a worker currently owns this selector's rebuild loop.
    is_running: bool,
    /// Newest request waiting for that worker; older requests are replaced.
    pending_request: Option<SelectorRebuildRequest>,
}

/// Restores a selector's rebuild state if its worker exits unexpectedly.
struct SelectorRebuildTaskGuard<S>
where
    S: BackendSelection,
{
    /// Selector slot whose worker state this guard owns.
    slot: Arc<SelectorSlot<S>>,
    /// Wakes the group background loop after cleanup.
    rebuild_notify: Arc<Notify>,
    /// Request restored to `pending` if the worker stops during a build.
    in_flight: Option<SelectorRebuildRequest>,
    /// Whether dropping this guard should perform cleanup.
    armed: bool,
}

/// Monotonic cancellation signal shared by all selector tasks in one group.
///
/// [`Self::wait_for_cancel`] registers its waiter before rechecking the flag because
/// [`Notify::notify_waiters`] does not retain a permit for future waiters.
struct SelectorRebuildCancellation {
    /// Remains true after cancellation begins.
    cancelled: AtomicBool,
    /// Wakes every rebuild task waiting for cancellation.
    notify: Notify,
}

impl SelectorRebuildCancellation {
    fn new() -> Self {
        Self {
            cancelled: AtomicBool::new(false),
            notify: Notify::new(),
        }
    }

    fn cancel(&self) {
        self.cancelled.store(true, Release);
        self.notify.notify_waiters();
    }

    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Acquire)
    }

    async fn wait_for_cancel(&self) {
        if self.is_cancelled() {
            return;
        }

        let notified = self.notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if self.is_cancelled() {
            return;
        }
        notified.await;
    }
}

impl<S> SelectorRebuildTaskGuard<S>
where
    S: BackendSelection,
{
    fn new(slot: Arc<SelectorSlot<S>>, rebuild_notify: Arc<Notify>) -> Self {
        Self {
            slot,
            rebuild_notify,
            in_flight: None,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl<S> Drop for SelectorRebuildTaskGuard<S>
where
    S: BackendSelection,
{
    fn drop(&mut self) {
        // Normal completion disarms the guard after clearing the worker state.
        if !self.armed {
            return;
        }

        // Record a worker panic that bypassed the normal rebuild error path.
        if std::thread::panicking() {
            self.slot.failed_rebuilds.fetch_add(1, Relaxed);
            log::error!("load-balancing selector rebuild task panicked");
        }
        // This lock is never held across an await or user code, so task
        // cancellation cannot re-enter it and competing critical sections are short.
        let mut rebuild_state = lock_rebuild_state(&self.slot.rebuild_state);
        if let Some(request) = self.in_flight.take() {
            if rebuild_state
                .pending_request
                .as_ref()
                .is_none_or(|pending| pending.generation <= request.generation)
            {
                rebuild_state.pending_request = Some(request);
            }
        }
        // no worker running the rebuild anymore
        rebuild_state.is_running = false;
        drop(rebuild_state);
        // Prompt startup readiness to recheck generations after unexpected
        // cleanup. This does not restart the stopped rebuild worker.
        self.rebuild_notify.notify_one();
    }
}

/// One independently configured selector in a [`LoadBalancerGroup`].
struct SelectorSlot<S>
where
    S: BackendSelection,
{
    /// Selector snapshot currently used by requests.
    selector: ArcSwap<PublishedSelector<S>>,
    /// Configuration used to build this selector.
    config: Option<S::Config>,
    /// Backend generation served by the published selector.
    generation: AtomicU64,
    /// Active worker and newest pending rebuild request.
    rebuild_state: Mutex<SelectorRebuildState>,
    /// Timing from the most recently published rebuild.
    last_update_timing: ArcSwap<Option<SelectorUpdateTimings>>,
    /// Number of pending requests replaced by newer generations.
    coalesced_rebuilds: AtomicU64,
    /// Number of rebuild attempts that failed.
    failed_rebuilds: AtomicU64,
    /// Interrupts retry backoff when a newer request arrives.
    interrupt_retry: Notify,
}

/// Signals when a retired selector and all of its readers have been released.
struct SelectorReleaseSignal {
    /// Whether the retired selector and all readers have been dropped.
    released: AtomicBool,
    /// Wakes the rebuild gate when release completes.
    notify: Notify,
}

impl SelectorReleaseSignal {
    fn new() -> Self {
        Self {
            released: AtomicBool::new(false),
            notify: Notify::new(),
        }
    }

    async fn wait_for_release(&self) {
        loop {
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.released.load(Acquire) {
                return;
            }
            notified.await;
        }
    }
}

/// Field-drop guard that signals a selector's release.
///
/// Kept as the final field of [`PublishedSelector`] so its [`Drop`] runs
/// *after* the `Arc<S>` selector field has been dropped. A manual
/// `impl Drop for PublishedSelector` would instead run before any field is
/// dropped, signaling release while the selector is still alive and violating
/// the one-additional-generation memory bound.
struct SelectorReleaseGuard {
    /// Signal updated when this final field is dropped.
    release_signal: Arc<SelectorReleaseSignal>,
}

impl Drop for SelectorReleaseGuard {
    fn drop(&mut self) {
        self.release_signal.released.store(true, Release);
        self.release_signal.notify.notify_one();
    }
}

/// A built selector snapshot published to request readers through [`ArcSwap`].
///
/// It pairs the selection data with the readiness snapshot for its generation
/// and a signal used to track when a replaced snapshot is no longer referenced.
/// Keeping readiness here lets a retired selector keep serving its own backends
/// with their own readiness, independently of the current view state.
struct PublishedSelector<S> {
    /// Declared before `release_guard` so the shared selector is dropped (and,
    /// when this holds the last reference, destroyed) before release is
    /// signaled.
    selector: Arc<S>,
    /// Readiness for the generation this selector was built from.
    readiness: ReadinessSnapshot,
    /// Signals after `selector` and all published references are dropped.
    release_guard: SelectorReleaseGuard,
}

impl<S> PublishedSelector<S> {
    fn new(selector: S, readiness: ReadinessSnapshot) -> Self {
        Self {
            selector: Arc::new(selector),
            readiness,
            release_guard: SelectorReleaseGuard {
                release_signal: Arc::new(SelectorReleaseSignal::new()),
            },
        }
    }

    fn release_signal(&self) -> Arc<SelectorReleaseSignal> {
        Arc::clone(&self.release_guard.release_signal)
    }
}

/// A serial rebuild gate shared by one or more [`LoadBalancerGroup`]s.
///
/// The gate permits one selector build at a time. After a replacement is
/// published, it also waits for all readers of the retired selector to release
/// it before permitting another build. Groups that share one gate therefore
/// retain at most one additional selector generation beyond their currently
/// published selectors: either one replacement under construction or one
/// retired selector still held by readers, never both.
///
/// Retired selectors notify the gate when their final reader releases them, so
/// one gate can coordinate groups with different selector implementations
/// without polling.
pub struct SelectorRebuildGate {
    /// Allows only one selector build through this gate at a time.
    semaphore: Arc<Semaphore>,
    /// Release signal that must complete before the next build starts.
    retired: Mutex<Option<Arc<SelectorReleaseSignal>>>,
}

impl Default for SelectorRebuildGate {
    fn default() -> Self {
        Self::new()
    }
}

impl SelectorRebuildGate {
    /// Create a serial selector rebuild gate.
    pub fn new() -> Self {
        Self {
            semaphore: Arc::new(Semaphore::new(1)),
            retired: Mutex::new(None),
        }
    }

    async fn acquire(self: &Arc<Self>) -> OwnedSemaphorePermit {
        let permit = Arc::clone(&self.semaphore)
            .acquire_owned()
            .await
            .expect("selector rebuild gate semaphore is never closed");
        let retired = self
            .retired
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        if let Some(retired) = retired {
            retired.wait_for_release().await;
            let mut registered = self
                .retired
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            // Clear only the signal we waited for, not a newer retired selector.
            if registered
                .as_ref()
                .is_some_and(|current| Arc::ptr_eq(current, &retired))
            {
                registered.take();
            }
        }
        permit
    }

    /// Register `selector` as the generation that must be released before the
    /// next rebuild can start.
    ///
    /// The caller must hold the permit returned by [`Self::acquire`] and call
    /// this before dropping it. Otherwise another rebuild could acquire the
    /// gate without observing the retired selector's release signal.
    fn retire<S>(&self, selector: &Arc<PublishedSelector<S>>) {
        let release_signal = selector.release_signal();
        *self
            .retired
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(release_signal);
    }
}

/// A collection of eventually consistent load-balancing selectors that share
/// one backend pool.
///
/// Discovery membership and readiness are published independently from
/// selector construction. Each selector continues serving its previous
/// generation while its replacement is built on a blocking worker. Pending
/// updates are coalesced so that at most one newer generation waits behind an
/// in-progress build or a shared rebuild gate. Selector builds and retired
/// generations are bounded by a [`SelectorRebuildGate`].
///
/// Each published selector owns the readiness snapshot for its generation, so a
/// removed backend stays selectable only through selectors still serving an
/// older generation. That readiness is released when the selector is replaced
/// and its last reader drops it; no separate pruning step is needed.
pub struct LoadBalancerGroup<S>
where
    S: BackendSelection,
{
    /// Backend membership and readiness shared by every selector.
    backends: Backends,
    /// Independently configured selectors and their rebuild state.
    selectors: Box<[Arc<SelectorSlot<S>>]>,
    /// Limits selector builds and waits for retired selectors to be released.
    rebuild_gate: Arc<SelectorRebuildGate>,
    /// Wakes startup readiness checks when a selector rebuild changes state.
    rebuild_notify: Arc<Notify>,
    /// Stops selector rebuild tasks when the group is dropped.
    rebuild_cancellation: Arc<SelectorRebuildCancellation>,

    /// Timing information from the most recent [`update`](Self::update) call.
    ///
    /// `None` until the first successful update completes.
    last_update_timing: ArcSwap<Option<UpdateTimings>>,

    /// How frequently the health check logic (if set) should run.
    ///
    /// If `None`, the health check logic will only run once at the beginning.
    /// This setting is ignored for views created with
    /// [`BackendView::new_with_health_registry`]; use [`HealthCheckService`]
    /// for those views.
    pub health_check_frequency: Option<Duration>,
    /// How frequently service discovery should run.
    ///
    /// If `None`, service discovery will only run once at the beginning.
    pub update_frequency: Option<Duration>,
    /// Whether to run health checks for all backends in parallel. Default is false.
    pub parallel_health_check: bool,
}

fn lock_rebuild_state(state: &Mutex<SelectorRebuildState>) -> MutexGuard<'_, SelectorRebuildState> {
    state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Initial delay before retrying a failed selector rebuild.
///
/// Failed builds (e.g. a panicking selector constructor) are retried so a
/// transient failure cannot strand a selector at a stale generation when
/// discovery membership does not change again. The delay backs off
/// exponentially up to [`MAX_SELECTOR_REBUILD_BACKOFF`] to bound retries and
/// avoid a hot loop when a build fails persistently.
const INITIAL_SELECTOR_REBUILD_BACKOFF: Duration = Duration::from_millis(10);
/// Upper bound for the retry delay after repeated selector rebuild failures.
const MAX_SELECTOR_REBUILD_BACKOFF: Duration = Duration::from_secs(1);

async fn run_selector_rebuilds<S>(
    slot: Arc<SelectorSlot<S>>,
    rebuild_gate: Arc<SelectorRebuildGate>,
    rebuild_notify: Arc<Notify>,
    rebuild_cancellation: Arc<SelectorRebuildCancellation>,
) where
    S: BackendSelection + Send + Sync + 'static,
    S::Config: 'static,
{
    // This future can be dropped at any await. The guard restores an in-flight
    // request and clears `running` so a later rebuild can start a new worker.
    let mut task_guard =
        SelectorRebuildTaskGuard::new(Arc::clone(&slot), Arc::clone(&rebuild_notify));
    let mut backoff = INITIAL_SELECTOR_REBUILD_BACKOFF;
    loop {
        if rebuild_cancellation.is_cancelled() {
            // rebuild cancelled already, exit
            return;
        }
        {
            let mut rebuild_state = lock_rebuild_state(&slot.rebuild_state);
            if rebuild_state.pending_request.is_none() {
                // nothing to do, exit
                rebuild_state.is_running = false;
                task_guard.disarm();
                return;
            }
        }

        let permit = tokio::select! {
            // wait for exclusive rebuild access and for readers to release the retired selector
            permit = rebuild_gate.acquire() => permit,
            _ = rebuild_cancellation.wait_for_cancel() => return, // cancelled
        };

        let Some(request) = lock_rebuild_state(&slot.rebuild_state)
            .pending_request
            .take()
        else {
            // another worker took the job already
            drop(permit);
            continue;
        };
        // consume the wakeup associated with the request now being processed
        // so we do not incorrectly interrupt a later retry
        let _ = slot.interrupt_retry.notified().now_or_never();
        let build_start = Instant::now();
        let queue_duration = build_start.saturating_duration_since(request.requested_at);
        let generation = request.generation;
        let requested_at = request.requested_at;
        let backends = Arc::clone(&request.backends);
        let readiness = request.readiness.clone();
        task_guard.in_flight = Some(request);
        let build_backends = Arc::clone(&backends);
        let build_slot = Arc::clone(&slot);
        let result = tokio::task::spawn_blocking(move || {
            build_selector::<S>(&build_backends, build_slot.config.as_ref())
        })
        .await;
        let build_duration = build_start.elapsed();

        if rebuild_cancellation.is_cancelled() {
            // cancelled during construction
            if let Ok(selector) = result {
                let _drop_task = tokio::task::spawn_blocking(move || {
                    drop(selector);
                    drop(permit);
                });
            } else {
                drop(permit);
            }
            return;
        }

        match result {
            Ok(selector) => {
                let retired = slot
                    .selector
                    .swap(Arc::new(PublishedSelector::new(selector, readiness)));
                // Track the replaced selector until all readers release it.
                rebuild_gate.retire(&retired);
                let _drop_task = tokio::task::spawn_blocking(move || drop(retired));
                slot.last_update_timing
                    .store(Arc::new(Some(SelectorUpdateTimings {
                        generation,
                        queue_duration,
                        build_duration,
                    })));
                slot.generation.store(generation, Release);
                task_guard.in_flight = None;
                backoff = INITIAL_SELECTOR_REBUILD_BACKOFF;
                drop(permit);
                // notify load balancer group
                rebuild_notify.notify_one();
            }
            Err(error) => {
                slot.failed_rebuilds.fetch_add(1, Relaxed);
                if error.is_panic() {
                    log::error!(
                        "load-balancing selector rebuild panicked for generation {generation}: {error}"
                    );
                } else if error.is_cancelled() {
                    log::error!(
                        "load-balancing selector rebuild was cancelled for generation {generation}: {error}"
                    );
                } else {
                    log::error!(
                        "load-balancing selector rebuild failed for generation {generation}: {error}"
                    );
                }
                // Reschedule this generation unless a newer request already
                // superseded it, then release the gate and retry after a
                // bounded backoff so the selector can still converge even when
                // discovery membership does not change again.
                let retry_failed_generation = {
                    let mut state = lock_rebuild_state(&slot.rebuild_state);
                    if state
                        .pending_request
                        .as_ref()
                        .is_some_and(|pending| pending.generation > generation)
                    {
                        false
                    } else {
                        state.pending_request = Some(SelectorRebuildRequest {
                            generation,
                            backends,
                            readiness,
                            requested_at,
                        });
                        true
                    }
                };
                task_guard.in_flight = None;
                drop(permit);
                if retry_failed_generation {
                    tokio::select! {
                        _ = tokio::time::sleep(backoff) => {
                            // The failed generation still needs retrying; increase
                            // the delay if its next attempt also fails.
                            backoff = (backoff * 2).min(MAX_SELECTOR_REBUILD_BACKOFF);
                        }
                        _ = slot.interrupt_retry.notified() => {
                            // A newer generation is pending, so process it now
                            // instead of waiting on the older failure's backoff.
                            backoff = INITIAL_SELECTOR_REBUILD_BACKOFF;
                        }
                        // The group was dropped while this worker was waiting.
                        _ = rebuild_cancellation.wait_for_cancel() => return,
                    }
                } else {
                    // Consume the notification associated with the pending newer
                    // generation so it cannot skip a later retry delay.
                    let _ = slot.interrupt_retry.notified().now_or_never();
                    backoff = INITIAL_SELECTOR_REBUILD_BACKOFF;
                }
            }
        }
    }
}

impl<S> LoadBalancerGroup<S>
where
    S: BackendSelection + Send + Sync + 'static,
    S::Config: 'static,
    S::Iter: BackendIter,
{
    /// Build a group of selectors over one shared backend pool.
    ///
    /// Each item in `configs` creates one selector. `None` uses
    /// [`BackendSelection::build`], while `Some(config)` uses
    /// [`BackendSelection::build_with_config`].
    ///
    /// # Panics
    ///
    /// Panics if `backends` has already been updated. A group owns backend
    /// updates and must start with selector generation zero.
    pub fn from_backends_with_configs(
        backends: Backends,
        configs: impl IntoIterator<Item = Option<S::Config>>,
    ) -> Self {
        assert_eq!(
            backends.generation(),
            0,
            "backends must not be updated before constructing a load balancer group"
        );
        let current_backends = backends.get_backend();
        let current_readiness = backends.readiness_snapshot();
        let selectors = configs
            .into_iter()
            .map(|config| {
                Arc::new(SelectorSlot {
                    selector: ArcSwap::new(Arc::new(PublishedSelector::new(
                        build_selector::<S>(&current_backends, config.as_ref()),
                        current_readiness.clone(),
                    ))),
                    config,
                    generation: AtomicU64::new(0),
                    rebuild_state: Mutex::new(SelectorRebuildState::default()),
                    last_update_timing: ArcSwap::new(Arc::new(None)),
                    coalesced_rebuilds: AtomicU64::new(0),
                    failed_rebuilds: AtomicU64::new(0),
                    interrupt_retry: Notify::new(),
                })
            })
            .collect();

        Self {
            backends,
            selectors,
            rebuild_gate: Arc::new(SelectorRebuildGate::new()),
            rebuild_notify: Arc::new(Notify::new()),
            rebuild_cancellation: Arc::new(SelectorRebuildCancellation::new()),
            last_update_timing: ArcSwap::new(Arc::new(None)),
            health_check_frequency: None,
            update_frequency: None,
            parallel_health_check: false,
        }
    }

    /// Use a rebuild gate shared with other selector groups.
    ///
    /// A group uses a private serial gate by default. Supplying the same gate
    /// to multiple groups extends the one-extra-generation memory bound across
    /// all of them.
    pub fn with_rebuild_gate(mut self, rebuild_gate: Arc<SelectorRebuildGate>) -> Self {
        self.rebuild_gate = rebuild_gate;
        self
    }

    /// Return the number of selectors in this group.
    pub fn selector_count(&self) -> usize {
        self.selectors.len()
    }

    fn schedule_selector_rebuild(
        &self,
        slot: &Arc<SelectorSlot<S>>,
        update: &BackendUpdate,
        requested_at: Instant,
    ) {
        let should_spawn = {
            let mut rebuild_state = lock_rebuild_state(&slot.rebuild_state);
            // update selector rebuild request if another one happens while we were already pending
            if rebuild_state
                .pending_request
                .replace(SelectorRebuildRequest {
                    generation: update.generation,
                    backends: Arc::clone(&update.backends),
                    readiness: update.readiness.clone(),
                    requested_at,
                })
                .is_some()
            {
                slot.coalesced_rebuilds.fetch_add(1, Relaxed);
            }

            if rebuild_state.is_running {
                // already spawned and running
                false
            } else {
                // spawn it
                rebuild_state.is_running = true;
                true
            }
        };

        if should_spawn {
            let task = run_selector_rebuilds(
                Arc::clone(slot),
                Arc::clone(&self.rebuild_gate),
                Arc::clone(&self.rebuild_notify),
                Arc::clone(&self.rebuild_cancellation),
            );
            let _rebuild_task = tokio::spawn(task);
        } else {
            slot.interrupt_retry.notify_one();
        }
    }

    /// Run service discovery and enqueue selector rebuilds when membership changes.
    ///
    /// The discovered backend membership is published before this method returns.
    /// Selector rebuilds run asynchronously. A removed backend stays selectable
    /// through selectors still serving an older generation, via the readiness
    /// snapshot each of those selectors owns.
    ///
    /// To wait for convergence, read [`backend_generation`](Self::backend_generation)
    /// after this call and poll [`selectors_ready_for`](Self::selectors_ready_for).
    /// Calls on the same group must not overlap. [`Self::run`] serializes them.
    pub async fn update(&self) -> Result<()> {
        let start = Instant::now();
        let changed = self.backends.update_backends().await?;
        let discovery_duration = start.elapsed();

        if let Some(update) = changed {
            for slot in &self.selectors {
                self.schedule_selector_rebuild(slot, &update, Instant::now());
            }
        }

        self.last_update_timing.store(Arc::new(Some(UpdateTimings {
            discovery_duration,
            // Selector builds continue asynchronously and report their own timing.
            build_duration: Duration::ZERO,
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
    /// Each selector consults the readiness snapshot published with it, so a
    /// selector serving an older generation keeps using that generation's
    /// readiness. Returns `None` when `selector_index` is out of bounds.
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
        // `published` is an `ArcSwap` guard held for the whole selection so the
        // selector data and its matching readiness snapshot share one snapshot
        // without a per-request `Arc` clone.
        //
        // Declaration order matters for the release invariant: `published` is
        // declared before `iter`, so `iter` (and the per-iteration `Arc<S>`
        // clone it holds) is dropped before the guard. The guard's drop can
        // destroy the retired selector and fire its release signal, so release
        // must not be signaled while an iterator still references the selector.
        let published = self.selectors.get(selector_index)?.selector.load();
        let mut iter = UniqueIterator::new(published.selector.iter(key), max_iterations);
        while let Some(backend) = iter.get_next() {
            if accept(&backend, published.readiness.ready(&backend)) {
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

    /// Return the latest backend membership generation.
    pub fn backend_generation(&self) -> u64 {
        self.backends.generation()
    }

    /// Return the generation currently served by one selector.
    pub fn selector_generation(&self, selector_index: usize) -> Option<u64> {
        self.selectors
            .get(selector_index)
            .map(|slot| slot.generation.load(Acquire))
    }

    /// Return whether every selector serves at least `generation`.
    pub fn selectors_ready_for(&self, generation: u64) -> bool {
        self.selectors
            .iter()
            .all(|slot| slot.generation.load(Acquire) >= generation)
    }

    /// Return timing information for the most recently published selector generation.
    pub fn selector_last_update_timing(
        &self,
        selector_index: usize,
    ) -> Option<SelectorUpdateTimings> {
        self.selectors
            .get(selector_index)
            .and_then(|slot| **slot.last_update_timing.load())
    }

    /// Return how many pending selector generations were replaced by a newer one.
    pub fn selector_coalesced_rebuilds(&self, selector_index: usize) -> Option<u64> {
        self.selectors
            .get(selector_index)
            .map(|slot| slot.coalesced_rebuilds.load(Relaxed))
    }

    /// Return how many selector rebuild tasks failed.
    pub fn selector_failed_rebuilds(&self, selector_index: usize) -> Option<u64> {
        self.selectors
            .get(selector_index)
            .map(|slot| slot.failed_rebuilds.load(Relaxed))
    }

    /// Wait for a selector rebuild to complete or terminate unexpectedly.
    pub(crate) fn rebuild_notified(&self) -> impl Future<Output = ()> + '_ {
        self.rebuild_notify.notified()
    }

    /// Return timing information from the most recent successful [`update`](Self::update).
    pub fn last_update_timing(&self) -> Option<UpdateTimings> {
        **self.last_update_timing.load()
    }
}

impl<S> Drop for LoadBalancerGroup<S>
where
    S: BackendSelection,
{
    fn drop(&mut self) {
        self.rebuild_cancellation.cancel();
    }
}

#[cfg(test)]
mod test {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed};
    use std::sync::Condvar;

    use super::*;
    use async_trait::async_trait;
    use pingora_core::services::ServiceReadyNotifier;

    struct BuildTracker {
        blocked: Mutex<bool>,
        unblocked: Condvar,
        /// Builds whose zero-based index is `>= block_from` block until
        /// released, independently of the `blocked` flag. This allows releasing
        /// an earlier build while deterministically holding a later one.
        block_from: AtomicUsize,
        /// Number of subsequent builds that should panic before succeeding.
        panics: AtomicUsize,
        /// Whether selector destructors should block until released.
        drop_blocked: Mutex<bool>,
        drop_unblocked: Condvar,
        drop_waiters: AtomicUsize,
        active: AtomicUsize,
        max_active: AtomicUsize,
        builds: AtomicUsize,
        live_selectors: AtomicUsize,
        max_live_selectors: AtomicUsize,
    }

    impl Default for BuildTracker {
        fn default() -> Self {
            Self {
                blocked: Mutex::new(false),
                unblocked: Condvar::new(),
                block_from: AtomicUsize::new(usize::MAX),
                panics: AtomicUsize::new(0),
                drop_blocked: Mutex::new(false),
                drop_unblocked: Condvar::new(),
                drop_waiters: AtomicUsize::new(0),
                active: AtomicUsize::new(0),
                max_active: AtomicUsize::new(0),
                builds: AtomicUsize::new(0),
                live_selectors: AtomicUsize::new(0),
                max_live_selectors: AtomicUsize::new(0),
            }
        }
    }

    impl BuildTracker {
        fn run_build(&self) {
            if self.panics.load(Relaxed) > 0 {
                self.panics.fetch_sub(1, Relaxed);
                panic!("intentional selector build panic");
            }
            let index = self.builds.fetch_add(1, Relaxed);
            let active = self.active.fetch_add(1, Relaxed) + 1;
            self.max_active.fetch_max(active, Relaxed);

            let mut blocked = self
                .blocked
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            while *blocked || index >= self.block_from.load(Relaxed) {
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

        fn set_block_from(&self, index: usize) {
            self.block_from.store(index, Relaxed);
            self.unblocked.notify_all();
        }

        fn set_panics(&self, count: usize) {
            self.panics.store(count, Relaxed);
        }

        fn block_drop(&self) {
            *self
                .drop_blocked
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = true;
        }

        fn unblock_drop(&self) {
            *self
                .drop_blocked
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = false;
            self.drop_unblocked.notify_all();
        }

        fn wait_drop(&self) {
            self.drop_waiters.fetch_add(1, Relaxed);
            let mut blocked = self
                .drop_blocked
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            while *blocked {
                blocked = self
                    .drop_unblocked
                    .wait(blocked)
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
            }
            self.drop_waiters.fetch_sub(1, Relaxed);
        }

        fn reset(&self) {
            self.active.store(0, Relaxed);
            self.max_active.store(0, Relaxed);
            self.builds.store(0, Relaxed);
            self.max_live_selectors
                .store(self.live_selectors.load(Relaxed), Relaxed);
        }

        fn selector_created(&self) {
            let live = self.live_selectors.fetch_add(1, Relaxed) + 1;
            self.max_live_selectors.fetch_max(live, Relaxed);
        }

        fn selector_dropped(&self) {
            self.live_selectors.fetch_sub(1, Relaxed);
        }
    }

    #[derive(Clone)]
    struct TestSelectionConfig {
        reverse: bool,
        tracker: Option<Arc<BuildTracker>>,
    }

    struct TestSelection {
        backends: Vec<Backend>,
        tracker: Option<Arc<BuildTracker>>,
    }

    impl Drop for TestSelection {
        fn drop(&mut self) {
            if let Some(tracker) = &self.tracker {
                tracker.wait_drop();
                tracker.selector_dropped();
            }
        }
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
            if let Some(tracker) = &config.tracker {
                tracker.selector_created();
            }
            Self {
                backends,
                tracker: config.tracker.clone(),
            }
        }

        fn build(backends: &BTreeSet<Backend>) -> Self {
            Self {
                backends: backends.iter().cloned().collect(),
                tracker: None,
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
        enablement: ArcSwap<HashMap<u64, bool>>,
    }

    impl MutableDiscovery {
        fn new(backends: BTreeSet<Backend>) -> Self {
            Self {
                backends: ArcSwap::new(Arc::new(backends)),
                enablement: ArcSwap::new(Arc::new(HashMap::new())),
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

        fn set_enabled(&self, backend: &Backend, enabled: bool) {
            let mut enablement = HashMap::clone(&self.enablement.load());
            enablement.insert(backend.hash_key(), enabled);
            self.enablement.store(Arc::new(enablement));
        }
    }

    #[async_trait]
    impl ServiceDiscovery for Arc<MutableDiscovery> {
        async fn discover(&self) -> Result<(BTreeSet<Backend>, HashMap<u64, bool>)> {
            Ok((
                BTreeSet::clone(&self.backends.load()),
                HashMap::clone(&self.enablement.load()),
            ))
        }
    }

    struct FailThenWaitDiscovery {
        backend: Backend,
        attempts: AtomicUsize,
        allow_success: Notify,
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

    fn address_health_key(backend: &Backend) -> u64 {
        let mut hasher = DefaultHasher::new();
        backend.addr.hash(&mut hasher);
        hasher.finish()
    }

    struct SelectiveHealthCheck {
        checks: Arc<AtomicUsize>,
        unhealthy: Option<SocketAddr>,
    }

    #[async_trait]
    impl health_check::HealthCheck for SelectiveHealthCheck {
        async fn check(&self, target: &Backend) -> Result<()> {
            self.checks.fetch_add(1, Relaxed);
            if self.unhealthy.as_ref() == Some(&target.addr) {
                Err(pingora_error::Error::new(ErrorType::InternalError))
            } else {
                Ok(())
            }
        }

        fn health_threshold(&self, _success: bool) -> usize {
            1
        }
    }

    /// Health check whose result depends on the backend's full identity (its
    /// weight), so two backends sharing an address can produce different
    /// results.
    struct WeightHealthCheck {
        checks: Arc<AtomicUsize>,
        unhealthy_weight: usize,
    }

    #[async_trait]
    impl health_check::HealthCheck for WeightHealthCheck {
        async fn check(&self, target: &Backend) -> Result<()> {
            self.checks.fetch_add(1, Relaxed);
            if target.weight == self.unhealthy_weight {
                Err(pingora_error::Error::new(ErrorType::InternalError))
            } else {
                Ok(())
            }
        }

        fn health_threshold(&self, _success: bool) -> usize {
            1
        }
    }

    async fn wait_for_group_generation(group: &LoadBalancerGroup<TestSelection>, generation: u64) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while !group.selectors_ready_for(generation) {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("selector rebuild timed out");
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

    async fn wait_for_builds_started(tracker: &BuildTracker, expected: usize) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while tracker.builds.load(Relaxed) != expected {
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
    #[should_panic(
        expected = "backends must not be updated before constructing a load balancer group"
    )]
    async fn test_load_balancer_group_rejects_updated_backends() {
        let backend = Backend::new("1.0.0.1:80").unwrap();
        let backends = Backends::new(Box::new(Arc::new(MutableDiscovery::new(BTreeSet::from([
            backend,
        ])))));
        backends.update(|_| {}).await.unwrap();

        let _ = LoadBalancerGroup::<TestSelection>::from_backends_with_configs(backends, [None]);
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
        wait_for_group_generation(&group, 1).await;
        assert_eq!(group.select(0, b"", 10), Some(b1.clone()));
        assert_eq!(group.select(1, b"", 10), Some(b2));

        discovery.add(b3.clone());
        group.update().await.unwrap();
        wait_for_group_generation(&group, 2).await;
        assert_eq!(group.select(0, b"", 10), Some(b1));
        assert_eq!(group.select(1, b"", 10), Some(b3));
        assert!(group.last_update_timing().is_some());
        assert_eq!(group.backend_generation(), 2);
        assert_eq!(group.selector_generation(0), Some(2));
        assert_eq!(group.select(2, b"", 10), None);
    }

    #[tokio::test]
    async fn test_unchanged_backends_do_not_rebuild_view_or_selector() {
        let backend = Backend::new("1.0.0.1:80").unwrap();
        let discovery = Arc::new(MutableDiscovery::new(BTreeSet::from([backend])));
        let tracker = Arc::new(BuildTracker::default());
        let group = LoadBalancerGroup::<TestSelection>::from_backends_with_configs(
            Backends::new(Box::new(discovery)),
            [Some(TestSelectionConfig {
                reverse: false,
                tracker: Some(Arc::clone(&tracker)),
            })],
        );
        tracker.reset();

        group.update().await.unwrap();
        wait_for_group_generation(&group, 1).await;
        let view_state = group.backends.state.load_full();
        let registry_state = group.backends.health_registry.state.load_full();
        let selector = group.selectors[0].selector.load_full();
        let builds = tracker.builds.load(Relaxed);

        group.update().await.unwrap();

        assert_eq!(group.backend_generation(), 1);
        assert_eq!(group.selector_generation(0), Some(1));
        assert_eq!(tracker.builds.load(Relaxed), builds);
        assert!(Arc::ptr_eq(&view_state, &group.backends.state.load_full()));
        assert!(Arc::ptr_eq(
            &registry_state,
            &group.backends.health_registry.state.load_full()
        ));
        assert!(Arc::ptr_eq(
            &selector,
            &group.selectors[0].selector.load_full()
        ));
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
        wait_for_group_generation(&group, 1).await;
        group.backends().run_health_check(false).await;

        assert_eq!(checks.load(Relaxed), 2);
        group.backends().set_enable(&b1, false);
        assert_eq!(group.select(0, b"", 10), Some(b2.clone()));
        assert_eq!(group.select(1, b"", 10), Some(b2));
    }

    #[test]
    #[should_panic(expected = "health check already configured")]
    fn test_health_registry_rejects_replacing_health_check() {
        let registry = HealthRegistry::new();
        let checks = Arc::new(AtomicUsize::new(0));
        registry.set_health_check(Box::new(CountingHealthCheck {
            checks: Arc::clone(&checks),
        }));

        registry.set_health_check(Box::new(CountingHealthCheck { checks }));
    }

    #[tokio::test]
    async fn test_view_drop_defers_removal_without_locking_views() {
        let backend = Backend::new("1.0.0.1:80").unwrap();
        let registry = Arc::new(HealthRegistry::new());
        let view = Backends::new_with_health_registry(
            Box::new(Arc::new(MutableDiscovery::new(BTreeSet::from([backend])))),
            Arc::clone(&registry),
        );
        view.update(|_| {}).await.unwrap();
        let view_id = view.view_id;

        let views = registry
            .views
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        drop(view);
        assert!(views.contains_key(&view_id));
        drop(views);

        assert_eq!(registry.target_count(), 0);
        assert!(!registry
            .views
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .contains_key(&view_id));
    }

    #[tokio::test]
    async fn test_health_registry_shares_probes_across_backend_views() {
        let shared = Backend::new("1.0.0.1:80").unwrap();
        let first_only = Backend::new("1.0.0.2:80").unwrap();
        let second_only = Backend::new("1.0.0.3:80").unwrap();
        let first_discovery = Arc::new(MutableDiscovery::new(BTreeSet::from([
            shared.clone(),
            first_only.clone(),
        ])));
        let second_discovery = Arc::new(MutableDiscovery::new(BTreeSet::from([
            shared.clone(),
            second_only.clone(),
        ])));
        let checks = Arc::new(AtomicUsize::new(0));
        let registry = Arc::new(HealthRegistry::new());
        registry.set_health_check(Box::new(SelectiveHealthCheck {
            checks: Arc::clone(&checks),
            unhealthy: Some(shared.addr.clone()),
        }));

        let first = Backends::new_with_health_registry(
            Box::new(Arc::clone(&first_discovery)),
            Arc::clone(&registry),
        );
        let second = Backends::new_with_health_registry(
            Box::new(Arc::clone(&second_discovery)),
            Arc::clone(&registry),
        );
        first.update(|_| {}).await.unwrap();
        second.update(|_| {}).await.unwrap();

        assert_eq!(registry.target_count(), 3);
        registry.run_health_check(false).await;
        assert_eq!(checks.load(Relaxed), 3);
        assert!(!first.ready(&shared));
        assert!(!second.ready(&shared));
        assert!(first.ready(&first_only));
        assert!(second.ready(&second_only));
        assert!(!first.ready(&second_only));
        assert!(!second.ready(&first_only));

        first_discovery.replace(BTreeSet::new());
        first.update(|_| {}).await.unwrap();
        assert_eq!(registry.target_count(), 2);
        registry.run_health_check(false).await;
        assert_eq!(checks.load(Relaxed), 5);

        second_discovery.replace(BTreeSet::new());
        second.update(|_| {}).await.unwrap();
        assert_eq!(registry.target_count(), 0);
        registry.run_health_check(false).await;
        assert_eq!(checks.load(Relaxed), 5);
    }

    #[tokio::test]
    async fn test_health_registry_deduplicates_metadata_variants_by_address() {
        let first_backend = Backend::new_with_weight("1.0.0.1:80", 1).unwrap();
        let second_backend = Backend::new_with_weight("1.0.0.1:80", 2).unwrap();
        let checks = Arc::new(AtomicUsize::new(0));
        let registry = Arc::new(HealthRegistry::with_equivalence(address_health_key));
        registry.set_health_check(Box::new(CountingHealthCheck {
            checks: Arc::clone(&checks),
        }));

        let first = Backends::new_with_health_registry(
            Box::new(Arc::new(MutableDiscovery::new(BTreeSet::from([
                first_backend.clone(),
            ])))),
            Arc::clone(&registry),
        );
        let second = Backends::new_with_health_registry(
            Box::new(Arc::new(MutableDiscovery::new(BTreeSet::from([
                second_backend.clone(),
            ])))),
            Arc::clone(&registry),
        );
        first.update(|_| {}).await.unwrap();
        second.update(|_| {}).await.unwrap();

        assert_eq!(registry.target_count(), 1);
        registry.run_health_check(false).await;
        assert_eq!(checks.load(Relaxed), 1);
        assert!(first.ready(&first_backend));
        assert!(second.ready(&second_backend));

        first.set_enable(&first_backend, false);
        assert!(!first.ready(&first_backend));
        assert!(second.ready(&second_backend));
    }

    #[tokio::test]
    async fn test_different_health_registries_probe_independently() {
        let backend = Backend::new("1.0.0.1:80").unwrap();
        let discovery = Arc::new(MutableDiscovery::new(BTreeSet::from([backend])));
        let checks = Arc::new(AtomicUsize::new(0));
        let first_registry = Arc::new(HealthRegistry::new());
        let second_registry = Arc::new(HealthRegistry::new());
        first_registry.set_health_check(Box::new(CountingHealthCheck {
            checks: Arc::clone(&checks),
        }));
        second_registry.set_health_check(Box::new(CountingHealthCheck {
            checks: Arc::clone(&checks),
        }));

        let first = Backends::new_with_health_registry(
            Box::new(Arc::clone(&discovery)),
            Arc::clone(&first_registry),
        );
        let second =
            Backends::new_with_health_registry(Box::new(discovery), Arc::clone(&second_registry));
        first.update(|_| {}).await.unwrap();
        second.update(|_| {}).await.unwrap();

        first_registry.run_health_check(false).await;
        second_registry.run_health_check(false).await;
        assert_eq!(checks.load(Relaxed), 2);
    }

    #[tokio::test]
    async fn test_backend_view_enablement_is_not_shared() {
        let backend = Backend::new("1.0.0.1:80").unwrap();
        let first_discovery = Arc::new(MutableDiscovery::new(BTreeSet::from([backend.clone()])));
        let second_discovery = Arc::new(MutableDiscovery::new(BTreeSet::from([backend.clone()])));
        let registry = Arc::new(HealthRegistry::new());
        let first = Backends::new_with_health_registry(
            Box::new(Arc::clone(&first_discovery)),
            Arc::clone(&registry),
        );
        let second =
            Backends::new_with_health_registry(Box::new(Arc::clone(&second_discovery)), registry);
        first.update(|_| {}).await.unwrap();
        second.update(|_| {}).await.unwrap();
        assert!(first.ready(&backend));
        assert!(second.ready(&backend));

        first_discovery.set_enabled(&backend, false);
        first.update(|_| {}).await.unwrap();
        assert!(!first.ready(&backend));
        assert!(second.ready(&backend));
    }

    #[tokio::test]
    async fn test_manual_enablement_survives_discovery_updates_without_override() {
        let first_backend = Backend::new("1.0.0.1:80").unwrap();
        let second_backend = Backend::new("1.0.0.2:80").unwrap();
        let discovery = Arc::new(MutableDiscovery::new(BTreeSet::from([
            first_backend.clone()
        ])));
        let backends = Backends::new(Box::new(Arc::clone(&discovery)));

        backends.update(|_| {}).await.unwrap();
        backends.set_enable(&first_backend, false);

        backends.update(|_| {}).await.unwrap();
        assert!(!backends.ready(&first_backend));

        discovery.add(second_backend.clone());
        backends.update(|_| {}).await.unwrap();
        assert!(!backends.ready(&first_backend));
        assert!(backends.ready(&second_backend));

        discovery.set_enabled(&first_backend, true);
        backends.update(|_| {}).await.unwrap();
        assert!(backends.ready(&first_backend));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_discovery_override_updates_retained_snapshot_on_readd() {
        let old = Backend::new("1.0.0.1:80").unwrap();
        let replacement = Backend::new("1.0.0.2:80").unwrap();
        let discovery = Arc::new(MutableDiscovery::new(BTreeSet::from([old.clone()])));
        let tracker = Arc::new(BuildTracker::default());
        let group = LoadBalancerGroup::<TestSelection>::from_backends_with_configs(
            Backends::new(Box::new(Arc::clone(&discovery))),
            [Some(TestSelectionConfig {
                reverse: false,
                tracker: Some(Arc::clone(&tracker)),
            })],
        );

        group.update().await.unwrap();
        wait_for_group_generation(&group, 1).await;
        group.backends().set_enable(&old, false);
        assert_eq!(group.select(0, b"", 10), None);

        // Keep the old selector published while its backend leaves and then
        // returns in newer backend generations.
        tracker.reset();
        tracker.block();
        discovery.replace(BTreeSet::from([replacement]));
        group.update().await.unwrap();
        wait_for_active_builds(&tracker, 1).await;
        assert_eq!(group.selector_generation(0), Some(1));
        assert_eq!(group.select(0, b"", 10), None);

        // An explicit discovery override is authoritative. Re-adding the exact
        // identity updates both current readiness and the retained selector's
        // shared enablement flag.
        discovery.set_enabled(&old, true);
        discovery.replace(BTreeSet::from([old.clone()]));
        group.update().await.unwrap();
        assert!(group.backends().ready(&old));
        assert_eq!(group.select(0, b"", 10), Some(old.clone()));

        tracker.unblock();
        wait_for_group_generation(&group, 3).await;
        assert_eq!(group.select(0, b"", 10), Some(old));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_manual_enablement_survives_transition_publication() {
        let first_backend = Backend::new("1.0.0.1:80").unwrap();
        let second_backend = Backend::new("1.0.0.2:80").unwrap();
        let discovery = Arc::new(MutableDiscovery::new(BTreeSet::from([
            first_backend.clone()
        ])));
        let backends = Arc::new(Backends::new(Box::new(Arc::clone(&discovery))));
        backends.update(|_| {}).await.unwrap();

        let tracker = Arc::new(BuildTracker::default());
        tracker.block();
        discovery.add(second_backend);
        let update_task = tokio::spawn({
            let backends = Arc::clone(&backends);
            let tracker = Arc::clone(&tracker);
            async move {
                backends.update(|_| tracker.run_build()).await.unwrap();
            }
        });

        wait_for_active_builds(&tracker, 1).await;
        backends.set_enable(&first_backend, false);
        tracker.unblock();
        update_task.await.unwrap();

        assert!(!backends.ready(&first_backend));
    }

    #[tokio::test]
    async fn test_health_check_service_runs_shared_registry_once() {
        let backend = Backend::new("1.0.0.1:80").unwrap();
        let discovery = Arc::new(MutableDiscovery::new(BTreeSet::from([backend])));
        let checks = Arc::new(AtomicUsize::new(0));
        let registry = Arc::new(HealthRegistry::new());
        registry.set_health_check(Box::new(CountingHealthCheck {
            checks: Arc::clone(&checks),
        }));
        let view = Backends::new_with_health_registry(Box::new(discovery), Arc::clone(&registry));
        view.update(|_| {}).await.unwrap();

        let service = HealthCheckService::new(registry);
        let (_shutdown_tx, shutdown) = tokio::sync::watch::channel(false);
        service.run(shutdown, None).await;

        assert_eq!(checks.load(Relaxed), 1);
    }

    #[tokio::test]
    async fn test_health_check_service_without_check_does_not_signal_ready() {
        let service = Arc::new(HealthCheckService::new(Arc::new(HealthRegistry::new())));
        let (shutdown_tx, shutdown) = tokio::sync::watch::channel(false);
        let (ready_tx, ready_rx) = tokio::sync::watch::channel(false);
        let task = tokio::spawn({
            let service = Arc::clone(&service);
            async move {
                service
                    .run(shutdown, Some(ServiceReadyNotifier::new(ready_tx)))
                    .await;
            }
        });

        tokio::task::yield_now().await;
        assert!(!*ready_rx.borrow());
        assert!(!task.is_finished());

        shutdown_tx.send(true).unwrap();
        task.await.unwrap();
    }

    #[tokio::test]
    async fn test_health_check_service_wakes_when_first_view_publishes_targets() {
        let backend = Backend::new("1.0.0.1:80").unwrap();
        let discovery = Arc::new(MutableDiscovery::new(BTreeSet::from([backend])));
        let checks = Arc::new(AtomicUsize::new(0));
        let registry = Arc::new(HealthRegistry::new());
        registry.set_health_check(Box::new(CountingHealthCheck {
            checks: Arc::clone(&checks),
        }));

        let mut service = HealthCheckService::new(Arc::clone(&registry));
        service.health_check_frequency = Some(Duration::from_secs(3600));
        let service = Arc::new(service);
        let (shutdown_tx, shutdown) = tokio::sync::watch::channel(false);
        let service_task = tokio::spawn({
            let service = Arc::clone(&service);
            async move { service.run(shutdown, None).await }
        });

        tokio::task::yield_now().await;
        let view = Backends::new_with_health_registry(Box::new(discovery), registry);
        view.update(|_| {}).await.unwrap();

        tokio::time::timeout(Duration::from_secs(1), async {
            while checks.load(Relaxed) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("health service did not wake for newly published targets");
        assert_eq!(checks.load(Relaxed), 1);

        shutdown_tx.send(true).unwrap();
        service_task.await.unwrap();
    }

    #[tokio::test]
    async fn test_health_check_service_reconciles_dropped_view() {
        let backend = Backend::new("1.0.0.1:80").unwrap();
        let discovery = Arc::new(MutableDiscovery::new(BTreeSet::from([backend])));
        let checks = Arc::new(AtomicUsize::new(0));
        let registry = Arc::new(HealthRegistry::new());
        registry.set_health_check(Box::new(CountingHealthCheck {
            checks: Arc::clone(&checks),
        }));
        let view = Backends::new_with_health_registry(Box::new(discovery), Arc::clone(&registry));
        view.update(|_| {}).await.unwrap();

        let mut service = HealthCheckService::new(Arc::clone(&registry));
        service.health_check_frequency = Some(Duration::from_secs(3600));
        let (shutdown_tx, shutdown) = tokio::sync::watch::channel(false);
        let service_task = tokio::spawn(async move { service.run(shutdown, None).await });
        tokio::time::timeout(Duration::from_secs(1), async {
            while checks.load(Relaxed) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("initial health check did not complete");

        drop(view);
        tokio::time::timeout(Duration::from_secs(1), async {
            while !registry.state.load().targets.is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("health service did not reconcile the dropped view");

        shutdown_tx.send(true).unwrap();
        service_task.await.unwrap();
    }

    #[tokio::test]
    async fn test_registry_notifies_only_when_first_target_is_added() {
        let discovery = Arc::new(MutableDiscovery::new(BTreeSet::new()));
        let registry = Arc::new(HealthRegistry::new());
        let first = Backends::new_with_health_registry(
            Box::new(Arc::clone(&discovery)),
            Arc::clone(&registry),
        );

        first.update(|_| {}).await.unwrap();
        assert!(tokio::time::timeout(
            Duration::from_millis(10),
            registry.targets_available.notified()
        )
        .await
        .is_err());

        let backend = Backend::new("1.0.0.1:80").unwrap();
        discovery.add(backend.clone());
        first.update(|_| {}).await.unwrap();
        tokio::time::timeout(
            Duration::from_secs(1),
            registry.targets_available.notified(),
        )
        .await
        .expect("first target did not notify the registry");

        let second = Backends::new_with_health_registry(
            Box::new(Arc::new(MutableDiscovery::new(BTreeSet::from([backend])))),
            Arc::clone(&registry),
        );
        second.update(|_| {}).await.unwrap();
        assert!(tokio::time::timeout(
            Duration::from_millis(10),
            registry.targets_available.notified()
        )
        .await
        .is_err());
    }

    #[tokio::test]
    async fn test_health_check_service_shuts_down_while_waiting_for_targets() {
        let registry = Arc::new(HealthRegistry::new());
        registry.set_health_check(Box::new(CountingHealthCheck {
            checks: Arc::new(AtomicUsize::new(0)),
        }));
        let mut service = HealthCheckService::new(registry);
        service.health_check_frequency = Some(Duration::from_secs(3600));

        let (shutdown_tx, shutdown) = tokio::sync::watch::channel(false);
        let service_task = tokio::spawn(async move { service.run(shutdown, None).await });
        tokio::task::yield_now().await;
        shutdown_tx.send(true).unwrap();

        tokio::time::timeout(Duration::from_secs(1), service_task)
            .await
            .expect("health service did not stop while waiting for targets")
            .expect("health service task failed");
    }

    // Keep a removed backend available until the selector rebuilds.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_stale_group_selector_serves_removed_backend_until_converged() {
        let backend = Backend::new("1.0.0.1:80").unwrap();
        let discovery = Arc::new(MutableDiscovery::new(BTreeSet::from([backend.clone()])));
        let registry = Arc::new(HealthRegistry::new());
        let tracker = Arc::new(BuildTracker::default());
        let group = LoadBalancerGroup::<TestSelection>::from_backends_with_configs(
            Backends::new_with_health_registry(
                Box::new(Arc::clone(&discovery)),
                Arc::clone(&registry),
            ),
            [Some(TestSelectionConfig {
                reverse: false,
                tracker: Some(Arc::clone(&tracker)),
            })],
        );
        tracker.reset();

        group.update().await.unwrap();
        wait_for_group_generation(&group, 1).await;
        assert_eq!(group.select(0, b"", 10), Some(backend.clone()));

        // Remove the backend while the rebuild is blocked.
        tracker.block();
        discovery.replace(BTreeSet::new());
        group.update().await.unwrap();
        wait_for_active_builds(&tracker, 1).await;

        assert_eq!(group.selector_generation(0), Some(1));
        assert_eq!(group.select(0, b"", 10), Some(backend.clone()));
        assert_eq!(registry.target_count(), 0);

        // Finish the rebuild and remove the old readiness.
        tracker.unblock();
        wait_for_group_generation(&group, 2).await;
        assert_eq!(group.select(0, b"", 10), None);
        group.update().await.unwrap();
        assert!(!group.backends().ready(&backend));
    }

    // Keep selection available during a disjoint membership change.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_group_selection_available_across_disjoint_membership_swap() {
        let old = Backend::new("1.0.0.1:80").unwrap();
        let new = Backend::new("1.0.0.2:80").unwrap();
        let discovery = Arc::new(MutableDiscovery::new(BTreeSet::from([old.clone()])));
        let tracker = Arc::new(BuildTracker::default());
        let group = LoadBalancerGroup::<TestSelection>::from_backends_with_configs(
            Backends::new(Box::new(Arc::clone(&discovery))),
            [Some(TestSelectionConfig {
                reverse: false,
                tracker: Some(Arc::clone(&tracker)),
            })],
        );
        tracker.reset();

        group.update().await.unwrap();
        wait_for_group_generation(&group, 1).await;
        assert_eq!(group.select(0, b"", 10), Some(old.clone()));

        // Change membership while the rebuild is blocked.
        tracker.block();
        discovery.replace(BTreeSet::from([new.clone()]));
        group.update().await.unwrap();
        wait_for_active_builds(&tracker, 1).await;
        assert_eq!(group.selector_generation(0), Some(1));
        assert_eq!(group.backend_generation(), 2);

        // The old selector can still use `old` through the readiness snapshot
        // it was published with, even though current membership dropped it.
        assert_eq!(group.select(0, b"", 10), Some(old.clone()));
        // Current-view readiness reports only the current membership.
        assert!(!group.backends().ready(&old));
        assert!(group.backends().ready(&new));

        // The old selector's snapshot shares the removed backend's enablement
        // flag, reached through the interner, so manual disable/enable affects it.
        group.backends().set_enable(&old, false);
        assert_eq!(group.select(0, b"", 10), None);
        group.backends().set_enable(&old, true);
        assert_eq!(group.select(0, b"", 10), Some(old.clone()));

        // Finish the rebuild. The old selector and its readiness snapshot are
        // replaced and released; no explicit prune is needed.
        tracker.unblock();
        wait_for_group_generation(&group, 2).await;
        assert_eq!(group.select(0, b"", 10), Some(new.clone()));
        assert!(!group.backends().ready(&old));
        assert!(group.backends().ready(&new));
        assert_eq!(group.select(0, b"", 10), Some(new));
    }

    // Keep each backend identity paired with its own readiness while weights change.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_group_weight_churn_preserves_selector_identity() {
        let addr = "1.0.0.1:80";
        let w100 = Backend::new_with_weight(addr, 100).unwrap();
        let w101 = Backend::new_with_weight(addr, 101).unwrap();
        let w102 = Backend::new_with_weight(addr, 102).unwrap();
        let discovery = Arc::new(MutableDiscovery::new(BTreeSet::from([w100.clone()])));
        let tracker = Arc::new(BuildTracker::default());
        let group = LoadBalancerGroup::<TestSelection>::from_backends_with_configs(
            Backends::new(Box::new(Arc::clone(&discovery))),
            [Some(TestSelectionConfig {
                reverse: false,
                tracker: Some(Arc::clone(&tracker)),
            })],
        );

        group.update().await.unwrap();
        wait_for_group_generation(&group, 1).await;
        assert_eq!(group.select(0, b"", 10), Some(w100.clone()));

        // Change the weight twice while the rebuild is blocked.
        tracker.reset();
        tracker.block();
        discovery.replace(BTreeSet::from([w101.clone()]));
        group.update().await.unwrap();
        discovery.replace(BTreeSet::from([w102.clone()]));
        group.update().await.unwrap();
        assert_eq!(group.backend_generation(), 3);
        wait_for_active_builds(&tracker, 1).await;
        assert_eq!(group.selector_generation(0), Some(1));

        // Current-view readiness contains only the latest identity, while the
        // old selector retains the exact readiness paired with `w100`.
        assert_eq!(group.select(0, b"", 10), Some(w100.clone()));
        assert!(!group.backends().ready(&w100));
        assert!(!group.backends().ready(&w101));
        assert!(group.backends().ready(&w102));

        // Disabling the current identity does not affect the old selector.
        group.backends().set_enable(&w102, false);
        assert!(!group.backends().ready(&w102));
        assert_eq!(group.select(0, b"", 10), Some(w100.clone()));

        // The old identity can still be disabled through its retained handle.
        group.backends().set_enable(&w100, false);
        assert_eq!(group.select(0, b"", 10), None);

        // Re-enable both independent identities before publication converges.
        group.backends().set_enable(&w100, true);
        assert_eq!(group.select(0, b"", 10), Some(w100.clone()));
        group.backends().set_enable(&w102, true);
        assert!(group.backends().ready(&w102));

        // Finish the rebuild at the latest weight.
        tracker.unblock();
        wait_for_group_generation(&group, 3).await;
        assert_eq!(group.selector_generation(0), Some(3));
        assert_eq!(group.select(0, b"", 10), Some(w102.clone()));

        // The latest identity is current and ready.
        assert!(group.backends().ready(&w102));
        assert_eq!(group.select(0, b"", 10), Some(w102));
    }

    // An old selector keeps serving a backend whose address disappeared before
    // its rebuild, from the readiness snapshot it was published with.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_group_old_selector_serves_disappeared_address_via_snapshot() {
        let old_addr = "1.0.0.1:80";
        let w100 = Backend::new_with_weight(old_addr, 100).unwrap();
        let w101 = Backend::new_with_weight(old_addr, 101).unwrap();
        let moved = Backend::new("1.0.0.2:80").unwrap();
        let discovery = Arc::new(MutableDiscovery::new(BTreeSet::from([w100.clone()])));
        let tracker = Arc::new(BuildTracker::default());
        let group = LoadBalancerGroup::<TestSelection>::from_backends_with_configs(
            Backends::new(Box::new(Arc::clone(&discovery))),
            [Some(TestSelectionConfig {
                reverse: false,
                tracker: Some(Arc::clone(&tracker)),
            })],
        );

        group.update().await.unwrap();
        wait_for_group_generation(&group, 1).await;
        assert_eq!(group.select(0, b"", 10), Some(w100.clone()));

        // Change the weight, then remove the address while blocked.
        tracker.reset();
        tracker.block();
        discovery.replace(BTreeSet::from([w101.clone()]));
        group.update().await.unwrap();
        discovery.replace(BTreeSet::from([moved.clone()]));
        group.update().await.unwrap();
        assert_eq!(group.backend_generation(), 3);
        wait_for_active_builds(&tracker, 1).await;
        assert_eq!(group.selector_generation(0), Some(1));

        // The old selector keeps serving `w100` from its own readiness
        // snapshot, though its address left current membership.
        assert!(!group.backends().ready(&w100));
        assert_eq!(group.select(0, b"", 10), Some(w100.clone()));
        assert!(group.backends().ready(&moved));

        // Finish the rebuild. The old selector and snapshot are released.
        tracker.unblock();
        wait_for_group_generation(&group, 3).await;
        assert_eq!(group.select(0, b"", 10), Some(moved.clone()));
        assert!(!group.backends().ready(&w100));
        assert!(group.backends().ready(&moved));
        assert_eq!(group.select(0, b"", 10), Some(moved));
    }

    // An older published selector keeps selecting its own disjoint backend from
    // the readiness snapshot it was published with, even after a newer
    // generation is published. The snapshot's lifetime ends when the reader
    // drops it; no prune step runs.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_old_published_snapshot_selects_disjoint_backend_after_update() {
        let old = Backend::new("1.0.0.1:80").unwrap();
        let new = Backend::new("1.0.0.2:80").unwrap();
        let discovery = Arc::new(MutableDiscovery::new(BTreeSet::from([old.clone()])));
        let group = LoadBalancerGroup::<TestSelection>::from_backends_with_configs(
            Backends::new(Box::new(Arc::clone(&discovery))),
            [Some(TestSelectionConfig {
                reverse: false,
                tracker: None,
            })],
        );

        group.update().await.unwrap();
        wait_for_group_generation(&group, 1).await;

        // Capture the generation-1 published selector and hold it across a later
        // update, as an in-flight request iterator would.
        let published = group.selectors[0].selector.load_full();
        assert!(published.readiness.ready(&old));

        // Publish a disjoint generation 2.
        discovery.replace(BTreeSet::from([new.clone()]));
        group.update().await.unwrap();
        wait_for_group_generation(&group, 2).await;

        // The held generation-1 snapshot still yields `old` and still marks it
        // ready, though current membership dropped it.
        let mut iter = published.selector.iter(b"");
        assert_eq!(iter.next(), Some(&old));
        assert!(published.readiness.ready(&old));
        assert!(!group.backends().ready(&old));

        // Current selection uses the generation-2 snapshot.
        assert_eq!(group.select(0, b"", 10), Some(new.clone()));
        assert!(group.backends().ready(&new));

        // Dropping the reader ends the old snapshot's lifetime without a prune.
        drop(iter);
        drop(published);
        assert_eq!(group.select(0, b"", 10), Some(new));
    }

    // Two selectors published at different generations each consult the
    // readiness snapshot they were built with, not a shared current view.
    #[tokio::test]
    async fn test_selectors_at_different_generations_use_matching_snapshots() {
        let a = Backend::new("1.0.0.1:80").unwrap();
        let b = Backend::new("1.0.0.2:80").unwrap();

        // Build two independent readiness snapshots from separate views.
        let backends_a =
            Backends::new(Box::new(Arc::new(MutableDiscovery::new(BTreeSet::from([
                a.clone(),
            ])))));
        backends_a.update(|_| {}).await.unwrap();
        let readiness_a = backends_a.readiness_snapshot();
        assert!(readiness_a.ready(&a));

        let backends_b =
            Backends::new(Box::new(Arc::new(MutableDiscovery::new(BTreeSet::from([
                b.clone(),
            ])))));
        backends_b.update(|_| {}).await.unwrap();
        let readiness_b = backends_b.readiness_snapshot();
        assert!(readiness_b.ready(&b));

        // A group with two selectors; publish each slot at a distinct generation
        // with its matching snapshot.
        let group = LoadBalancerGroup::<TestSelection>::from_backends_with_configs(
            Backends::new(Box::new(Arc::new(MutableDiscovery::new(BTreeSet::new())))),
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
        group.selectors[0]
            .selector
            .store(Arc::new(PublishedSelector::new(
                TestSelection::build(&BTreeSet::from([a.clone()])),
                readiness_a,
            )));
        group.selectors[0].generation.store(1, Release);
        group.selectors[1]
            .selector
            .store(Arc::new(PublishedSelector::new(
                TestSelection::build(&BTreeSet::from([b.clone()])),
                readiness_b,
            )));
        group.selectors[1].generation.store(2, Release);

        // Each selector selects its own backend, marked ready by its own
        // snapshot.
        assert_eq!(group.select(0, b"", 10), Some(a.clone()));
        assert_eq!(group.select(1, b"", 10), Some(b.clone()));

        // The snapshots are distinct: neither knows the other's backend.
        let published0 = group.selectors[0].selector.load();
        assert!(published0.readiness.ready(&a));
        assert!(!published0.readiness.ready(&b));
        let published1 = group.selectors[1].selector.load();
        assert!(published1.readiness.ready(&b));
        assert!(!published1.readiness.ready(&a));
    }

    // The transition state published during a synchronous update callback must
    // cover both the outgoing and incoming readiness, so a request in the
    // callback (before the selector is swapped) does not lose the old backend.
    // Once the callback returns, only the current membership is ready.
    #[tokio::test]
    async fn test_transition_snapshot_covers_outgoing_and_incoming_readiness() {
        let a = Backend::new("1.0.0.1:80").unwrap();
        let b = Backend::new("1.0.0.2:80").unwrap();
        let discovery = Arc::new(MutableDiscovery::new(BTreeSet::from([a.clone()])));
        let backends = Backends::new(Box::new(Arc::clone(&discovery)));

        backends.update(|_| {}).await.unwrap();
        assert!(backends.ready(&a));

        // Disjoint swap A -> B. During the callback both must be ready.
        discovery.replace(BTreeSet::from([b.clone()]));
        let ran = AtomicBool::new(false);
        backends
            .update(|_| {
                ran.store(true, Relaxed);
                assert!(
                    backends.ready(&a),
                    "outgoing backend dropped mid-transition"
                );
                assert!(backends.ready(&b));
            })
            .await
            .unwrap();
        assert!(ran.load(Relaxed));

        // After the callback the current view holds only B.
        assert!(!backends.ready(&a));
        assert!(backends.ready(&b));
    }

    #[tokio::test]
    async fn test_current_readiness_uses_exact_backend_identity() {
        let old_backend = Backend::new_with_weight("1.0.0.1:80", 1).unwrap();
        let new_backend = Backend::new_with_weight("1.0.0.1:80", 2).unwrap();
        let discovery = Arc::new(MutableDiscovery::new(BTreeSet::from([old_backend.clone()])));
        let backends = Backends::new(Box::new(discovery.clone()));

        backends.update(|_| {}).await.unwrap();
        assert!(backends.ready(&old_backend));

        discovery.replace(BTreeSet::from([new_backend.clone()]));
        backends.update(|_| {}).await.unwrap();
        assert!(!backends.ready(&old_backend));
        assert!(backends.ready(&new_backend));

        discovery.replace(BTreeSet::new());
        backends.update(|_| {}).await.unwrap();
        assert!(!backends.ready(&old_backend));
        assert!(!backends.ready(&new_backend));
    }

    // Backend variants sharing an address retain independent enablement.
    #[tokio::test]
    async fn test_same_address_variants_have_independent_readiness() {
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

        backends.set_enable(&backend_v1, false);
        assert!(!backends.ready(&backend_v1));
        assert!(backends.ready(&backend_v2));
        assert!(!backends.ready(&stale_backend));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_load_balancer_group_bounds_and_coalesces_rebuilds() {
        let b1 = Backend::new("1.0.0.1:80").unwrap();
        let b2 = Backend::new("1.1.1.1:80").unwrap();
        let b3 = Backend::new("1.1.1.2:80").unwrap();
        let b4 = Backend::new("1.1.1.3:80").unwrap();
        let b5 = Backend::new("1.1.1.4:80").unwrap();
        let discovery = Arc::new(MutableDiscovery::new(BTreeSet::from([b1.clone(), b2])));
        let tracker = Arc::new(BuildTracker::default());
        let configs = (0..6).map(|index| {
            Some(TestSelectionConfig {
                reverse: index % 2 == 1,
                tracker: Some(Arc::clone(&tracker)),
            })
        });
        let group = LoadBalancerGroup::<TestSelection>::from_backends_with_configs(
            Backends::new(Box::new(discovery.clone())),
            configs,
        );
        tracker.reset();
        tracker.block();

        group.update().await.unwrap();
        assert_eq!(group.backend_generation(), 1);
        assert_eq!(group.selector_generation(0), Some(0));
        wait_for_active_builds(&tracker, 1).await;

        discovery.add(b3);
        group.update().await.unwrap();
        discovery.add(b4);
        group.update().await.unwrap();
        discovery.add(b5.clone());
        group.update().await.unwrap();
        assert_eq!(group.backend_generation(), 4);

        tracker.unblock();
        wait_for_group_generation(&group, 4).await;

        assert_eq!(tracker.max_active.load(Relaxed), 1);
        assert_eq!(tracker.max_live_selectors.load(Relaxed), 7);
        assert_eq!(tracker.builds.load(Relaxed), 7);
        let mut coalesced_rebuilds = Vec::new();
        for selector_index in 0..group.selector_count() {
            assert_eq!(group.selector_generation(selector_index), Some(4));
            coalesced_rebuilds.push(group.selector_coalesced_rebuilds(selector_index).unwrap());
            assert_eq!(group.selector_failed_rebuilds(selector_index), Some(0));
            assert_eq!(
                group
                    .selector_last_update_timing(selector_index)
                    .map(|timing| timing.generation),
                Some(4)
            );
        }
        coalesced_rebuilds.sort_unstable();
        assert_eq!(coalesced_rebuilds, [2, 3, 3, 3, 3, 3]);
        assert_eq!(group.select(1, b"", 10), Some(b5));
        assert_eq!(group.select(0, b"", 10), Some(b1));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_shared_rebuild_gate_never_overlaps_retired_and_replacement_selectors() {
        let b1 = Backend::new("1.0.0.1:80").unwrap();
        let b2 = Backend::new("1.1.1.1:80").unwrap();
        let b3 = Backend::new("1.1.1.2:80").unwrap();
        let first_discovery = Arc::new(MutableDiscovery::new(BTreeSet::from([b1.clone()])));
        let second_discovery = Arc::new(MutableDiscovery::new(BTreeSet::from([b1.clone()])));
        let tracker = Arc::new(BuildTracker::default());
        let gate = Arc::new(SelectorRebuildGate::new());
        let config = || {
            [Some(TestSelectionConfig {
                reverse: false,
                tracker: Some(Arc::clone(&tracker)),
            })]
        };

        let first = LoadBalancerGroup::<TestSelection>::from_backends_with_configs(
            Backends::new(Box::new(Arc::clone(&first_discovery))),
            config(),
        )
        .with_rebuild_gate(Arc::clone(&gate));
        let second = LoadBalancerGroup::<TestSelection>::from_backends_with_configs(
            Backends::new(Box::new(Arc::clone(&second_discovery))),
            config(),
        )
        .with_rebuild_gate(gate);

        first.update().await.unwrap();
        second.update().await.unwrap();
        wait_for_group_generation(&first, 1).await;
        wait_for_group_generation(&second, 1).await;
        tracker.reset();

        let retained_first_generation = first.selectors[0].selector.load_full();
        first_discovery.add(b2.clone());
        first.update().await.unwrap();
        wait_for_group_generation(&first, 2).await;
        wait_for_builds_started(&tracker, 1).await;

        second_discovery.add(b2);
        second.update().await.unwrap();
        first_discovery.add(b3);
        first.update().await.unwrap();
        tokio::time::sleep(Duration::from_millis(25)).await;

        assert_eq!(tracker.active.load(Relaxed), 0);
        assert_eq!(tracker.builds.load(Relaxed), 1);
        assert_eq!(tracker.live_selectors.load(Relaxed), 3);
        assert_eq!(tracker.max_live_selectors.load(Relaxed), 3);

        drop(retained_first_generation);
        wait_for_group_generation(&first, 3).await;
        wait_for_group_generation(&second, 2).await;

        assert_eq!(tracker.builds.load(Relaxed), 3);
        assert_eq!(tracker.max_active.load(Relaxed), 1);
        assert_eq!(tracker.max_live_selectors.load(Relaxed), 3);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_rebuild_gate_cancellation_preserves_retired_selector() {
        let gate = Arc::new(SelectorRebuildGate::new());
        let permit = gate.acquire().await;
        let retired = Arc::new(PublishedSelector::new(
            TestSelection::build(&BTreeSet::new()),
            ReadinessSnapshot::empty(),
        ));
        let release_signal = retired.release_signal();
        gate.retire(&retired);
        drop(permit);

        let waiting_gate = Arc::clone(&gate);
        let waiting = tokio::spawn(async move { waiting_gate.acquire().await });
        tokio::time::timeout(Duration::from_secs(5), async {
            while Arc::strong_count(&release_signal) < 4 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("gate waiter did not start waiting for the retired selector");

        waiting.abort();
        assert!(waiting.await.unwrap_err().is_cancelled());
        assert!(gate
            .retired
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, &release_signal)));

        let next_gate = Arc::clone(&gate);
        let next = tokio::spawn(async move { next_gate.acquire().await });
        tokio::time::timeout(Duration::from_secs(5), async {
            while Arc::strong_count(&release_signal) < 4 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("next gate acquire did not wait for the retired selector");
        assert!(!next.is_finished());

        drop(retired);
        let _ = tokio::time::timeout(Duration::from_secs(5), next)
            .await
            .expect("next gate acquire did not observe selector release")
            .expect("next gate acquire task failed");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_cancelled_selector_destruction_holds_rebuild_gate() {
        let first_backend = Backend::new("1.0.0.1:80").unwrap();
        let second_backend = Backend::new("1.0.0.2:80").unwrap();
        let first_discovery = Arc::new(MutableDiscovery::new(BTreeSet::from([first_backend])));
        let second_discovery = Arc::new(MutableDiscovery::new(BTreeSet::from([second_backend])));
        let tracker = Arc::new(BuildTracker::default());
        let gate = Arc::new(SelectorRebuildGate::new());
        let config = || {
            [Some(TestSelectionConfig {
                reverse: false,
                tracker: Some(Arc::clone(&tracker)),
            })]
        };
        let first = LoadBalancerGroup::<TestSelection>::from_backends_with_configs(
            Backends::new(Box::new(first_discovery)),
            config(),
        )
        .with_rebuild_gate(Arc::clone(&gate));
        let second = LoadBalancerGroup::<TestSelection>::from_backends_with_configs(
            Backends::new(Box::new(second_discovery)),
            config(),
        )
        .with_rebuild_gate(gate);
        tracker.reset();
        tracker.block();

        first.update().await.unwrap();
        wait_for_active_builds(&tracker, 1).await;
        let first_slot = Arc::clone(&first.selectors[0]);
        drop(first);
        second.update().await.unwrap();

        tracker.block_drop();
        tracker.unblock();
        tokio::time::timeout(Duration::from_secs(5), async {
            while tracker.drop_waiters.load(Relaxed) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("cancelled selector destruction did not start");

        assert_eq!(tracker.builds.load(Relaxed), 1);
        assert_eq!(second.selector_generation(0), Some(0));

        tracker.unblock_drop();
        wait_for_group_generation(&second, 1).await;
        drop(first_slot);
        assert_eq!(tracker.builds.load(Relaxed), 2);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_selector_rebuild_task_abort_restores_in_flight_request() {
        let backend = Backend::new("1.0.0.1:80").unwrap();
        let tracker = Arc::new(BuildTracker::default());
        let group = LoadBalancerGroup::<TestSelection>::from_backends_with_configs(
            Backends::new(Box::new(Arc::new(MutableDiscovery::new(BTreeSet::new())))),
            [Some(TestSelectionConfig {
                reverse: false,
                tracker: Some(Arc::clone(&tracker)),
            })],
        );
        tracker.reset();
        tracker.block();

        // A distinguishable readiness snapshot travels with the request, so the
        // abort path must restore it alongside membership and generation.
        let readiness = ReadinessSnapshot(Arc::new(HashMap::from([(
            backend.hash_key(),
            BackendReadiness {
                enabled: Arc::new(AtomicBool::new(true)),
                health: Health::default(),
            },
        )])));
        let slot = Arc::clone(&group.selectors[0]);
        {
            let mut rebuild_state = lock_rebuild_state(&slot.rebuild_state);
            rebuild_state.is_running = true;
            rebuild_state.pending_request = Some(SelectorRebuildRequest {
                generation: 1,
                backends: Arc::new(BTreeSet::from([backend.clone()])),
                readiness,
                requested_at: Instant::now(),
            });
        }
        let rebuild_task = tokio::spawn(run_selector_rebuilds(
            Arc::clone(&slot),
            Arc::clone(&group.rebuild_gate),
            Arc::clone(&group.rebuild_notify),
            Arc::clone(&group.rebuild_cancellation),
        ));
        wait_for_active_builds(&tracker, 1).await;

        rebuild_task.abort();
        assert!(rebuild_task.await.unwrap_err().is_cancelled());
        {
            let state = lock_rebuild_state(&slot.rebuild_state);
            assert!(!state.is_running);
            assert_eq!(
                state
                    .pending_request
                    .as_ref()
                    .map(|request| request.generation),
                Some(1)
            );
            assert!(
                state
                    .pending_request
                    .as_ref()
                    .is_some_and(|request| request.readiness.ready(&backend)),
                "aborted rebuild lost its readiness bundle"
            );
        }

        tracker.unblock();
        tokio::time::timeout(Duration::from_secs(5), async {
            while tracker.active.load(Relaxed) > 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("detached blocking build did not finish");
    }

    #[tokio::test]
    async fn test_selector_rebuild_task_guard_cleans_up_after_panic() {
        let group = LoadBalancerGroup::<TestSelection>::from_backends_with_configs(
            Backends::new(Box::new(Arc::new(MutableDiscovery::new(BTreeSet::new())))),
            [Some(TestSelectionConfig {
                reverse: false,
                tracker: None,
            })],
        );
        let slot = Arc::clone(&group.selectors[0]);
        lock_rebuild_state(&slot.rebuild_state).is_running = true;
        let rebuild_notify = Arc::new(Notify::new());

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe({
            let slot = Arc::clone(&slot);
            let rebuild_notify = Arc::clone(&rebuild_notify);
            move || {
                let _guard = SelectorRebuildTaskGuard::new(slot, rebuild_notify);
                panic!("intentional selector rebuild panic");
            }
        }));

        assert!(result.is_err());
        assert!(!lock_rebuild_state(&slot.rebuild_state).is_running);
        assert_eq!(slot.failed_rebuilds.load(Relaxed), 1);
        tokio::time::timeout(Duration::from_secs(1), rebuild_notify.notified())
            .await
            .expect("panic cleanup did not notify the background loop");
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
            allow_success: Notify::new(),
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
            .expect("load balancer group did not stop")
            .expect("load balancer group task failed");
    }

    // Finding 1: a blocking selector build must not open a fail-closed window;
    // the previously published selection stays usable until the new selector is
    // built and published, then the new selection becomes usable.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_select_during_blocking_update_keeps_old_backend_ready() {
        let old = Backend::new("1.0.0.1:80").unwrap();
        let new = Backend::new("1.0.0.2:80").unwrap();
        let discovery = Arc::new(MutableDiscovery::new(BTreeSet::from([old.clone()])));
        let tracker = Arc::new(BuildTracker::default());
        let lb = Arc::new(LoadBalancer::<TestSelection>::from_backends_with_config(
            Backends::new(Box::new(Arc::clone(&discovery))),
            Some(TestSelectionConfig {
                reverse: false,
                tracker: Some(Arc::clone(&tracker)),
            }),
        ));

        lb.update().await.unwrap();
        assert_eq!(lb.select(b"", 10), Some(old.clone()));

        tracker.reset();
        tracker.block();
        discovery.replace(BTreeSet::from([new.clone()]));
        let update_lb = Arc::clone(&lb);
        let update_task = tokio::spawn(async move { update_lb.update().await.unwrap() });

        // The replacement selector is still being built; the old selection and
        // its readiness must remain usable for the whole build.
        wait_for_active_builds(&tracker, 1).await;
        assert_eq!(lb.select(b"", 10), Some(old.clone()));
        assert!(lb.backends().ready(&old));

        tracker.unblock();
        update_task.await.unwrap();

        assert_eq!(lb.select(b"", 10), Some(new.clone()));
        assert!(lb.backends().ready(&new));
        assert!(!lb.backends().ready(&old));
    }

    #[tokio::test]
    async fn test_new_selector_is_ready_before_update_callback_returns() {
        let old = Backend::new("1.0.0.1:80").unwrap();
        let new = Backend::new("1.0.0.2:80").unwrap();
        let discovery = Arc::new(MutableDiscovery::new(BTreeSet::from([old.clone()])));
        let backends = Backends::new(Box::new(Arc::clone(&discovery)));
        backends.update(|_| {}).await.unwrap();
        let selector = ArcSwap::new(Arc::new(TestSelection::build(&BTreeSet::from([
            old.clone()
        ]))));

        discovery.replace(BTreeSet::from([new.clone()]));
        backends
            .update(|members| {
                selector.store(Arc::new(TestSelection::build(&members)));
                let selected = selector.load().backends[0].clone();
                assert_eq!(selected, new);
                assert!(backends.ready(&selected));
                assert!(backends.ready(&old));
            })
            .await
            .unwrap();

        assert!(backends.ready(&new));
        assert!(!backends.ready(&old));
    }

    // Finding 2: one-shot mode must wait for the first target before running its
    // single pass and signaling ready, even if the service starts before any
    // target is published.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_health_check_service_one_shot_waits_for_first_target() {
        let backend = Backend::new("1.0.0.1:80").unwrap();
        let discovery = Arc::new(MutableDiscovery::new(BTreeSet::new()));
        let checks = Arc::new(AtomicUsize::new(0));
        let registry = Arc::new(HealthRegistry::new());
        registry.set_health_check(Box::new(CountingHealthCheck {
            checks: Arc::clone(&checks),
        }));
        let view = Backends::new_with_health_registry(
            Box::new(Arc::clone(&discovery)),
            Arc::clone(&registry),
        );
        view.update(|_| {}).await.unwrap();

        // One-shot: health_check_frequency defaults to None.
        let service = HealthCheckService::new(Arc::clone(&registry));
        let (shutdown_tx, shutdown) = tokio::sync::watch::channel(false);
        let (ready_tx, ready_rx) = tokio::sync::watch::channel(false);
        let task = tokio::spawn(async move {
            service
                .run(shutdown, Some(ServiceReadyNotifier::new(ready_tx)))
                .await;
        });

        // With an empty registry the single pass must not run or signal ready.
        tokio::time::sleep(Duration::from_millis(25)).await;
        assert_eq!(checks.load(Relaxed), 0);
        assert!(!*ready_rx.borrow());
        assert!(!task.is_finished());

        // Publishing the first target lets the single pass run and complete.
        discovery.add(backend);
        view.update(|_| {}).await.unwrap();

        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("one-shot health service did not finish")
            .expect("health service task failed");
        assert_eq!(checks.load(Relaxed), 1);
        assert!(*ready_rx.borrow());

        drop(shutdown_tx);
    }

    // Finding 3: readiness caches the shared health handle in the view state, so
    // health observations (and reconciliation triggered by other views) stay
    // visible without a second registry snapshot.
    #[tokio::test]
    async fn test_shared_health_update_visible_after_other_view_reconcile() {
        let backend = Backend::new("1.0.0.1:80").unwrap();
        let other = Backend::new("1.0.0.2:80").unwrap();
        let registry = Arc::new(HealthRegistry::new());
        registry.set_health_check(Box::new(SelectiveHealthCheck {
            checks: Arc::new(AtomicUsize::new(0)),
            unhealthy: Some(backend.addr.clone()),
        }));
        let first_discovery = Arc::new(MutableDiscovery::new(BTreeSet::from([backend.clone()])));
        let first = Backends::new_with_health_registry(
            Box::new(Arc::clone(&first_discovery)),
            Arc::clone(&registry),
        );
        first.update(|_| {}).await.unwrap();
        assert!(first.ready(&backend));

        // Flip the shared health to unhealthy.
        registry.run_health_check(false).await;
        assert!(!first.ready(&backend));

        // A second view joining triggers reconciliation; the cached handle must
        // still reflect the shared unhealthy state (reconcile preserves it).
        let second = Backends::new_with_health_registry(
            Box::new(Arc::new(MutableDiscovery::new(BTreeSet::from([
                other.clone()
            ])))),
            Arc::clone(&registry),
        );
        second.update(|_| {}).await.unwrap();
        assert!(!first.ready(&backend));

        // A membership change on the first view re-snapshots handles and must
        // continue to observe the shared unhealthy state.
        first_discovery.add(other.clone());
        first.update(|_| {}).await.unwrap();
        assert!(!first.ready(&backend));
        assert!(first.ready(&other));
    }

    // Finding 4: a selector whose build fails once must still converge to the
    // desired generation without another discovery change.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_group_selector_retries_after_failed_build() {
        let backend = Backend::new("1.0.0.1:80").unwrap();
        let discovery = Arc::new(MutableDiscovery::new(BTreeSet::from([backend.clone()])));
        let tracker = Arc::new(BuildTracker::default());
        let group = LoadBalancerGroup::<TestSelection>::from_backends_with_configs(
            Backends::new(Box::new(Arc::clone(&discovery))),
            [Some(TestSelectionConfig {
                reverse: false,
                tracker: Some(Arc::clone(&tracker)),
            })],
        );

        // The construction build already ran; make the next build panic once.
        tracker.set_panics(1);
        group.update().await.unwrap();
        assert_eq!(group.backend_generation(), 1);

        // Despite the first rebuild panicking, the selector converges via retry.
        wait_for_group_generation(&group, 1).await;
        assert_eq!(group.selector_failed_rebuilds(0), Some(1));
        assert_eq!(group.select(0, b"", 10), Some(backend));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_new_generation_interrupts_rebuild_backoff() {
        let first = Backend::new("1.0.0.1:80").unwrap();
        let second = Backend::new("1.0.0.2:80").unwrap();
        let discovery = Arc::new(MutableDiscovery::new(BTreeSet::from([first])));
        let tracker = Arc::new(BuildTracker::default());
        let group = LoadBalancerGroup::<TestSelection>::from_backends_with_configs(
            Backends::new(Box::new(Arc::clone(&discovery))),
            [Some(TestSelectionConfig {
                reverse: false,
                tracker: Some(Arc::clone(&tracker)),
            })],
        );

        tracker.set_panics(20);
        group.update().await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while group.selector_failed_rebuilds(0).unwrap_or_default() < 6 {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("selector did not enter exponential backoff");

        tracker.set_panics(0);
        discovery.add(second);
        group.update().await.unwrap();
        tokio::time::timeout(Duration::from_millis(150), async {
            while group.selector_generation(0) != Some(2) {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("new generation did not interrupt stale rebuild backoff");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_group_drop_cancels_failed_rebuild_retries() {
        let backend = Backend::new("1.0.0.1:80").unwrap();
        let discovery = Arc::new(MutableDiscovery::new(BTreeSet::from([backend])));
        let tracker = Arc::new(BuildTracker::default());
        let group = LoadBalancerGroup::<TestSelection>::from_backends_with_configs(
            Backends::new(Box::new(discovery)),
            [Some(TestSelectionConfig {
                reverse: false,
                tracker: Some(Arc::clone(&tracker)),
            })],
        );

        tracker.set_panics(usize::MAX);
        group.update().await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while group.selector_failed_rebuilds(0).unwrap_or_default() < 2 {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("selector did not begin retrying failed rebuilds");

        let slot = Arc::clone(&group.selectors[0]);
        drop(group);
        tokio::time::timeout(Duration::from_secs(5), async {
            while lock_rebuild_state(&slot.rebuild_state).is_running {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("selector rebuild task did not stop after group drop");
        let remaining_panics = tracker.panics.load(Relaxed);
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(tracker.panics.load(Relaxed), remaining_panics);
    }

    // Finding 5: before readiness is signaled, every successful update advances
    // the readiness target to the latest generation, so a burst of updates does
    // not let a stale selector satisfy readiness.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_group_readiness_tracks_latest_generation_during_burst() {
        let b1 = Backend::new("1.0.0.1:80").unwrap();
        let b2 = Backend::new("1.1.1.1:80").unwrap();
        let discovery = Arc::new(MutableDiscovery::new(BTreeSet::from([b1.clone()])));
        let tracker = Arc::new(BuildTracker::default());
        let mut group = LoadBalancerGroup::<TestSelection>::from_backends_with_configs(
            Backends::new(Box::new(Arc::clone(&discovery))),
            [Some(TestSelectionConfig {
                reverse: false,
                tracker: Some(Arc::clone(&tracker)),
            })],
        );
        group.update_frequency = Some(Duration::from_millis(5));
        let group = Arc::new(group);
        tracker.reset();
        // Hold the generation-1 build so generation 2 can arrive first.
        tracker.block();

        let (shutdown_tx, shutdown) = tokio::sync::watch::channel(false);
        let (ready_tx, mut ready_rx) = tokio::sync::watch::channel(false);
        let run_group = Arc::clone(&group);
        let run_task = tokio::spawn(async move {
            run_group
                .run(shutdown, Some(ServiceReadyNotifier::new(ready_tx)))
                .await;
        });

        wait_for_active_builds(&tracker, 1).await;
        assert_eq!(group.backend_generation(), 1);

        // Generation 2 arrives while the generation-1 build is still blocked.
        discovery.add(b2.clone());
        tokio::time::timeout(Duration::from_secs(5), async {
            while group.backend_generation() < 2 {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("generation 2 did not register");

        // Let generation 1 complete but deterministically block generation 2's
        // build (the second build, index 1).
        tracker.set_block_from(1);
        tracker.unblock();

        tokio::time::timeout(Duration::from_secs(5), async {
            while group.selector_generation(0) != Some(1) {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("generation 1 selector did not publish");
        wait_for_active_builds(&tracker, 1).await;

        // Current membership is generation 2 but the selector is at generation
        // 1, so readiness must not fire yet.
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(
            !*ready_rx.borrow(),
            "readiness fired while a selector was still stale"
        );

        // Releasing generation 2 lets the selector converge and readiness fire.
        tracker.set_block_from(usize::MAX);
        tracker.unblock();
        tokio::time::timeout(Duration::from_secs(5), ready_rx.changed())
            .await
            .expect("ready notification timed out")
            .expect("ready notifier was dropped");
        assert!(*ready_rx.borrow());
        assert_eq!(group.selector_generation(0), Some(2));

        shutdown_tx.send(true).unwrap();
        let _ = tokio::time::timeout(Duration::from_secs(5), run_task).await;
    }

    // Finding 6: the release signal for a retired selector must not fire until
    // the selector (including references held by in-flight iterators) is fully
    // destroyed, so the next build cannot start early.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_selector_release_waits_for_iterator_and_destructor() {
        let b1 = Backend::new("1.0.0.1:80").unwrap();
        let b2 = Backend::new("1.1.1.1:80").unwrap();
        let b3 = Backend::new("1.1.1.2:80").unwrap();
        let discovery = Arc::new(MutableDiscovery::new(BTreeSet::from([b1.clone()])));
        let tracker = Arc::new(BuildTracker::default());
        let group = Arc::new(
            LoadBalancerGroup::<TestSelection>::from_backends_with_configs(
                Backends::new(Box::new(Arc::clone(&discovery))),
                [Some(TestSelectionConfig {
                    reverse: false,
                    tracker: Some(Arc::clone(&tracker)),
                })],
            ),
        );

        group.update().await.unwrap();
        wait_for_group_generation(&group, 1).await;
        tracker.reset();

        // Hold a live iterator over the generation-1 selector. It keeps both the
        // published selector guard and a separate `Arc<S>` clone alive. The
        // tuple drops `iter` (and its `Arc<S>` clone) before the guard, matching
        // the release ordering `select_with` relies on.
        let published = group.selectors[0].selector.load();
        let iter = published.selector.iter(b"");
        let reader = (iter, published);

        // Make the eventual destructor of the generation-1 selector block.
        tracker.block_drop();

        // Generation 2 builds a replacement and retires generation 1.
        discovery.add(b2);
        group.update().await.unwrap();
        wait_for_builds_started(&tracker, 1).await;

        // Generation 3 is queued but cannot build until generation 1 is released.
        discovery.add(b3);
        group.update().await.unwrap();

        // While the reader holds the retired selector, generation 3 must not
        // build.
        tokio::time::sleep(Duration::from_millis(25)).await;
        assert_eq!(tracker.builds.load(Relaxed), 1);

        // Dropping the reader begins destruction of generation 1 on a blocking
        // thread; the destructor is held, so release still must not fire and
        // generation 3 still must not build.
        let drop_reader = tokio::task::spawn_blocking(move || drop(reader));
        tokio::time::sleep(Duration::from_millis(25)).await;
        assert_eq!(
            tracker.builds.load(Relaxed),
            1,
            "next build started before the retired selector was destroyed"
        );

        // Completing destruction releases the gate and lets generation 3 build.
        tracker.unblock_drop();
        drop_reader.await.unwrap();
        wait_for_group_generation(&group, 3).await;
        assert_eq!(tracker.builds.load(Relaxed), 2);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_retired_selector_destruction_does_not_block_runtime() {
        let b1 = Backend::new("1.0.0.1:80").unwrap();
        let b2 = Backend::new("1.1.1.1:80").unwrap();
        let discovery = Arc::new(MutableDiscovery::new(BTreeSet::from([b1])));
        let tracker = Arc::new(BuildTracker::default());
        let group = LoadBalancerGroup::<TestSelection>::from_backends_with_configs(
            Backends::new(Box::new(Arc::clone(&discovery))),
            [Some(TestSelectionConfig {
                reverse: false,
                tracker: Some(Arc::clone(&tracker)),
            })],
        );

        group.update().await.unwrap();
        wait_for_group_generation(&group, 1).await;
        tracker.block_drop();

        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let release_tracker = Arc::clone(&tracker);
        let release_thread = std::thread::spawn(move || {
            let _ = release_rx.recv_timeout(Duration::from_secs(1));
            release_tracker.unblock_drop();
        });

        let start = Instant::now();
        discovery.add(b2);
        group.update().await.unwrap();
        wait_for_group_generation(&group, 2).await;
        let publish_duration = start.elapsed();

        let _ = release_tx.send(());
        release_thread.join().unwrap();
        assert!(
            publish_duration < Duration::from_millis(500),
            "selector publication waited for retired selector destruction: {publish_duration:?}"
        );
    }

    // A private registry keys health by full backend identity, so two backends
    // sharing an address but differing in weight retain independent health.
    #[tokio::test]
    async fn test_private_registry_preserves_backend_identity() {
        let healthy = Backend::new_with_weight("1.0.0.1:80", 1).unwrap();
        let unhealthy = Backend::new_with_weight("1.0.0.1:80", 2).unwrap();
        let discovery = Arc::new(MutableDiscovery::new(BTreeSet::from([
            healthy.clone(),
            unhealthy.clone(),
        ])));
        let checks = Arc::new(AtomicUsize::new(0));
        let mut backends = Backends::new(Box::new(discovery));
        backends.set_health_check(Box::new(WeightHealthCheck {
            checks: Arc::clone(&checks),
            unhealthy_weight: 2,
        }));

        backends.update(|_| {}).await.unwrap();
        backends.run_health_check(false).await;

        // Each identity is probed independently.
        assert_eq!(checks.load(Relaxed), 2);
        assert!(backends.ready(&healthy));
        assert!(!backends.ready(&unhealthy));
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
                    m.insert(b.hash_key(), true);
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
                backend_count = backends.get_backend().len();
            }
            assert_eq!(backend_count, expected);
            assert!(lb.select_with(b"test", 1, |_, _| true).is_some());
        }
    }
}
