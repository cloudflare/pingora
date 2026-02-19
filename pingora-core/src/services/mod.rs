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

//! The service interface
//!
//! A service to the pingora server is just something runs forever until the server is shutting
//! down.
//!
//! Two types of services are particularly useful
//! - services that are listening to some (TCP) endpoints
//! - services that are just running in the background.

use async_trait::async_trait;
use daggy::Walker;
use daggy::{petgraph::visit::Topo, Dag, NodeIndex};
use log::{error, info, warn};
use parking_lot::Mutex;
use std::borrow::Borrow;
use std::sync::Arc;
use std::sync::Weak;
use std::time::Duration;
use tokio::sync::watch;

#[cfg(unix)]
use crate::server::ListenFds;
use crate::server::ShutdownWatch;

pub mod background;
pub mod listening;

/// A notification channel for signaling when a service has become ready.
///
/// Services can use this to notify other services that may depend on them
/// that they have successfully started and are ready to serve requests.
///
/// # Example
///
/// ```rust,ignore
/// use pingora_core::services::ServiceReadyNotifier;
///
/// async fn my_service(ready_notifier: ServiceReadyNotifier) {
///     // Perform initialization...
///
///     // Signal that the service is ready
///     ready_notifier.notify_ready();
///
///     // Continue with main service loop...
/// }
/// ```
pub struct ServiceReadyNotifier {
    sender: watch::Sender<bool>,
}

impl Drop for ServiceReadyNotifier {
    /// In the event that the notifier is dropped before notifying that the
    /// service is ready, we opt to signal ready anyway
    fn drop(&mut self) {
        // Ignore errors - if there are no receivers, that's fine
        let _ = self.sender.send(true);
    }
}

impl ServiceReadyNotifier {
    /// Creates a new ServiceReadyNotifier from a watch sender.
    /// You will not need to create one of these for normal usage, but being
    /// able to is useful for testing.
    pub fn new(sender: watch::Sender<bool>) -> Self {
        Self { sender }
    }

    /// Notifies dependent services that this service is ready.
    ///
    /// Consumes the notifier to ensure ready is only signaled once.
    pub fn notify_ready(self) {
        // Dropping the notifier will signal that the service is ready
        drop(self);
    }
}

/// A receiver for watching when a service becomes ready.
pub type ServiceReadyWatch = watch::Receiver<bool>;

/// A handle to a service in the server.
///
/// This is returned by [`crate::server::Server::add_service()`] and provides
/// methods to declare that other services depend on this one.
///
/// # Example
///
/// ```rust,ignore
/// let db_handle = server.add_service(database_service);
/// let cache_handle = server.add_service(cache_service);
///
/// let api_handle = server.add_service(api_service);
/// api_handle.add_dependency(&db_handle);
/// api_handle.add_dependency(&cache_handle);
/// ```
#[derive(Debug, Clone)]
pub struct ServiceHandle {
    pub(crate) id: NodeIndex,
    name: String,
    ready_watch: ServiceReadyWatch,
    dependencies: Weak<Mutex<DependencyGraph>>,
}

/// Internal representation of a dependency relationship.
#[derive(Debug, Clone)]
pub(crate) struct ServiceDependency {
    pub name: String,
    pub ready_watch: ServiceReadyWatch,
}

impl ServiceHandle {
    /// Creates a new ServiceHandle with the given ID, name, and readiness watcher.
    pub(crate) fn new(
        id: NodeIndex,
        name: String,
        ready_watch: ServiceReadyWatch,
        dependencies: &Arc<Mutex<DependencyGraph>>,
    ) -> Self {
        Self {
            id,
            name,
            ready_watch,
            dependencies: Arc::downgrade(dependencies),
        }
    }

    #[cfg(test)]
    fn get_dependencies(&self) -> Vec<ServiceDependency> {
        let Some(deps_lock) = self.dependencies.upgrade() else {
            return Vec::new();
        };

        let deps = deps_lock.lock();
        deps.get_dependencies(self.id)
    }

    /// Returns the name of the service.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns a clone of the readiness watcher for this service.
    #[allow(dead_code)]
    pub(crate) fn ready_watch(&self) -> ServiceReadyWatch {
        self.ready_watch.clone()
    }

