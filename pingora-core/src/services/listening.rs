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

//! The listening service
//!
//! A [Service] (listening service) responds to incoming requests on its endpoints.
//! Each [Service] can be configured with custom application logic (e.g. an `HTTPProxy`) and one or
//! more endpoints to listen to.

use crate::apps::ServerApp;
use crate::listeners::{Listeners, ServerAddress, TcpSocketOptions, TlsSettings, TransportStack};
use crate::protocols::Stream;
#[cfg(unix)]
use crate::server::ListenFds;
use crate::server::ShutdownWatch;
use crate::services::Service as ServiceTrait;

use async_trait::async_trait;
use log::{debug, error, info};
use pingora_error::Result;
use pingora_runtime::current_handle;
use std::fs::Permissions;
use std::sync::Arc;

/// The type of service that is associated with a list of listening endpoints and a particular application
pub struct Service<A> {
    name: String,
    listeners: Listeners,
    app_logic: Option<A>,
    /// The number of preferred threads. `None` to follow global setting.
    pub threads: Option<usize>,
}

impl<A> Service<A> {
    /// Create a new [`Service`] with the given application (see [`crate::apps`]).
    pub fn new(name: String, app_logic: A) -> Self {
        Service {
            name,
            listeners: Listeners::new(),
            app_logic: Some(app_logic),
            threads: None,
        }
    }

    /// Create a new [`Service`] with the given application (see [`crate::apps`]) and the given
    /// [`Listeners`].
    pub fn with_listeners(name: String, listeners: Listeners, app_logic: A) -> Self {
        Service {
            name,
            listeners,
            app_logic: Some(app_logic),
            threads: None,
        }
    }

    /// Get the [`Listeners`], mostly to add more endpoints.
    pub fn endpoints(&mut self) -> &mut Listeners {
        &mut self.listeners
    }

    // the follow add* function has no effect if the server is already started

    /// Add a TCP listening endpoint with the given address (e.g., `127.0.0.1:8000`).
    pub fn add_tcp(&mut self, addr: &str) {
        self.listeners.add_tcp(addr);
    }

    /// Add a TCP listening endpoint with the given [`TcpSocketOptions`].
    pub fn add_tcp_with_settings(&mut self, addr: &str, sock_opt: TcpSocketOptions) {
        self.listeners.add_tcp_with_settings(addr, sock_opt);
    }

    /// Add a Unix domain socket listening endpoint with the given path.
    ///
    /// Optionally take a permission of the socket file. The default is read and write access for
    /// everyone (0o666).
    #[cfg(unix)]
    pub fn add_uds(&mut self, addr: &str, perm: Option<Permissions>) {
        self.listeners.add_uds(addr, perm);
    }

    /// Add a TLS listening endpoint with the given certificate and key paths.
    pub fn add_tls(&mut self, addr: &str, cert_path: &str, key_path: &str) -> Result<()> {
        self.listeners.add_tls(addr, cert_path, key_path)
    }

    /// Add a TLS listening endpoint with the given [`TlsSettings`] and [`TcpSocketOptions`].
    pub fn add_tls_with_settings(
        &mut self,
        addr: &str,
        sock_opt: Option<TcpSocketOptions>,
        settings: TlsSettings,
    ) {
        self.listeners
            .add_tls_with_settings(addr, sock_opt, settings)
    }

    /// Add an endpoint according to the given [`ServerAddress`]
    pub fn add_address(&mut self, addr: ServerAddress) {
        self.listeners.add_address(addr);
    }

    /// Get a reference to the application inside this service
    pub fn app_logic(&self) -> Option<&A> {
        self.app_logic.as_ref()
    }

    /// Get a mutable reference to the application inside this service
    pub fn app_logic_mut(&mut self) -> Option<&mut A> {
        self.app_logic.as_mut()
    }
}

impl<A: ServerApp + Send + Sync + 'static> Service<A> {
    pub async fn handle_event(event: Stream, app_logic: Arc<A>, shutdown: ShutdownWatch) {
        debug!("new event!");
        let mut reuse_event = app_logic.process_new(event, &shutdown).await;
        while let Some(event) = reuse_event {
            // TODO: with no steal runtime, consider spawn() the next event on
            // another thread for more evenly load balancing
            debug!("new reusable event!");
            reuse_event = app_logic.process_new(event, &shutdown).await;
        }
    }

    async fn run_endpoint(
        app_logic: Arc<A>,
        mut stack: TransportStack,
        mut shutdown: ShutdownWatch,
    ) {
        if let Err(e) = stack.listen().await {
            error!("Listen() failed: {e}");
            return;
        }

        // the accept loop, until the system is shutting down
        loop {
            let new_io = tokio::select! { // TODO: consider biased for perf reason?
                new_io = stack.accept() => new_io,
                shutdown_signal = shutdown.changed() => {
                    match shutdown_signal {
                        Ok(()) => {
                            if !*shutdown.borrow() {
                                // happen in the initial read
                                continue;
                            }
                            info!("Shutting down {}", stack.as_str());
                            break;
                        }
                        Err(e) => {
                            error!("shutdown_signal error {e}");
                            break;
                        }
                    }
                }
            };
            match new_io {
                Ok(io) => {
                    let app = app_logic.clone();
                    let shutdown = shutdown.clone();
                    current_handle().spawn(async move {
                        match io.handshake().await {
                            Ok(io) => Self::handle_event(io, app, shutdown).await,
                            Err(e) => {
                                // TODO: Maybe IOApp trait needs a fn to handle/filter our this error
                                error!("Downstream handshake error {e}");
                            }
                        }
                    });
                }
                Err(e) => {
                    error!("Accept() failed {e}");
                    if let Some(io_error) = e
                        .root_cause()
                        .downcast_ref::<std::io::Error>()
                        .and_then(|e| e.raw_os_error())
                    {
                        // 24: too many open files. In this case accept() will continue return this
                        // error without blocking, which could use up all the resources
                        if io_error == 24 {
                            // call sleep to calm the thread down and wait for others to release
                            // some resources
                            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                        }
                    }
                }
            }
        }

        stack.cleanup();
    }
}

#[async_trait]
impl<A: ServerApp + Send + Sync + 'static> ServiceTrait for Service<A> {
    async fn start_service(
        &mut self,
        #[cfg(unix)] fds: Option<ListenFds>,
        shutdown: ShutdownWatch,
    ) {
        let runtime = current_handle();
        let endpoints = self.listeners.build(
            #[cfg(unix)]
            fds,
        );
        let app_logic = self
            .app_logic
            .take()
            .expect("can only start_service() once");
        let app_logic = Arc::new(app_logic);

        let handlers = endpoints.into_iter().map(|endpoint| {
            let shutdown = shutdown.clone();
            let my_app_logic = app_logic.clone();
            runtime.spawn(async move {
                Self::run_endpoint(my_app_logic, endpoint, shutdown).await;
            })
        });

        futures::future::join_all(handlers).await;
        self.listeners.cleanup();
        app_logic.cleanup().await;
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn threads(&self) -> Option<usize> {
        self.threads
    }
}

use crate::apps::prometheus_http_app::PrometheusServer;

impl Service<PrometheusServer> {
    /// The Prometheus HTTP server
    ///
    /// The HTTP server endpoint that reports Prometheus metrics collected in the entire service
    pub fn prometheus_http_service() -> Self {
        Service::new(
            "Prometheus metric HTTP".to_string(),
            PrometheusServer::new(),
        )
    }
}
