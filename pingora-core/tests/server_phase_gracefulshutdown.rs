// Copyright 2025 Cloudflare, Inc.
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

use pingora_core::server::{configuration::ServerConf, ExecutionPhase, RunArgs, Server};

// Ensure that execution phases are reported correctly.
#[test]
fn test_server_execution_phase_monitor_graceful_shutdown() {
    let conf = ServerConf {
        // Use small timeouts to speed up the test.
        grace_period_seconds: Some(1),
        graceful_shutdown_timeout_seconds: Some(1),
        ..Default::default()
    };
    let mut server = Server::new_with_opt_and_conf(None, conf);

    let mut phase = server.watch_execution_phase();

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
        dbg!(phase.blocking_recv().unwrap()),
        ExecutionPhase::ShutdownRuntimes,
    ));

    join.join().unwrap();

    assert!(matches!(
        phase.blocking_recv().unwrap(),
        ExecutionPhase::Terminated,
    ));
}
