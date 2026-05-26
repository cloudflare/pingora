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

//! Server process and configuration management

mod bootstrap_services;
pub mod configuration;
#[cfg(unix)]
mod daemon;
#[cfg(unix)]
pub(crate) mod transfer_fd;

use async_trait::async_trait;
#[cfg(unix)]
use daemon::daemonize;
use daggy::NodeIndex;
use log::{debug, error, info, warn};
use parking_lot::Mutex;
use pingora_runtime::{BlockingPoolOpts, Runtime, RuntimeBuilder};
use pingora_timeout::fast_timeout;
#[cfg(feature = "sentry")]
use sentry::ClientOptions;
use std::sync::Arc;
use std::thread;
use std::time::SystemTime;
#[cfg(unix)]
use tokio::signal::unix;
use tokio::sync::{broadcast, watch};
use tokio::time::{sleep, Duration};

use crate::prelude::background_service;
use crate::server::bootstrap_services::{Bootstrap, BootstrapService};
use crate::services::{
    DependencyGraph, ServiceHandle, ServiceReadyNotifier, ServiceReadyWatch, ServiceWithDependents,
};
use configuration::{Opt, ServerConf};
use std::collections::HashMap;
#[cfg(unix)]
pub use transfer_fd::Fds;

use pingora_error::{Error, ErrorType, Result};

/* Time to wait before exiting the program.
This is the graceful period for all existing sessions to finish */
const EXIT_TIMEOUT: u64 = 60 * 5;
/* Time to wait before shutting down listening sockets.
This is the graceful period for the new service to get ready */
const CLOSE_TIMEOUT: u64 = 5;

enum ShutdownType {
    Graceful,
    Quick,
}

/// Internal wrapper for services with dependency metadata.
pub(crate) struct ServiceWrapper {
    ready_notifier: Option<ServiceReadyNotifier>,
    service: Box<dyn ServiceWithDependents>,
    service_handle: ServiceHandle,
}

/// The execution phase the server is currently in.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum ExecutionPhase {
    /// The server was created, but has not started yet.
    Setup,

    /// Services are being prepared.
    ///
    /// During graceful upgrades this phase acquires the listening FDs from the old process.
    Bootstrap,

    /// Bootstrap has finished, listening FDs have been transferred.
    BootstrapComplete,

    /// The server is running and is listening for shutdown signals.
    Running(tokio::runtime::Handle),

    /// A QUIT signal was received, indicating that a new process wants to take over.
    ///
    /// The server is trying to send the fds to the new process over a Unix socket.
    GracefulUpgradeTransferringFds(tokio::runtime::Handle),

    /// FDs have been sent to the new process.
    /// Waiting a fixed amount of time to allow the new process to take the sockets.
    GracefulUpgradeCloseTimeout(tokio::runtime::Handle),

    /// A TERM signal was received, indicating that the server should shut down gracefully.
    GracefulTerminate(tokio::runtime::Handle),

    /// The server is shutting down.
    ShutdownStarted,

    /// Waiting for the configured grace period to end before shutting down.
    ShutdownGracePeriod,

    /// Wait for runtimes to finish.
    ShutdownRuntimes,

    /// The server has stopped.
    Terminated,
}

/// The receiver for server's shutdown event. The value will turn to true once the server starts
/// to shutdown
pub type ShutdownWatch = watch::Receiver<bool>;
#[cfg(unix)]
pub type ListenFds = Arc<Mutex<Fds>>;

/// The type of shutdown process that has been requested.
#[derive(Debug)]
pub enum ShutdownSignal {
    /// Send file descriptors to the new process before starting runtime shutdown with
    /// [ServerConf::graceful_shutdown_timeout_seconds] timeout.
    GracefulUpgrade,
    /// Wait for [ServerConf::grace_period_seconds] before starting runtime shutdown with
    /// [ServerConf::graceful_shutdown_timeout_seconds] timeout.
    GracefulTerminate,
    /// Shutdown with no timeout for runtime shutdown.
    FastShutdown,
}

/// Watcher of a shutdown signal, e.g., [UnixShutdownSignalWatch] for Unix-like
/// platforms.
#[async_trait]
pub trait ShutdownSignalWatch {
    /// Returns the desired shutdown type once one has been requested.
    async fn recv(&self) -> ShutdownSignal;
}

