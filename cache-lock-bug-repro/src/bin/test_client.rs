// Test Client for Cache Lock Bug Reproduction
//
// This client demonstrates the Pingora cache lock client disconnect bug:
//
// 1. First request (writer) is sent to a slow origin (5s delay)
//    - This request acquires the cache lock
//
// 2. While writer waits for origin, reader requests are sent
//    - These requests wait on the cache lock
//
// 3. Reader clients disconnect after 500ms
//    - BUG: Server-side handlers keep waiting on the lock
//    - With fix: Server detects disconnect and exits immediately
//
// The test measures how long the server takes to release resources
// after client disconnect. Without the fix, this is ~60 seconds (lock timeout).
// With the fix, this should be <1 second.

use clap::Parser;
use log::{error, info, warn};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::{sleep, timeout};

#[derive(Parser, Debug)]
#[command(name = "test-client")]
#[command(about = "Test client for cache lock bug reproduction")]
struct Args {
    /// Proxy port
    #[arg(short, long, default_value = "6148")]
    proxy_port: u16,

    /// Number of reader clients to spawn
    #[arg(short, long, default_value = "3")]
    num_readers: usize,

    /// Time to wait before disconnecting readers (milliseconds)
    #[arg(short, long, default_value = "500")]
    disconnect_delay_ms: u64,

    /// Expected server close time threshold (milliseconds)
    /// With fix: should be < 1000ms
    /// Without fix: will be ~60000ms (lock timeout)
    #[arg(short, long, default_value = "2000")]
    threshold_ms: u64,

    /// URL path to request (should be unique per test run)
    #[arg(long, default_value = "/test/cache-lock-bug")]
    path: String,

    /// Origin sleep time in seconds (writer will wait this long)
    #[arg(long, default_value = "5")]
    origin_delay: u64,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let args = Args::parse();
    let proxy_addr = format!("127.0.0.1:{}", args.proxy_port);