    /// Declares that this service depends on another service.
    ///
    /// This service will not start until the specified dependency has started
    /// and signaled readiness.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let db_id = server.add_service(database_service);
    /// let api_id = server.add_service(api_service);
    ///
    /// // API service depends on database
    /// api_id.add_dependency(&db_id);
    /// ```
    pub fn add_dependency(&self, dependency: impl Borrow<ServiceHandle>) {
        let Some(deps_lock) = self.dependencies.upgrade() else {
            warn!("Attempted to add a dependency after the dependency tree was dropped");
            return;
        };

        let mut deps = deps_lock.lock();
        if let Err(e) = deps.add_dependency(self.id, dependency.borrow().id) {
            error!("Error creating dependency edge: {e}");
        }
    }

    /// Declares that this service depends on the given other services.
    ///
    /// This service will not start until the specified dependencies have
    /// started and signaled readiness.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let db_id = server.add_service(database_service);
    /// let cache_id = server.add_service(cache_service);
    /// let api_id = server.add_service(api_service);
    ///
    /// // API service depends on database
    /// api_id.add_dependencies(&[&db_id, &cache_id]);
    /// ```
    pub fn add_dependencies<'a, D>(&self, dependencies: impl IntoIterator<Item = D>)
    where
        D: Borrow<ServiceHandle> + 'a,
    {
        for dependency in dependencies {
            self.add_dependency(dependency);
        }
    }
}

/// Helper for validating service dependency graphs using daggy.
pub(crate) struct DependencyGraph {
    /// The directed acyclic graph structure from daggy.
    dag: Dag<ServiceDependency, ()>,
}

impl DependencyGraph {
    /// Creates a new dependency graph.
    pub(crate) fn new() -> Self {
        Self { dag: Dag::new() }
    }

    /// Adds a service node to the graph.
    ///
    /// This should be called for all services first, before adding edges.
    pub(crate) fn add_node(&mut self, name: String, ready_watch: ServiceReadyWatch) -> NodeIndex {
        self.dag.add_node(ServiceDependency { name, ready_watch })
    }
    /// Adds a dependency edge from one service to another.
    ///
    /// Returns an error if adding this dependency would create a cycle or reference
    /// a non-existent service.
    pub(crate) fn add_dependency(
        &mut self,
        dependent_service_node_idx: NodeIndex,
        dependency_service_node_idx: NodeIndex,
    ) -> Result<(), String> {
        // Try to add edge (from dependency to dependent)
        // daggy will return an error if this would create a cycle
        if let Err(cycle) =
            self.dag
                .add_edge(dependency_service_node_idx, dependent_service_node_idx, ())
        {
            return Err(format!(
                "Circular service dependency detected between {} and {} creating cycle: {cycle}",
                self.dag[dependency_service_node_idx].name,
                self.dag[dependent_service_node_idx].name
            ));
        }

        Ok(())
    }

    /// Returns services in topological order (dependencies before dependents).
    ///
    /// This ordering ensures that services are started in the correct order.
    /// Returns service IDs in the correct startup order.
    pub(crate) fn topological_sort(&self) -> Result<Vec<(NodeIndex, ServiceDependency)>, String> {
        // Use daggy's built-in topological walker
        let mut sorted = Vec::new();
        let mut topo = Topo::new(&self.dag);

        while let Some(service_id) = topo.next(&self.dag) {
            sorted.push((service_id, self.dag[service_id].clone()));
        }

        Ok(sorted)
    }

    pub(crate) fn get_dependencies(&self, service_id: NodeIndex) -> Vec<ServiceDependency> {
        self.dag
            .parents(service_id)
            .iter(&self.dag)
            .map(|(_, n)| self.dag[n].clone())
            .collect()
    }
}

impl Default for DependencyGraph {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
pub trait ServiceWithDependents: Send + Sync {
    /// This function will be called when the server is ready to start the service.
    ///
    /// Override this method if you need to control exactly when the service signals readiness
    /// (e.g., after async initialization is complete).
    ///
    /// # Arguments
    ///
    /// - `fds` (Unix only): a collection of listening file descriptors. During zero downtime restart
    ///   the `fds` would contain the listening sockets passed from the old service, services should
    ///   take the sockets they need to use then. If the sockets the service looks for don't appear in
    ///   the collection, the service should create its own listening sockets and then put them into
    ///   the collection in order for them to be passed to the next server.
    /// - `shutdown`: the shutdown signal this server would receive.
    /// - `listeners_per_fd`: number of listener tasks to spawn per file descriptor.
    /// - `ready_notifier`: notifier to signal when the service is ready. Services with
    ///   dependents should call `ready_notifier.notify_ready()` once they are fully initialized.
    async fn start_service(
        &mut self,
        #[cfg(unix)] fds: Option<ListenFds>,
        shutdown: ShutdownWatch,
        listeners_per_fd: usize,
        ready_notifier: ServiceReadyNotifier,
    );

