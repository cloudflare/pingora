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

//! Integration tests for `bootstrap_as_a_service`.
//!
//! Verifies that when `bootstrap_as_a_service()` dependencies are declared, the
//! `BootstrapComplete` execution phase is not reached until all dependency services have
//! finished their initialization work.

use async_trait::async_trait;
use pingora_core::server::ShutdownWatch;
use pingora_core::server::{configuration::ServerConf, ExecutionPhase, RunArgs, Server};
use pingora_core::services::background::{background_service, BackgroundService};
use pingora_core::services::ServiceReadyNotifier;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// A background service that sets a flag when it completes, after an optional delay.
///
/// Signals readiness only after completing its initialization work so that dependent
/// services (like `BootstrapService`) cannot start until this service is truly done.
struct TrackableService {
    delay: Duration,
    completed: Arc<AtomicBool>,
}

#[async_trait]
impl BackgroundService for TrackableService {
    async fn start_with_ready_notifier(
        &self,
        _shutdown: ShutdownWatch,
        ready_notifier: ServiceReadyNotifier,
    ) {
        if !self.delay.is_zero() {
            tokio::time::sleep(self.delay).await;
        }
        self.completed.store(true, Ordering::SeqCst);
        // Signal readiness only after work is done — this is what the dependency
        // mechanism waits on before allowing BootstrapService to proceed.
        ready_notifier.notify_ready();
    }
}

/// Verifies that `bootstrap_as_a_service` does not reach `BootstrapComplete` until all
/// declared dependency services have finished their initialization work.
#[test]
fn test_bootstrap_waits_for_dependencies() {
    let conf = ServerConf {
        grace_period_seconds: Some(1),
        graceful_shutdown_timeout_seconds: Some(1),
        ..Default::default()
    };

    let mut server = Server::new_with_opt_and_conf(None, conf);
    let mut phase = server.watch_execution_phase();

    // Two dependency services with delays. The second (150 ms) sets the pace.
    let dep1_done = Arc::new(AtomicBool::new(false));
    let dep2_done = Arc::new(AtomicBool::new(false));

    let dep1_handle = server.add_service(background_service(
        "dep1",
        TrackableService {
            delay: Duration::from_millis(50),
            completed: dep1_done.clone(),
        },
    ));
    let dep2_handle = server.add_service(background_service(
        "dep2",
        TrackableService {
            delay: Duration::from_millis(150),
            completed: dep2_done.clone(),
        },
    ));

    // BootstrapService must not reach BootstrapComplete until dep1 and dep2 are done.
    let bootstrap_handle = server.bootstrap_as_a_service();
    bootstrap_handle.add_dependencies([&dep1_handle, &dep2_handle]);

    // When using bootstrap_as_a_service, do NOT call server.bootstrap() separately —
    // the BootstrapService runs as a background service during run(), and emits
    // BootstrapComplete only after all its declared dependencies are ready.
    let _join = std::thread::spawn(move || {
        server.run(RunArgs::default());
    });

    let mut received_bootstrap = false;
    let mut received_bootstrap_complete = false;

    // Collect phases until BootstrapComplete is seen. Running may arrive
    // before or after Bootstrap/BootstrapComplete since main_loop starts
    // concurrently with the service runtimes.
    loop {
        match phase.blocking_recv() {
            Ok(ExecutionPhase::Bootstrap) => {
                received_bootstrap = true;
            }
            Ok(ExecutionPhase::BootstrapComplete) => {
                // Both dependencies must have set their flags before bootstrap completes.
                assert!(
                    dep1_done.load(Ordering::SeqCst),
                    "dep1 should be done before BootstrapComplete"
                );
                assert!(
                    dep2_done.load(Ordering::SeqCst),
                    "dep2 should be done before BootstrapComplete"
                );
                received_bootstrap_complete = true;
                break;
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }

    assert!(received_bootstrap, "should have seen Bootstrap phase");
    assert!(
        received_bootstrap_complete,
        "should have seen BootstrapComplete phase"
    );

    // Shut down cleanly.
    std::process::exit(0);
}
