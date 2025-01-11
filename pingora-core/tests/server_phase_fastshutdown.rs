// NOTE: This test sends a shutdown signal to itself,
// so it needs to be in an isolated test to prevent concurrency.

use pingora_core::server::{ExecutionPhase, Server};

// Ensure that execution phases are reported correctly.
#[test]
fn test_server_execution_phase_monitor_fast_shutdown() {
    let mut server = Server::new(None).unwrap();

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
        libc::raise(libc::SIGINT);
    }

    assert!(matches!(
        phase.blocking_recv().unwrap(),
        ExecutionPhase::ShutdownStarted,
    ));

    assert!(matches!(
        phase.blocking_recv().unwrap(),
        ExecutionPhase::ShutdownRuntimes,
    ));

    join.join().unwrap();

    assert!(matches!(
        phase.blocking_recv().unwrap(),
        ExecutionPhase::Terminated,
    ));
}