    // Add timestamp to path to ensure unique cache key
    let unique_path = format!("{}/{}", args.path, chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0));

    info!("=== Cache Lock Client Disconnect Bug Reproduction ===");
    info!("");
    info!("Configuration:");
    info!("  Proxy address: {}", proxy_addr);
    info!("  Request path: {}", unique_path);
    info!("  Number of readers: {}", args.num_readers);
    info!("  Reader disconnect delay: {}ms", args.disconnect_delay_ms);
    info!("  Origin delay: {}s", args.origin_delay);
    info!("  Success threshold: {}ms", args.threshold_ms);
    info!("");
    info!("Test scenario:");
    info!("  1. Writer acquires cache lock, origin takes {}s to respond", args.origin_delay);
    info!("  2. {} readers start waiting on cache lock", args.num_readers);
    info!("  3. Readers disconnect after {}ms", args.disconnect_delay_ms);
    info!("  4. Measure how long server takes to close connections");
    info!("");
    info!("Expected behavior:");
    info!("  WITH FIX: Server closes reader connections in <{}ms", args.threshold_ms);
    info!("  WITHOUT FIX: Server waits for lock timeout (up to 60s)");
    info!("");

    let test_start = Instant::now();

    // === Step 1: Start writer request ===
    info!("--- Step 1: Starting writer request ---");
    let writer_proxy_addr = proxy_addr.clone();
    let writer_path = unique_path.clone();
    let writer_origin_delay = args.origin_delay;

    let writer_handle = tokio::spawn(async move {
        let start = Instant::now();
        info!("Writer: Connecting to proxy...");

        match TcpStream::connect(&writer_proxy_addr).await {
            Ok(mut stream) => {
                let request = format!(
                    "GET {} HTTP/1.1\r\n\
                     Host: localhost\r\n\
                     x-set-sleep: {}\r\n\
                     Connection: close\r\n\
                     \r\n",
                    writer_path, writer_origin_delay
                );

                if let Err(e) = stream.write_all(request.as_bytes()).await {
                    error!("Writer: Failed to send request: {}", e);
                    return None;
                }

                info!("Writer: Request sent, waiting for response (origin delay: {}s)...", writer_origin_delay);

                // Read response
                let mut response = Vec::new();
                match stream.read_to_end(&mut response).await {
                    Ok(_) => {
                        let elapsed = start.elapsed();
                        info!("Writer: Response received after {:?}", elapsed);
                        Some(elapsed)
                    }
                    Err(e) => {
                        error!("Writer: Failed to read response: {}", e);
                        None
                    }
                }
            }
            Err(e) => {
                error!("Writer: Failed to connect: {}", e);
                None
            }
        }
    });

    // Give writer time to acquire the cache lock
    sleep(Duration::from_millis(200)).await;

    // === Step 2: Start reader requests ===
    info!("--- Step 2: Starting {} reader requests ---", args.num_readers);

    let mut reader_handles = Vec::new();

    for i in 0..args.num_readers {
        let reader_proxy_addr = proxy_addr.clone();
        let reader_path = unique_path.clone();
        let disconnect_delay = Duration::from_millis(args.disconnect_delay_ms);

        reader_handles.push(tokio::spawn(async move {
            let start = Instant::now();
            info!("Reader {}: Connecting to proxy...", i);

            match TcpStream::connect(&reader_proxy_addr).await {
                Ok(mut stream) => {
                    let request = format!(
                        "GET {} HTTP/1.1\r\n\
                         Host: localhost\r\n\
                         Connection: close\r\n\
                         \r\n",
                        reader_path
                    );

                    if let Err(e) = stream.write_all(request.as_bytes()).await {
                        error!("Reader {}: Failed to send request: {}", i, e);
                        return (i, None, "send_failed".to_string());
                    }

                    info!("Reader {}: Request sent, will disconnect in {:?}...", i, disconnect_delay);

                    // Wait a bit then shutdown write side (signal disconnect)
                    sleep(disconnect_delay).await;

                    let disconnect_time = Instant::now();
                    info!("Reader {}: Shutting down write side (signaling disconnect)...", i);

                    if let Err(e) = stream.shutdown().await {
                        warn!("Reader {}: Shutdown error (expected): {}", i, e);
                    }

                    // Keep read side open to observe when server closes the connection
                    // This is the key measurement: how long until server releases resources?
                    let mut buf = [0u8; 1024];
                    let read_result = timeout(Duration::from_secs(120), stream.read(&mut buf)).await;

                    let server_close_time = disconnect_time.elapsed();
                    let total_time = start.elapsed();

                    let result_desc = match &read_result {
                        Ok(Ok(0)) => "server_closed_gracefully".to_string(),
                        Ok(Ok(n)) => format!("received_{}_bytes", n),
                        Ok(Err(e)) => format!("read_error: {}", e),
                        Err(_) => "timeout_120s".to_string(),
                    };

                    info!(
                        "Reader {}: Server closed connection after {:?} (total: {:?}, result: {})",
                        i, server_close_time, total_time, result_desc
                    );

                    (i, Some(server_close_time), result_desc)
                }
                Err(e) => {
                    error!("Reader {}: Failed to connect: {}", i, e);
                    (i, None, format!("connect_failed: {}", e))
                }
            }
        }));
    }

    // === Step 3: Wait for readers and measure server close times ===
    info!("--- Step 3: Waiting for readers to complete ---");

    let mut server_close_times = Vec::new();
    let mut any_timeout = false;

    for handle in reader_handles {
        match handle.await {
            Ok((i, Some(close_time), desc)) => {
                info!("Reader {}: Server close time = {:?} ({})", i, close_time, desc);
                if desc == "timeout_120s" {
                    any_timeout = true;
                }
                server_close_times.push(close_time);
            }
            Ok((i, None, desc)) => {
                warn!("Reader {}: Failed to get close time ({})", i, desc);
            }
            Err(e) => {
                error!("Reader task panicked: {}", e);
            }
        }
    }

    // === Step 4: Wait for writer to complete ===
    info!("--- Step 4: Waiting for writer to complete ---");

    match writer_handle.await {
        Ok(Some(duration)) => {
            info!("Writer completed in {:?}", duration);
        }
        Ok(None) => {
            warn!("Writer failed");
        }
        Err(e) => {
            error!("Writer task panicked: {}", e);
        }
    }

    // === Step 5: Analyze results ===
    info!("");
    info!("=== RESULTS ===");
    info!("");

    let total_test_time = test_start.elapsed();
    info!("Total test time: {:?}", total_test_time);

    if server_close_times.is_empty() {
        error!("No server close times recorded - test failed");
        return Ok(());
    }

    let max_close_time = server_close_times.iter().max().unwrap();
    let min_close_time = server_close_times.iter().min().unwrap();
    let avg_close_time = server_close_times.iter().sum::<Duration>() / server_close_times.len() as u32;

    info!("Server close times after client disconnect:");
    info!("  Min: {:?}", min_close_time);
    info!("  Max: {:?}", max_close_time);
    info!("  Avg: {:?}", avg_close_time);
    info!("");

    let threshold = Duration::from_millis(args.threshold_ms);

    if any_timeout {
        error!("FAIL: Some readers timed out waiting for server to close connection");
        error!("");
        error!("This indicates the bug is present:");
        error!("  Server did not detect client disconnect");
        error!("  Server is likely waiting for the cache lock to timeout (60s)");
        error!("");
        error!("With the fix, server should detect disconnect immediately");
    } else if *max_close_time > threshold {
        error!("FAIL: Server took too long to close connections");
        error!("");
        error!("Max close time: {:?}", max_close_time);
        error!("Threshold: {:?}", threshold);
        error!("");
        error!("This indicates the bug is present:");
        error!("  Server waited for cache lock timeout instead of detecting disconnect");
        error!("");
        error!("With the fix, server should close connections in <{}ms", args.threshold_ms);
    } else {
        info!("PASS: Server closed connections quickly after client disconnect");
        info!("");
        info!("Max close time {:?} < threshold {:?}", max_close_time, threshold);
        info!("");
        info!("This indicates the fix is working:");
        info!("  Server detected client disconnect");
        info!("  Server exited cache lock wait early");
    }

    info!("");
    Ok(())
}