    /// The name of the service, just for logging and naming the threads assigned to this service
    ///
    /// Note that due to the limit of the underlying system, only the first 16 chars will be used
    fn name(&self) -> &str;

    /// The preferred number of threads to run this service
    ///
    /// If `None`, the global setting will be used
    fn threads(&self) -> Option<usize> {
        None
    }

    /// This is currently called to inform the service about the delay it
    /// experienced from between waiting on its dependencies. Default behavior
    /// is to log the time.
    ///
    /// TODO. It would be nice if this function was called intermittently by
    /// the server while the service was waiting to give live updates while the
    /// service was waiting and allow the service to decide whether to keep
    /// waiting, continue anyway, or exit
    fn on_startup_delay(&self, time_waited: Duration) {
        info!(
            "Service {} spent {}ms waiting on dependencies",
            self.name(),
            time_waited.as_millis()
        );
    }
}

#[async_trait]
impl<S> ServiceWithDependents for S
where
    S: Service,
{
    async fn start_service(
        &mut self,
        #[cfg(unix)] fds: Option<ListenFds>,
        shutdown: ShutdownWatch,
        listeners_per_fd: usize,
        ready_notifier: ServiceReadyNotifier,
    ) {
        // Signal ready immediately
        ready_notifier.notify_ready();

        #[cfg(unix)]
        {
            S::start_service(self, fds, shutdown, listeners_per_fd).await
        }

        #[cfg(not(unix))]
        {
            S::start_service(self, shutdown, listeners_per_fd).await
        }
    }

    fn name(&self) -> &str {
        S::name(self)
    }

    fn threads(&self) -> Option<usize> {
        S::threads(self)
    }

    fn on_startup_delay(&self, time_waited: Duration) {
        S::on_startup_delay(self, time_waited)
    }
}

/// The service interface
#[async_trait]
pub trait Service: Sync + Send {
    /// Start the service without readiness notification.
    ///
    /// This is a simpler version of [`Self::start_service()`] for services that don't need
    /// to control when they signal readiness. The default implementation does nothing.
    ///
    /// Most services should override this method instead of [`Self::start_service()`].
    ///
    /// # Arguments
    ///
    /// - `fds` (Unix only): a collection of listening file descriptors.
    /// - `shutdown`: the shutdown signal this server would receive.
    /// - `listeners_per_fd`: number of listener tasks to spawn per file descriptor.
    async fn start_service(
        &mut self,
        #[cfg(unix)] _fds: Option<ListenFds>,
        _shutdown: ShutdownWatch,
        _listeners_per_fd: usize,
    ) {
        // Default: do nothing
    }

    /// The name of the service, just for logging and naming the threads assigned to this service
    ///
    /// Note that due to the limit of the underlying system, only the first 16 chars will be used
    fn name(&self) -> &str;

    /// The preferred number of threads to run this service
    ///
    /// If `None`, the global setting will be used
    fn threads(&self) -> Option<usize> {
        None
    }

    /// This is currently called to inform the service about the delay it
    /// experienced from between waiting on its dependencies. Default behavior
    /// is to log the time.
    ///
    /// TODO. It would be nice if this function was called intermittently by
    /// the server while the service was waiting to give live updates while the
    /// service was waiting and allow the service to decide whether to keep
    /// waiting, continue anyway, or exit
    fn on_startup_delay(&self, time_waited: Duration) {
        info!(
            "Service {} spent {}ms waiting on dependencies",
            self.name(),
            time_waited.as_millis()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_service_handle_creation() {
        let deps: Arc<Mutex<DependencyGraph>> = Arc::new(Mutex::new(DependencyGraph::new()));
        let (tx, rx) = watch::channel(false);
        let service_id = ServiceHandle::new(0.into(), "test_service".to_string(), rx, &deps);

        assert_eq!(service_id.id, 0.into());
        assert_eq!(service_id.name(), "test_service");

        // Should be able to clone the watch
        let watch_clone = service_id.ready_watch();
        assert!(!*watch_clone.borrow());

        // Signaling ready should be observable through cloned watch
        tx.send(true).ok();
        assert!(*watch_clone.borrow());
    }

    #[test]
    fn test_service_handle_add_dependency() {
        let graph: Arc<Mutex<DependencyGraph>> = Arc::new(Mutex::new(DependencyGraph::new()));
        let (tx1, rx1) = watch::channel(false);
        let (tx1_clone, rx1_clone) = (tx1.clone(), rx1.clone());
        let (_tx2, rx2) = watch::channel(false);
        let (_tx2_clone, rx2_clone) = (_tx2.clone(), rx2.clone());

        // Add nodes to the graph first
        let dep_node = {
            let mut g = graph.lock();
            g.add_node("dependency".to_string(), rx1)
        };
        let main_node = {
            let mut g = graph.lock();
            g.add_node("main".to_string(), rx2)
        };

        let dep_service = ServiceHandle::new(dep_node, "dependency".to_string(), rx1_clone, &graph);
        let main_service = ServiceHandle::new(main_node, "main".to_string(), rx2_clone, &graph);

        // Add dependency
        main_service.add_dependency(&dep_service);

        // Get dependencies and verify
        let deps = main_service.get_dependencies();
        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0].name, "dependency");

        // Verify watch is working
        assert!(!*deps[0].ready_watch.borrow());
        tx1_clone.send(true).ok();
        assert!(*deps[0].ready_watch.borrow());
    }