/// A Unix shutdown watcher that awaits for Unix signals.
///
/// - `SIGQUIT`: graceful upgrade
/// - `SIGTERM`: graceful terminate
/// - `SIGINT`: fast shutdown
#[cfg(unix)]
pub struct UnixShutdownSignalWatch;

#[cfg(unix)]
#[async_trait]
impl ShutdownSignalWatch for UnixShutdownSignalWatch {
    async fn recv(&self) -> ShutdownSignal {
        let mut graceful_upgrade_signal = unix::signal(unix::SignalKind::quit()).unwrap();
        let mut graceful_terminate_signal = unix::signal(unix::SignalKind::terminate()).unwrap();
        let mut fast_shutdown_signal = unix::signal(unix::SignalKind::interrupt()).unwrap();

        tokio::select! {
            _ = graceful_upgrade_signal.recv() => {
                ShutdownSignal::GracefulUpgrade
            },
            _ = graceful_terminate_signal.recv() => {
                ShutdownSignal::GracefulTerminate
            },
            _ = fast_shutdown_signal.recv() => {
                ShutdownSignal::FastShutdown
            },
        }
    }
}

/// Arguments to configure running of the pingora server.
pub struct RunArgs {
    /// Signal for initating shutdown
    #[cfg(unix)]
    pub shutdown_signal: Box<dyn ShutdownSignalWatch>,
}

impl Default for RunArgs {
    #[cfg(unix)]
    fn default() -> Self {
        Self {
            shutdown_signal: Box::new(UnixShutdownSignalWatch),
        }
    }

    #[cfg(windows)]
    fn default() -> Self {
        Self {}
    }
}

/// The server object
///
/// This object represents an entire pingora server process which may have multiple independent
/// services (see [crate::services]). The server object handles signals, reading configuration,
/// zero downtime upgrade and error reporting.
pub struct Server {
    services: HashMap<NodeIndex, ServiceWrapper>,
    shutdown_watch: watch::Sender<bool>,
    // TODO: we many want to drop this copy to let sender call closed()
    shutdown_recv: ShutdownWatch,

    /// Tracks the execution phase of the server during upgrades and graceful shutdowns.
    ///
    /// Users can subscribe to the phase with [`Self::watch_execution_phase()`].
    execution_phase_watch: broadcast::Sender<ExecutionPhase>,

    /// Specification of service level dependencies
    dependencies: Arc<Mutex<DependencyGraph>>,

    /// Service initialization
    bootstrap: Arc<Mutex<Bootstrap>>,

    /// The parsed server configuration
    pub configuration: Arc<ServerConf>,
    /// The parser command line options
    pub options: Option<Opt>,
}

// TODO: delete the pid when exit

impl Server {
    /// Acquire a receiver for the server's execution phase.
    ///
    /// The receiver will produce values for each transition.
    pub fn watch_execution_phase(&self) -> broadcast::Receiver<ExecutionPhase> {
        self.execution_phase_watch.subscribe()
    }

