// NOTE: This test sends a shutdown signal to itself,
// so it needs to be in an isolated test to prevent concurrency.

use pingora_core::server::{configuration::ServerConf, ExecutionPhase, Server};

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
        server.run();
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
