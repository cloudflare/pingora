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

//! Example demonstrating how to start a server using [`Server::bootstrap_as_a_service`]
//! instead of calling [`Server::bootstrap`] directly.
//!
//! # Why `bootstrap_as_a_service`?
//!
//! [`Server::bootstrap`] runs the bootstrap phase synchronously before any services start.
//! This means the calling thread blocks during socket FD acquisition and Sentry initialization.
//!
//! [`Server::bootstrap_as_a_service`] instead schedules bootstrap as a dependency-aware init
//! service. This allows other services to declare a dependency on the bootstrap handle and
//! ensures they only start after bootstrap completes — while keeping setup fully asynchronous
//! and composable with the rest of the service graph.
//!
//! Use `bootstrap_as_a_service` when:
//! - You want to integrate bootstrap into the service dependency graph
//! - You want services to wait for bootstrap without blocking the main thread
//! - You are building more complex startup sequences (e.g. multiple ordered init steps)
//!
//! # Running the example
//!
//! ```bash
//! cargo run --example bootstrap_as_a_service --package pingora-core
//! ```
//!
//! # Expected behaviour
//!
//! Bootstrap runs as a service before `MyService` starts. `MyService` declares a dependency
//! on the bootstrap handle, so it will not be started until bootstrap has completed.

use async_trait::async_trait;
use log::info;
use pingora_core::server::configuration::Opt;
#[cfg(unix)]
use pingora_core::server::ListenFds;
use pingora_core::server::{Server, ShutdownWatch};
use pingora_core::services::Service;

/// A simple application service that requires bootstrap to be complete before it starts.
pub struct MyService;

#[async_trait]
impl Service for MyService {
    async fn start_service(
        &mut self,
        #[cfg(unix)] _fds: Option<ListenFds>,
        mut shutdown: ShutdownWatch,
        _listeners_per_fd: usize,
    ) {
        info!("MyService: bootstrap is complete, starting up");

        // Keep running until a shutdown signal is received.
        shutdown.changed().await.ok();

        info!("MyService: shutting down");
    }

    fn name(&self) -> &str {
        "my_service"
    }

    fn threads(&self) -> Option<usize> {
        Some(1)
    }
}

fn main() {
    env_logger::Builder::from_default_env()
        .filter_level(log::LevelFilter::Info)
        .init();

    let opt = Opt::parse_args();
    let mut server = Server::new(Some(opt)).unwrap();

    // Schedule bootstrap as a service instead of calling server.bootstrap() directly.
    // The returned handle can be used to declare dependencies so that other services
    // only start after bootstrap has finished.
    let bootstrap_handle = server.bootstrap_as_a_service();

    // Register our application service and get its handle.
    let service_handle = server.add_service(MyService);

    // MyService will not start until the bootstrap service has signaled that it is ready.
    service_handle.add_dependency(&bootstrap_handle);

    info!("Starting server — bootstrap will run as a service before MyService starts");

    server.run_forever();
}