    #[cfg(unix)]
    async fn main_loop(&self, run_args: RunArgs) -> ShutdownType {
        // waiting for exit signal

        self.execution_phase_watch
            .send(ExecutionPhase::Running(tokio::runtime::Handle::current()))
            .ok();

        match run_args.shutdown_signal.recv().await {
            ShutdownSignal::FastShutdown => {
                info!("SIGINT received, exiting");
                ShutdownType::Quick
            }
            ShutdownSignal::GracefulTerminate => {
                // we receive a graceful terminate, all instances are instructed to stop
                info!("SIGTERM received, gracefully exiting");
                // graceful shutdown if there are listening sockets
                info!("Broadcasting graceful shutdown");
                match self.shutdown_watch.send(true) {
                    Ok(_) => {
                        info!("Graceful shutdown started!");
                    }
                    Err(e) => {
                        error!("Graceful shutdown broadcast failed: {e}");
                    }
                }
                info!("Broadcast graceful shutdown complete");

                self.execution_phase_watch
                    .send(ExecutionPhase::GracefulTerminate(
                        tokio::runtime::Handle::current(),
                    ))
                    .ok();

                ShutdownType::Graceful
            }
            ShutdownSignal::GracefulUpgrade => {
                // TODO: still need to select! on signals in case a fast shutdown is needed
                // aka: move below to another task and only kick it off here
                info!("SIGQUIT received, sending socks and gracefully exiting");

                self.execution_phase_watch
                    .send(ExecutionPhase::GracefulUpgradeTransferringFds(
                        tokio::runtime::Handle::current(),
                    ))
                    .ok();

                let sent_fds = {
                    let fds = self.listen_fds();
                    let fds = fds.lock();
                    if fds.is_empty() {
                        info!("No socks to send, shutting down.");
                        false
                    } else {
                        info!("Trying to send socks");
                        match fds.send_to_sock(self.configuration.as_ref().upgrade_sock.as_str()) {
                            Ok(_) => {
                                info!("listener sockets sent");
                            }
                            Err(e) => {
                                error!("Unable to send listener sockets to new process: {e}");
                                #[cfg(all(not(debug_assertions), feature = "sentry"))]
                                sentry::capture_error(&e);
                            }
                        }
                        true
                    }
                };
                if sent_fds {
                    self.execution_phase_watch
                        .send(ExecutionPhase::GracefulUpgradeCloseTimeout(
                            tokio::runtime::Handle::current(),
                        ))
                        .ok();
                    sleep(Duration::from_secs(CLOSE_TIMEOUT)).await;
                }
                info!("Broadcasting graceful shutdown");
                // gracefully exiting
                match self.shutdown_watch.send(true) {
                    Ok(_) => {
                        info!("Graceful shutdown started!");
                    }
                    Err(e) => {
                        error!("Graceful shutdown broadcast failed: {e}");
                        // switch to fast shutdown
                        return ShutdownType::Graceful;
                    }
                }
                info!("Broadcast graceful shutdown complete");
                ShutdownType::Graceful
            }
        }
    }

    #[cfg(windows)]
    async fn main_loop(&self, _run_args: RunArgs) -> ShutdownType {
        // waiting for exit signal

        self.execution_phase_watch
            .send(ExecutionPhase::Running)
            .ok();

        match tokio::signal::ctrl_c().await {
            Ok(()) => {
                info!("Ctrl+C received, gracefully exiting");
                // graceful shutdown if there are listening sockets
                info!("Broadcasting graceful shutdown");
                match self.shutdown_watch.send(true) {
                    Ok(_) => {
                        info!("Graceful shutdown started!");
                    }
                    Err(e) => {
                        error!("Graceful shutdown broadcast failed: {e}");
                    }
                }
                info!("Broadcast graceful shutdown complete");

                self.execution_phase_watch
                    .send(ExecutionPhase::GracefulTerminate)
                    .ok();

                ShutdownType::Graceful
            }
            Err(e) => {
                error!("Unable to listen for shutdown signal: {}", e);
                ShutdownType::Quick
            }
        }
    }

    #[cfg(feature = "sentry")]
    #[cfg_attr(docsrs, doc(cfg(feature = "sentry")))]
    /// The Sentry ClientOptions.
    ///
    /// Panics and other events sentry captures will be sent to this DSN **only in release mode**
    pub fn set_sentry_config(&mut self, sentry_config: ClientOptions) {
        self.bootstrap.lock().set_sentry_config(Some(sentry_config));
    }

    /// Get the configured file descriptors for listening
    #[cfg(unix)]
    fn listen_fds(&self) -> ListenFds {
        self.bootstrap.lock().get_fds()
    }

    #[allow(clippy::too_many_arguments)]
    fn run_service(
        mut service: Box<dyn ServiceWithDependents>,
        #[cfg(unix)] fds: ListenFds,
        shutdown: ShutdownWatch,
        threads: usize,
        work_stealing: bool,
        listeners_per_fd: usize,
        ready_notifier: ServiceReadyNotifier,
        dependency_watches: Vec<ServiceReadyWatch>,
        blocking_opts: BlockingPoolOpts,
    ) -> Runtime
// NOTE: we need to keep the runtime outside async since
        // otherwise the runtime will be dropped.
    {
        let service_runtime =
            Server::create_runtime(service.name(), threads, work_stealing, blocking_opts);
        let service_name = service.name().to_string();
        service_runtime.get_handle().spawn(async move {
            // Wait for all dependencies to be ready
            let mut time_waited_opt: Option<Duration> = None;
            for mut watch in dependency_watches {
                let start = SystemTime::now();

                if watch.wait_for(|&ready| ready).await.is_err() {
                    error!(
                        "Service '{}' dependency channel closed before ready",
                        service_name
                    );
                }

                *time_waited_opt.get_or_insert_default() += start.elapsed().unwrap_or_default()
            }

            if let Some(time_waited) = time_waited_opt {
                service.on_startup_delay(time_waited);
            }

            // Start the actual service, passing the ready notifier
            service
                .start_service(
                    #[cfg(unix)]
                    Some(fds),
                    shutdown,
                    listeners_per_fd,
                    ready_notifier,
                )
                .await;
            info!("service '{}' exited.", service_name);
        });
        service_runtime
    }

