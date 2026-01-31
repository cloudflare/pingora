// Run with: RUST_LOG=INFO cargo run --example execution_phases
// Send process QUIT message: pkill -QUIT execution_phases
use pingora::prelude::*;
use pingora_core::server::ExecutionPhase;
use std::time::Duration;

fn main() {
    let mut server = Server::new(None).unwrap();
    let mut phase_watch = server.watch_execution_phase();

    std::thread::spawn(move || {
        println!("Watching server phases in a background thread.");
        while let Ok(phase) = phase_watch.blocking_recv() {
            match phase {
                ExecutionPhase::Running(handle) => {
                    println!("Server is now running");
                    handle.spawn(async {
                        tokio::time::sleep(Duration::from_secs(1)).await;
                        println!("Background task completed");
                    });
                }
                _ => println!("Phase: {:?}", phase),
            }
        }
    });

    server.bootstrap();
    server.run_forever();
}
