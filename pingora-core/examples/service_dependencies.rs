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

//! Example demonstrating service dependency management.
//!
//! This example shows how services can declare dependencies on other services using
//! a fluent API with [`ServiceHandle`] references, ensuring they start in the correct
//! order and wait for dependencies to be ready.
//!
//! # Running the example
//!
//! ```bash
//! cargo run --example service_dependencies --package pingora-core
//! ```
//!
//! Expected output:
//! - DatabaseService starts and initializes (takes 2 seconds)
//! - CacheService starts and initializes (takes 1 second)
//! - ApiService waits for both dependencies, then starts
//!
//! # Key Features Demonstrated
//!
//! - Fluent API for declaring dependencies via [`ServiceHandle::add_dependency()`]
//! - Type-safe dependency declaration (no strings)
//! - Multiple ways to implement services based on readiness needs:
//!   - **DatabaseService**: Custom readiness timing (uses `ServiceWithDependencies`)
//!   - **CacheService**: Ready immediately (uses `Service`)
//!   - **ApiService**: Ready immediately (uses `Service`)
//! - Automatic dependency ordering and validation
//! - Prevention of typos in service names (compile-time safety)

use async_trait::async_trait;
use log::info;
use pingora_core::server::configuration::Opt;
#[cfg(unix)]
use pingora_core::server::ListenFds;
use pingora_core::server::{Server, ShutdownWatch};
use pingora_core::services::{Service, ServiceWithDependents};
// DatabaseService needs to control readiness timing
use pingora_core::services::ServiceReadyNotifier;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::time::{sleep, Duration};

/// A custom service that delays signaling ready until initialization is complete
pub struct DatabaseService {
    connection_string: Arc<Mutex<Option<String>>>,
}

impl DatabaseService {
    fn new() -> Self {
        Self {
            connection_string: Arc::new(Mutex::new(None)),
        }
    }

    fn get_connection_string(&self) -> Arc<Mutex<Option<String>>> {
        self.connection_string.clone()
    }
}

#[async_trait]
impl ServiceWithDependents for DatabaseService {
    async fn start_service(
        &mut self,
        #[cfg(unix)] _fds: Option<ListenFds>,
        mut shutdown: ShutdownWatch,
        _listeners_per_fd: usize,
        ready_notifier: ServiceReadyNotifier,
    ) {
        info!("DatabaseService: Starting initialization...");

        // Simulate database connection setup
        sleep(Duration::from_secs(2)).await;

        // Store the connection string
        {
            let mut conn = self.connection_string.lock().await;
            *conn = Some("postgresql://localhost:5432/mydb".to_string());
        }

        info!("DatabaseService: Initialization complete, signaling ready");

        // Signal that the service is ready
        ready_notifier.notify_ready();

        // Keep running until shutdown
        shutdown.changed().await.ok();
        info!("DatabaseService: Shutting down");
    }

    fn name(&self) -> &str {
        "database"
    }

    fn threads(&self) -> Option<usize> {
        Some(1)
    }
}

/// A cache service that uses the simplified API
/// Signals ready immediately (using default implementation)
pub struct CacheService;

#[async_trait]
impl Service for CacheService {
    // Uses default start_service implementation which signals ready immediately

    async fn start_service(
        &mut self,
        #[cfg(unix)] _fds: Option<ListenFds>,
        mut shutdown: ShutdownWatch,
        _listeners_per_fd: usize,
    ) {
        info!("CacheService: Starting (ready immediately)...");

        // Simulate cache warmup
        sleep(Duration::from_secs(1)).await;
        info!("CacheService: Warmup complete");

        // Keep running until shutdown
        shutdown.changed().await.ok();
        info!("CacheService: Shutting down");
    }

    fn name(&self) -> &str {
        "cache"
    }

    fn threads(&self) -> Option<usize> {
        Some(1)
    }
}

/// An API service that depends on both database and cache
/// Uses the simplest API - signals ready immediately and just implements [Service]
pub struct ApiService {
    db_connection: Arc<Mutex<Option<String>>>,
}

impl ApiService {
    fn new(db_connection: Arc<Mutex<Option<String>>>) -> Self {
        Self { db_connection }
    }
}

#[async_trait]
impl Service for ApiService {
    // Uses default start_service - signals ready immediately

    async fn start_service(
        &mut self,
        #[cfg(unix)] _fds: Option<ListenFds>,
        mut shutdown: ShutdownWatch,
        _listeners_per_fd: usize,
    ) {
        info!("ApiService: Starting (dependencies should be ready)...");

        // Verify database connection is available
        {
            let conn = self.db_connection.lock().await;
            if let Some(conn_str) = &*conn {
                info!("ApiService: Using database connection: {}", conn_str);
            } else {
                panic!("ApiService: Database connection not available!");
            }
        }

        info!("ApiService: Ready to serve requests");

        // Keep running until shutdown
        shutdown.changed().await.ok();
        info!("ApiService: Shutting down");
    }

    fn name(&self) -> &str {
        "api"
    }

    fn threads(&self) -> Option<usize> {
        Some(1)
    }
}

fn main() {
    env_logger::Builder::from_default_env()
        .filter_level(log::LevelFilter::Info)
        .init();

    info!("Starting server with service dependencies...");

    let opt = Opt::parse_args();
    let mut server = Server::new(Some(opt)).unwrap();
    server.bootstrap();

    // Create the database service
    let db_service = DatabaseService::new();
    let db_connection = db_service.get_connection_string();

    // Create services
    let cache_service = CacheService;
    let api_service = ApiService::new(db_connection);

    // Add services and get their handles
    let db_handle = server.add_service(db_service);
    let cache_handle = server.add_service(cache_service);
    let api_handle = server.add_service(api_service);

    // Declare dependencies using the fluent API
    // The API service will not start until both dependencies signal ready
    api_handle.add_dependency(db_handle);
    api_handle.add_dependency(&cache_handle);

    info!("Services configured. Starting server...");
    info!("Expected startup order:");
    info!("  1. database (will initialize for 2 seconds)");
    info!("  2. cache (will initialize for 1 second)");
    info!("  3. api (will wait for both, then start)");
    info!("");
    info!("Press Ctrl+C to shut down");

    server.run_forever();
}
