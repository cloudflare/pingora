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

//! # Graceful Upgrade Example
//!
//! Demonstrates the `daemon_wait_for_ready` feature, which coordinates graceful process upgrades
//! by ensuring the new process is fully bootstrapped before the old one begins shutting down.
//!
//! ## Background
//!
//! In a standard daemonized pingora service, the parent process exits immediately after the
//! daemon fork. During a graceful upgrade, the process manager sends SIGQUIT to the old process
//! as soon as the new process's parent exits — potentially before the new process has finished
//! initializing its backends, consistent hash rings, or other state. This can cause a brief
//! window of 502s.
//!
//! With `daemon_wait_for_ready = true`, the parent instead waits for the daemon to send SIGUSR1
//! before exiting. The process manager only proceeds to stop the old process once the new one
//! signals that it is ready to serve traffic.
//!
//! ## Service startup order
//!
//! This example sets up the following dependency chain:
//!
//! ```text
//!   BackendDiscoveryService  HashRingService
//!             \                    /
//!              \                  /
//!            BootstrapService (socket transfer + SIGUSR1 to parent)
//! ```
//!
//! The bootstrap service — which handles transferring listening sockets from the old process and
//! sending SIGUSR1 to the parent to signal readiness — only runs after both slow initialization
//! services have completed. This ensures the parent never exits until the new process is truly
//! ready to serve traffic.
//!
//! ## Usage
//!
//! ```bash
//! # Run interactively (no daemonization)
//! cargo run --example graceful_upgrade -p pingora
//!
//! # Run as a daemon
//! cargo run --example graceful_upgrade -p pingora -- -d
//!
//! # Graceful upgrade of a running daemon instance
//! cargo run --example graceful_upgrade -p pingora -- -d -u
//! ```

use async_trait::async_trait;
use bytes::Bytes;
use clap::Parser;
use http::{Response, StatusCode};
use log::info;
use std::num::NonZeroU64;
use std::time::Duration;
use tokio::time::sleep;

use pingora::apps::http_app::ServeHttp;
use pingora::prelude::Opt;
use pingora::protocols::http::ServerSession;
use pingora::server::configuration::ServerConf;
use pingora::server::{Server, ShutdownWatch};
use pingora::services::background::{background_service, BackgroundService};
use pingora::services::listening::Service as ListeningService;

/// Simulates slow backend discovery — e.g. resolving upstream endpoints from a service registry.
pub struct BackendDiscoveryService;

#[async_trait]
impl BackgroundService for BackendDiscoveryService {
    async fn start(&self, _shutdown: ShutdownWatch) {
        info!("BackendDiscoveryService: discovering backends...");
        sleep(Duration::from_secs(2)).await;
        info!("BackendDiscoveryService: backends ready");
    }
}

/// Simulates slow consistent hash ring construction. Runs in parallel with
/// `BackendDiscoveryService`; bootstrap waits for both to complete.
pub struct HashRingService;

#[async_trait]
impl BackgroundService for HashRingService {
    async fn start(&self, _shutdown: ShutdownWatch) {
        info!("HashRingService: building consistent hash ring...");
        sleep(Duration::from_secs(3)).await;
        info!("HashRingService: hash ring ready");
    }
}

/// A minimal HTTP service that responds to every request with 200 OK.
///
/// Accepts an optional `sleep` query parameter specifying how many seconds to wait before
/// responding (e.g. `GET /?sleep=20`). This makes in-flight requests easy to observe during a
/// graceful upgrade: a request with a long sleep that arrives just before the upgrade begins will
/// still be running when the new process starts up, demonstrating that the old process keeps
/// serving until all connections are drained.
pub struct HelloApp;

#[async_trait]
impl ServeHttp for HelloApp {
    async fn response(&self, http_stream: &mut ServerSession) -> Response<Vec<u8>> {
        let delay_secs = http_stream
            .req_header()
            .uri
            .query()
            .and_then(|q| {
                q.split('&').find_map(|pair| {
                    let (key, val) = pair.split_once('=')?;
                    if key == "sleep" {
                        val.parse::<u64>().ok()
                    } else {
                        None
                    }
                })
            })
            .unwrap_or(0);

        if delay_secs > 0 {
            sleep(Duration::from_secs(delay_secs)).await;
        }

        let body = Bytes::from("hello from graceful_upgrade example\n");
        Response::builder()
            .status(StatusCode::OK)
            .header(http::header::CONTENT_TYPE, "text/plain")
            .header(http::header::CONTENT_LENGTH, body.len())
            .body(body.to_vec())
            .unwrap()
    }
}

fn main() {
    env_logger::init();

    let opt = Some(Opt::parse());

    // Build a ServerConf with daemon_wait_for_ready enabled.
    //
    // When the server is started with -d (daemon mode), the parent process waits for SIGUSR1
    // before exiting. The daemon sends SIGUSR1 only after the bootstrap service completes —
    // which in this example means after both slow services have signaled readiness.
    let conf = ServerConf {
        daemon: true,
        daemon_wait_for_ready: true,
        daemon_ready_timeout_seconds: NonZeroU64::new(60),
        ..ServerConf::default()
    };

    let mut server = Server::new_with_opt_and_conf(opt, conf);

    // Add the slow initialization services and retain their handles so bootstrap can depend
    // on them. Both run in parallel; the slowest (HashRingService at 3s) sets the pace.
    let backend_handle = server.add_service(background_service(
        "backend_discovery",
        BackendDiscoveryService,
    ));
    let hash_ring_handle = server.add_service(background_service("hash_ring", HashRingService));

    // bootstrap_as_a_service() registers the bootstrap service (socket transfer from the old
    // process + SIGUSR1 to the parent) and returns its ServiceHandle. Declaring the slow
    // services as dependencies ensures bootstrap only runs once both are ready.
    let bootstrap_handle = server.bootstrap_as_a_service();
    bootstrap_handle.add_dependencies([&backend_handle, &hash_ring_handle]);

    let mut http_service = ListeningService::new("hello_http".to_string(), HelloApp);
    http_service.add_tcp("0.0.0.0:8000");

    server
        .add_service(http_service)
        .add_dependency(backend_handle);

    server.run_forever();
}