    #[test]
    fn test_service_handle_multiple_dependencies() {
        let graph: Arc<Mutex<DependencyGraph>> = Arc::new(Mutex::new(DependencyGraph::new()));
        let (_tx1, rx1) = watch::channel(false);
        let rx1_clone = rx1.clone();
        let (_tx2, rx2) = watch::channel(false);
        let rx2_clone = rx2.clone();
        let (_tx3, rx3) = watch::channel(false);
        let rx3_clone = rx3.clone();

        // Add nodes to the graph first
        let dep1_node = {
            let mut g = graph.lock();
            g.add_node("dep1".to_string(), rx1)
        };
        let dep2_node = {
            let mut g = graph.lock();
            g.add_node("dep2".to_string(), rx2)
        };
        let main_node = {
            let mut g = graph.lock();
            g.add_node("main".to_string(), rx3)
        };

        let dep1 = ServiceHandle::new(dep1_node, "dep1".to_string(), rx1_clone, &graph);
        let dep2 = ServiceHandle::new(dep2_node, "dep2".to_string(), rx2_clone, &graph);
        let main_service = ServiceHandle::new(main_node, "main".to_string(), rx3_clone, &graph);

        // Add multiple dependencies
        main_service.add_dependency(&dep1);
        main_service.add_dependency(&dep2);

        // Get dependencies and verify
        let deps = main_service.get_dependencies();
        assert_eq!(deps.len(), 2);

        let dep_names: Vec<&str> = deps.iter().map(|d| d.name.as_str()).collect();
        assert!(dep_names.contains(&"dep1"));
        assert!(dep_names.contains(&"dep2"));
    }

    #[test]
    fn test_single_service_no_dependencies() {
        let mut graph = DependencyGraph::new();
        let (_tx, rx) = watch::channel(false);
        let _node = graph.add_node("service1".to_string(), rx);

        let order = graph.topological_sort().unwrap();
        assert_eq!(order.len(), 1);
        assert_eq!(order[0].1.name, "service1");
    }

    #[test]
    fn test_simple_dependency_chain() {
        let mut graph = DependencyGraph::new();
        let (_tx1, rx1) = watch::channel(false);
        let (_tx2, rx2) = watch::channel(false);
        let (_tx3, rx3) = watch::channel(false);

        let node1 = graph.add_node("service1".to_string(), rx1);
        let node2 = graph.add_node("service2".to_string(), rx2);
        let node3 = graph.add_node("service3".to_string(), rx3);

        // service2 depends on service1, service3 depends on service2
        graph.add_dependency(node2, node1).unwrap();
        graph.add_dependency(node3, node2).unwrap();

        let order = graph.topological_sort().unwrap();
        assert_eq!(order.len(), 3);
        // Verify order: service1, service2, service3
        assert_eq!(order[0].1.name, "service1");
        assert_eq!(order[1].1.name, "service2");
        assert_eq!(order[2].1.name, "service3");
    }