    /// Create a new [`Server`], using the [`Opt`] and [`ServerConf`] values provided
    ///
    /// This method is intended for pingora frontends that are NOT using the built-in
    /// command line and configuration file parsing, and are instead using their own.
    ///
    /// If a configuration file path is provided as part of `opt`, it will be ignored
    /// and a warning will be logged.
    pub fn new_with_opt_and_conf(raw_opt: impl Into<Option<Opt>>, mut conf: ServerConf) -> Server {
        let opt = raw_opt.into();
        if let Some(opts) = &opt {
            if let Some(c) = opts.conf.as_ref() {
                warn!("Ignoring command line argument using '{c}' as configuration, and using provided configuration instead.");
            }
            conf.merge_with_opt(opts);
        }

        let (tx, rx) = watch::channel(false);

        let execution_phase_watch = broadcast::channel(100).0;
        let bootstrap = Arc::new(Mutex::new(Bootstrap::new(
            &opt,
            &conf,
            &execution_phase_watch,
        )));

        Server {
            services: Default::default(),
            shutdown_watch: tx,
            shutdown_recv: rx,
            execution_phase_watch,
            configuration: Arc::new(conf),
            options: opt,
            dependencies: Arc::new(Mutex::new(DependencyGraph::new())),
            bootstrap,
        }
    }

    /// Create a new [`Server`].
    ///
    /// Only one [`Server`] needs to be created for a process. A [`Server`] can hold multiple
    /// independent services.
    ///
    /// Command line options can either be passed by parsing the command line arguments via
    /// `Opt::parse_args()`, or be generated by other means.
    pub fn new(opt: impl Into<Option<Opt>>) -> Result<Server> {
        let opt = opt.into();
        let (tx, rx) = watch::channel(false);

        let execution_phase_watch = broadcast::channel(100).0;
        let conf = if let Some(opt) = opt.as_ref() {
            opt.conf.as_ref().map_or_else(
                || {
                    // options, no conf, generated
                    ServerConf::new_with_opt_override(opt).ok_or_else(|| {
                        Error::explain(ErrorType::ReadError, "Conf generation failed")
                    })
                },
                |_| {
                    // options and conf loaded
                    ServerConf::load_yaml_with_opt_override(opt)
                },
            )
        } else {
            ServerConf::new()
                .ok_or_else(|| Error::explain(ErrorType::ReadError, "Conf generation failed"))
        }?;

        let bootstrap = Arc::new(Mutex::new(Bootstrap::new(
            &opt,
            &conf,
            &execution_phase_watch,
        )));

        Ok(Server {
            services: Default::default(),
            shutdown_watch: tx,
            shutdown_recv: rx,
            execution_phase_watch,
            configuration: Arc::new(conf),
            options: opt,
            dependencies: Arc::new(Mutex::new(DependencyGraph::new())),
            bootstrap,
        })
    }

    /// Add a service to this server.
    ///
    /// Returns a [`ServiceHandle`] that can be used to declare dependencies.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let db_id = server.add_service(database_service);
    /// let api_id = server.add_service(api_service);
    ///
    /// // Declare that API depends on database
    /// api_id.add_dependency(&db_id);
    /// ```
    pub fn add_service(&mut self, service: impl ServiceWithDependents + 'static) -> ServiceHandle {
        self.add_boxed_service(Box::new(service))
    }

