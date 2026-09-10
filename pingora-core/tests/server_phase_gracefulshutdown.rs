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

// NOTE: This test sends a shutdown signal to itself,
// so it needs to be in an isolated test to prevent concurrency.

use async_trait::async_trait;
use pingora_core::server::{
    configuration::ServerConf, ExecutionPhase, RunArgs, Server, ShutdownWatch,
};
use pingora_core::services::background::{background_service, BackgroundService};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// Bounds how long the server waits for service runtimes to exit. Set far above
/// what an idle runtime needs, so a regression that serves the timeout as a
/// delay rather than a bound is unmistakable.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(30);

/// Budget for tearing down one idle service runtime: two orders of magnitude
/// above the expected cost and well under [`SHUTDOWN_TIMEOUT`], so neither a
/// loaded CI machine can exceed it nor a regression stay under it.
const TEARDOWN_BUDGET: Duration = Duration::from_secs(5);

/// Records that it ran, then returns once the server signals shutdown.
#[derive(Default)]
struct ShutdownProbe {
    started: AtomicBool,
}

#[async_trait]
impl BackgroundService for ShutdownProbe {
    async fn start(&self, mut shutdown: ShutdownWatch) {
        self.started.store(true, Ordering::SeqCst);

        while shutdown.changed().await.is_ok() {
            if *shutdown.borrow() {
                break;
            }
        }
    }
}

// Ensure that execution phases are reported correctly, and that waiting for
// service runtimes to exit is bounded by the graceful shutdown timeout rather
// than always taking it.
#[test]
fn test_server_execution_phase_monitor_graceful_shutdown() {
    let conf = ServerConf {
        // The grace period is a fixed wait by design, so keep it short.
        grace_period_seconds: Some(1),
        graceful_shutdown_timeout_seconds: Some(SHUTDOWN_TIMEOUT.as_secs()),
        ..Default::default()
    };
    let mut server = Server::new_with_opt_and_conf(None, conf);

    let mut phase = server.watch_execution_phase();

    // A service is required: the server only waits on runtimes it started, so
    // with none registered that wait is over an empty list and the assertion
    // below would hold however long the wait takes.
    let service = background_service("shutdown probe", ShutdownProbe::default());
    let probe = service.task();
    server.add_service(service);

    let join = std::thread::spawn(move || {
        server.bootstrap();
        server.run(RunArgs::default());
    });

    assert!(matches!(
        phase.blocking_recv().unwrap(),
        ExecutionPhase::Bootstrap
    ));
    assert!(matches!(
        phase.blocking_recv().unwrap(),
        ExecutionPhase::BootstrapComplete,
    ));
    assert!(matches!(
        phase.blocking_recv().unwrap(),
        ExecutionPhase::Running,
    ));

    // Need to wait for startup, otherwise the signal handler is not
    // installed yet.
    //
    // TODO: signal handlers are installed after Running phase
    // message is sent, sleep for now to avoid test flake
    std::thread::sleep(std::time::Duration::from_millis(500));

    unsafe {
        libc::raise(libc::SIGTERM);
    }

    assert!(matches!(
        phase.blocking_recv().unwrap(),
        ExecutionPhase::GracefulTerminate,
    ));

    assert!(matches!(
        phase.blocking_recv().unwrap(),
        ExecutionPhase::ShutdownStarted,
    ));

    assert!(matches!(
        phase.blocking_recv().unwrap(),
        ExecutionPhase::ShutdownGracePeriod,
    ));

    assert!(matches!(
        phase.blocking_recv().unwrap(),
        ExecutionPhase::ShutdownRuntimes,
    ));
    // Measured from here rather than from the signal, so the grace period stays
    // outside the measurement.
    let runtime_shutdown_started = Instant::now();

    join.join().unwrap();

    assert!(matches!(
        phase.blocking_recv().unwrap(),
        ExecutionPhase::Terminated,
    ));
    let runtime_shutdown_took = runtime_shutdown_started.elapsed();

    assert!(
        probe.started.load(Ordering::SeqCst),
        "the background service never ran, so the server had no service runtime \
         to wait on and the timing assertion below would hold regardless"
    );

    assert!(
        runtime_shutdown_took < TEARDOWN_BUDGET,
        "waiting for an idle service runtime to exit took {runtime_shutdown_took:?}, \
         over the {TEARDOWN_BUDGET:?} budget, against a {SHUTDOWN_TIMEOUT:?} configured \
         timeout: the timeout is being served as a delay rather than used as a bound"
    );
}
