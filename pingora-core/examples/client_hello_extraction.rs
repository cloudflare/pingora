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

//! Example: Extracting ClientHello from TLS connections
//!
//! This example demonstrates how to extract ClientHello information
//! (SNI, ALPN, etc.) from incoming TLS connections using MSG_PEEK.
//!
//! This approach is TLS backend-agnostic and works by peeking at the
//! TCP stream before the TLS handshake begins.

use pingora_core::protocols::l4::stream::Stream;
use pingora_core::protocols::ClientHelloWrapper;
use tokio::net::TcpListener;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::init();

    println!("Starting TLS ClientHello extraction example...");
    println!("Listening on 127.0.0.1:8443");
    println!("\nTo test, use:");
    println!("  openssl s_client -connect 127.0.0.1:8443 -servername example.com -alpn h2,http/1.1");
    println!();

    let listener = TcpListener::bind("127.0.0.1:8443").await?;

    loop {
        let (tcp_stream, addr) = listener.accept().await?;
        println!("New connection from: {}", addr);

        tokio::spawn(async move {
            // Convert to Pingora Stream
            let stream: Stream = tcp_stream.into();

            // Wrap the stream with ClientHelloWrapper
            let mut wrapper = ClientHelloWrapper::new(stream);

            // Extract ClientHello before TLS handshake
            #[cfg(unix)]
            match wrapper.extract_client_hello() {
                Ok(Some(hello)) => {
                    println!("\n=== ClientHello Information ===");
                    println!("TLS Version: {:?}", hello.tls_version.map(|v| format!("0x{:04x}", v)));
                    println!("SNI: {:?}", hello.sni);
                    println!("ALPN Protocols: {:?}", hello.alpn);
                    println!("Raw ClientHello size: {} bytes", hello.raw.len());
                    println!("================================\n");

                    // Now you can use this information to:
                    // 1. Route to different backends based on SNI
                    // 2. Choose appropriate TLS certificates
                    // 3. Log connection information
                    // 4. Apply security policies based on SNI/ALPN
                }
                Ok(None) => {
                    println!("No ClientHello detected (might not be TLS)");
                }
                Err(e) => {
                    println!("Error extracting ClientHello: {:?}", e);
                }
            }

            #[cfg(not(unix))]
            println!("ClientHello extraction not supported on this platform");

            // At this point, you can proceed with the TLS handshake
            // The MSG_PEEK operation doesn't consume the data, so TLS
            // libraries will read the ClientHello normally

            println!("Connection handling complete for {}", addr);
        });
    }
}