    /// Add a pre-boxed service to this server.
    ///
    /// Returns a [`ServiceHandle`] that can be used to declare dependencies.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let db_id = server.add_service(database_service);
    /// let api_id = server.add_service(api_service);
    ///
    /// // Declare that API depends on database
    /// api_id.add_dependency(&db_id);
    /// ```
    pub fn add_boxed_service(
        &mut self,
        service_box: Box<dyn ServiceWithDependents>,
    ) -> ServiceHandle {
        let name = service_box.name().to_string();

        // Create a readiness notifier for this service
        let (tx, rx) = watch::channel(false);

        let id = self.dependencies.lock().add_node(name.clone(), rx.clone());

        let service_handle = ServiceHandle::new(id, name, rx, &self.dependencies);

        let wrapper = ServiceWrapper {
            ready_notifier: Some(ServiceReadyNotifier::new(tx)),
            service: service_box,
            service_handle: service_handle.clone(),
        };

        self.services.insert(id, wrapper);

        service_handle
    }

    /// Similar to [`Self::add_service()`], but take a list of services.
    ///
    /// Returns a `Vec<ServiceHandle>` for all added services.
    pub fn add_services(
        &mut self,
        services: Vec<Box<dyn ServiceWithDependents>>,
    ) -> Vec<ServiceHandle> {
        services
            .into_iter()
            .map(|service| self.add_boxed_service(service))
            .collect()
    }

    /// Prepare the server to start
    ///
    /// When trying to zero downtime upgrade from an older version of the server which is already
    /// running, this function will try to get all its listening sockets in order to take them over.
    pub fn bootstrap(&mut self) {
        self.bootstrap.lock().bootstrap();
    }

    /// Create a service that will run to prepare the service to start
    ///
    /// The created service will handle the zero-downtime upgrade from an older version of the server
    /// to this one. It will try to get all its listening sockets in order to take them over.
    pub fn bootstrap_as_a_service(&mut self) -> ServiceHandle {
        let bootstrap_service =
            background_service("Bootstrap Service", BootstrapService::new(&self.bootstrap));

        self.add_service(bootstrap_service)
    }

    /// Start the server using [Self::run] and default [RunArgs].
    ///
    /// This function will block forever until the server needs to quit. So this would be the last
    /// function to call for this object.
    ///
    /// Note: this function may fork the process for daemonization, so any additional threads created
    /// before this function will be lost to any service logic once this function is called.
    pub fn run_forever(self) -> ! {
        self.run(RunArgs::default());

        std::process::exit(0)
    }