    #[test]
    fn test_diamond_dependency() {
        let mut graph = DependencyGraph::new();
        let (_tx1, rx1) = watch::channel(false);
        let (_tx2, rx2) = watch::channel(false);
        let (_tx3, rx3) = watch::channel(false);

        let db = graph.add_node("db".to_string(), rx1);
        let cache = graph.add_node("cache".to_string(), rx2);
        let api = graph.add_node("api".to_string(), rx3);

        // api depends on both db and cache
        graph.add_dependency(api, db).unwrap();
        graph.add_dependency(api, cache).unwrap();

        let order = graph.topological_sort().unwrap();
        // api should come last, but db and cache order doesn't matter
        assert_eq!(order.len(), 3);
        assert_eq!(order[2].1.name, "api");
        let first_two: Vec<&str> = order[0..2].iter().map(|(_, d)| d.name.as_str()).collect();
        assert!(first_two.contains(&"db"));
        assert!(first_two.contains(&"cache"));
    }

    #[test]
    #[should_panic(expected = "node indices out of bounds")]
    fn test_missing_dependency() {
        let mut graph = DependencyGraph::new();
        let (_tx1, rx1) = watch::channel(false);

        let node1 = graph.add_node("service1".to_string(), rx1);
        let nonexistent = NodeIndex::new(999);

        // Try to add dependency on non-existent node - this should panic
        let _ = graph.add_dependency(node1, nonexistent);
    }

    #[test]
    fn test_circular_dependency_self() {
        let mut graph = DependencyGraph::new();
        let (_tx1, rx1) = watch::channel(false);

        let node1 = graph.add_node("service1".to_string(), rx1);

        // Try to make service depend on itself
        let result = graph.add_dependency(node1, node1);

        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Circular"));
    }

    #[test]
    fn test_circular_dependency_two_services() {
        let mut graph = DependencyGraph::new();
        let (_tx1, rx1) = watch::channel(false);
        let (_tx2, rx2) = watch::channel(false);

        // Add both nodes first
        let node1 = graph.add_node("service1".to_string(), rx1);
        let node2 = graph.add_node("service2".to_string(), rx2);

        // Try to add circular dependencies
        graph.add_dependency(node1, node2).unwrap();
        let result = graph.add_dependency(node2, node1);

        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Circular"));
    }

    #[test]
    fn test_circular_dependency_three_services() {
        let mut graph = DependencyGraph::new();
        let (_tx1, rx1) = watch::channel(false);
        let (_tx2, rx2) = watch::channel(false);
        let (_tx3, rx3) = watch::channel(false);

        // Add all nodes first
        let node1 = graph.add_node("service1".to_string(), rx1);
        let node2 = graph.add_node("service2".to_string(), rx2);
        let node3 = graph.add_node("service3".to_string(), rx3);

        // Add dependencies that would form a cycle
        graph.add_dependency(node1, node2).unwrap();
        graph.add_dependency(node2, node3).unwrap();
        let result = graph.add_dependency(node3, node1);

        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Circular"));
    }

    #[test]
    fn test_complex_valid_graph() {
        let mut graph = DependencyGraph::new();
        let (_tx1, rx1) = watch::channel(false);
        let (_tx2, rx2) = watch::channel(false);
        let (_tx3, rx3) = watch::channel(false);
        let (_tx4, rx4) = watch::channel(false);
        let (_tx5, rx5) = watch::channel(false);

        // Build a complex dependency graph:
        //   db, cache - no deps
        //   auth -> db
        //   api -> db, cache, auth
        //   frontend -> api
        let db = graph.add_node("db".to_string(), rx1);
        let cache = graph.add_node("cache".to_string(), rx2);
        let auth = graph.add_node("auth".to_string(), rx3);
        let api = graph.add_node("api".to_string(), rx4);
        let frontend = graph.add_node("frontend".to_string(), rx5);

        graph.add_dependency(auth, db).unwrap();
        graph.add_dependency(api, db).unwrap();
        graph.add_dependency(api, cache).unwrap();
        graph.add_dependency(api, auth).unwrap();
        graph.add_dependency(frontend, api).unwrap();

        let order = graph.topological_sort().unwrap();

        // Verify ordering constraints using names
        let db_pos = order.iter().position(|(_, d)| d.name == "db").unwrap();
        let cache_pos = order.iter().position(|(_, d)| d.name == "cache").unwrap();
        let auth_pos = order.iter().position(|(_, d)| d.name == "auth").unwrap();
        let api_pos = order.iter().position(|(_, d)| d.name == "api").unwrap();
        let frontend_pos = order
            .iter()
            .position(|(_, d)| d.name == "frontend")
            .unwrap();

        assert!(db_pos < auth_pos);
        assert!(auth_pos < api_pos);
        assert!(db_pos < api_pos);
        assert!(cache_pos < api_pos);
        assert!(api_pos < frontend_pos);
    }
}
