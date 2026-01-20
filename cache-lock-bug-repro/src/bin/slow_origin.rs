// Slow Origin Server for Cache Lock Bug Reproduction
//
// This server simulates a slow upstream that holds responses for a configurable
// time before responding. It's used to demonstrate the Pingora cache lock bug
// where clients waiting on a cache lock don't get notified when the lock holder's
// client disconnects.

use clap::Parser;
use log::{error, info};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::time::sleep;

#[derive(Parser, Debug)]
#[command(name = "slow-origin")]
#[command(about = "A slow origin server for testing cache lock behavior")]
struct Args {
    /// Port to listen on
    #[arg(short, long, default_value = "8000")]
    port: u16,

    /// Default response delay in seconds (can be overridden by x-set-sleep header)
    #[arg(short, long, default_value = "5")]
    delay: u64,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let args = Args::parse();
    let addr = format!("127.0.0.1:{}", args.port);
    let listener = TcpListener::bind(&addr).await?;

    info!("Slow origin server listening on {}", addr);
    info!("Default delay: {} seconds", args.delay);

    loop {
        let (mut socket, peer_addr) = listener.accept().await?;
        let default_delay = args.delay;

        tokio::spawn(async move {
            info!("Connection from {}", peer_addr);

            let (reader, mut writer) = socket.split();
            let mut buf_reader = BufReader::new(reader);
            let mut request_line = String::new();
            let mut headers = Vec::new();

            // Read request line
            if buf_reader.read_line(&mut request_line).await.is_err() {
                return;
            }
            info!("Request: {}", request_line.trim());

            // Read headers
            loop {
                let mut line = String::new();
                if buf_reader.read_line(&mut line).await.is_err() {
                    return;
                }
                if line == "\r\n" || line == "\n" || line.is_empty() {
                    break;
                }
                headers.push(line.trim().to_string());
            }

            // Parse x-set-sleep header to determine delay
            let mut delay_secs = default_delay;
            for header in &headers {
                if let Some(value) = header.strip_prefix("x-set-sleep:") {
                    if let Ok(secs) = value.trim().parse::<u64>() {
                        delay_secs = secs;
                    }
                }
                // Also check for X-Set-Sleep (case-insensitive comparison)
                if let Some(value) = header.strip_prefix("X-Set-Sleep:") {
                    if let Ok(secs) = value.trim().parse::<u64>() {
                        delay_secs = secs;
                    }
                }
            }

            info!("Sleeping for {} seconds before responding...", delay_secs);
            sleep(Duration::from_secs(delay_secs)).await;

            // Send response with caching headers
            let body = "hello world";
            let response = format!(
                "HTTP/1.1 200 OK\r\n\
                 Content-Type: text/plain\r\n\
                 Content-Length: {}\r\n\
                 Cache-Control: public, max-age=60\r\n\
                 Connection: close\r\n\
                 \r\n\
                 {}",
                body.len(),
                body
            );

            if let Err(e) = writer.write_all(response.as_bytes()).await {
                error!("Error writing response: {}", e);
            } else {
                info!("Response sent to {}", peer_addr);
            }
        });
    }
}