    /// Run the server until execution finished.
    ///
    /// This function will run until the server has been instructed to shut down
    /// through a signal, and will then wait for all services to finish and
    /// runtimes to exit.
    ///
    /// Note: if daemonization is enabled in the config, this function will
    /// never return.
    /// Instead it will either start the daemon process and exit, or panic
    /// if daemonization fails.
    pub fn run(mut self, run_args: RunArgs) {
        info!("Server starting");

        let conf = self.configuration.as_ref();

        #[cfg(unix)]
        if conf.daemon {
            info!("Daemonizing the server");
            fast_timeout::pause_for_fork();
            let daemonize_result = daemonize(&self.configuration);
            fast_timeout::unpause();
            // If daemon_wait_for_ready is enabled, pass the parent PID to bootstrap so it
            // can send SIGUSR1 to the parent after bootstrap completes.
            if let Some(pid) = daemonize_result.notify_parent_pid {
                self.bootstrap.lock().set_notify_parent_pid(pid);
            }
        }

        #[cfg(windows)]
        if conf.daemon {
            panic!("Daemonizing under windows is not supported");
        }

        let blocking_opts = BlockingPoolOpts {
            max_threads: conf.max_blocking_threads,
            thread_keep_alive: conf.blocking_threads_ttl_seconds.map(Duration::from_secs),
        };

        // Initialize (or re-initialize) sentry and persist the guard for
        // the lifetime of the server. When daemonizing, the transport
        // thread spawned by any earlier `sentry::init` during
        // `bootstrap()` is lost after `fork()`, so a fresh init in the
        // child process is required. In non-daemon mode this is the
        // authoritative initialization that keeps sentry active.
        #[cfg(feature = "sentry")]
        self.bootstrap.lock().start_sentry();

        // Holds tuples of runtimes and their service name.
        let mut runtimes: Vec<(Runtime, String)> = Vec::new();

        // Get services in topological order (dependencies first)
        let startup_order = match self.dependencies.lock().topological_sort() {
            Ok(order) => order,
            Err(e) => {
                error!("Failed to determine service startup order: {}", e);
                std::process::exit(1);
            }
        };

        // Log service names in startup order
        let service_names: Vec<String> = startup_order
            .iter()
            .map(|(_, service)| service.name.clone())
            .collect();
        info!("Starting services in dependency order: {:?}", service_names);

        // Start services in dependency order
        for (service_id, service) in startup_order {
            let mut wrapper = match self.services.remove(&service_id) {
                Some(w) => w,
                None => {
                    warn!(
                        "Service ID {:?}-{} in startup order but not found",
                        service_id, service.name
                    );
                    continue;
                }
            };

            let threads = wrapper.service.threads().unwrap_or(conf.threads);
            let name = wrapper.service.name().to_string();

            // Extract dependency watches from the ServiceHandle
            let dependencies = self
                .dependencies
                .lock()
                .get_dependencies(wrapper.service_handle.id);

            // Get the readiness notifier for this service by taking it from the Option.
            // Since service_id is the index, we can directly access it.
            // We take() the notifier, leaving None in its place.
            let ready_notifier = wrapper
                .ready_notifier
                .take()
                .expect("Service notifier should exist");

            if !dependencies.is_empty() {
                info!(
                    "Service '{name}' will wait for dependencies: {:?}",
                    dependencies.iter().map(|s| &s.name).collect::<Vec<_>>()
                );
            } else {
                info!("Starting service: {}", name);
            }

            let dependency_watches = dependencies
                .iter()
                .map(|s| s.ready_watch.clone())
                .collect::<Vec<_>>();

            let runtime = Server::run_service(
                wrapper.service,
                #[cfg(unix)]
                self.listen_fds(),
                self.shutdown_recv.clone(),
                threads,
                conf.work_stealing,
                self.configuration.listener_tasks_per_fd,
                ready_notifier,
                dependency_watches,
                blocking_opts.clone(),
            );
            runtimes.push((runtime, name));
        }

        // blocked on main loop so that it runs forever
        // Only work steal runtime can use block_on()
        let server_runtime = Server::create_runtime("Server", 1, true, BlockingPoolOpts::default());
        #[cfg(unix)]
        let shutdown_type = server_runtime
            .get_handle()
            .block_on(self.main_loop(run_args));
        #[cfg(windows)]
        let shutdown_type = server_runtime
            .get_handle()
            .block_on(self.main_loop(run_args));

        self.execution_phase_watch
            .send(ExecutionPhase::ShutdownStarted)
            .ok();

        if matches!(shutdown_type, ShutdownType::Graceful) {
            self.execution_phase_watch
                .send(ExecutionPhase::ShutdownGracePeriod)
                .ok();

            let exit_timeout = self
                .configuration
                .as_ref()
                .grace_period_seconds
                .unwrap_or(EXIT_TIMEOUT);
            info!("Graceful shutdown: grace period {}s starts", exit_timeout);
            thread::sleep(Duration::from_secs(exit_timeout));
            info!("Graceful shutdown: grace period ends");
        }

        // Give tokio runtimes time to exit
        let shutdown_timeout = match shutdown_type {
            ShutdownType::Quick => Duration::from_secs(0),
            ShutdownType::Graceful => Duration::from_secs(
                self.configuration
                    .as_ref()
                    .graceful_shutdown_timeout_seconds
                    .unwrap_or(5),
            ),
        };

        self.execution_phase_watch
            .send(ExecutionPhase::ShutdownRuntimes)
            .ok();

        let shutdowns: Vec<_> = runtimes
            .into_iter()
            .map(|(rt, name)| {
                info!("Waiting for runtimes to exit!");
                let join = thread::spawn(move || {
                    rt.shutdown_timeout(shutdown_timeout);
                    thread::sleep(shutdown_timeout)
                });
                (join, name)
            })
            .collect();
        for (shutdown, name) in shutdowns {
            info!("Waiting for service runtime {} to exit", name);
            if let Err(e) = shutdown.join() {
                error!("Failed to shutdown service runtime {}: {:?}", name, e);
            }
            debug!("Service runtime {} has exited", name);
        }
        info!("All runtimes exited, exiting now");

        self.execution_phase_watch
            .send(ExecutionPhase::Terminated)
            .ok();
    }

    fn create_runtime(
        name: &str,
        threads: usize,
        work_steal: bool,
        blocking_opts: BlockingPoolOpts,
    ) -> Runtime {
        RuntimeBuilder::new(threads, name)
            .work_steal(work_steal)
            .blocking_pool_opts(blocking_opts)
            .build()
    }
}
