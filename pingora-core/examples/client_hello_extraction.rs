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

use ::proxy_protocol as wire_proxy_protocol;
use pingora_core::protocols::l4::stream::Stream;
use pingora_core::protocols::proxy_protocol::{self, ProxyHeader};
use pingora_core::protocols::ClientHelloWrapper;
use std::{io, net::SocketAddr};
use tokio::net::{TcpListener, TcpStream};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::init();

    let listen_addr = "127.0.0.1:8443";

    println!("Starting TLS ClientHello extraction example...");
    println!("Listening on {listen_addr}");
    println!("\nTo test, use:");
    println!("  openssl s_client -connect {listen_addr} -servername example.com -alpn h2,http/1.1");
    println!();

    println!("\nTo test proxy requests with socat:");
    println!("  # Terminal 1: forward connections and inject a PROXY header");
    println!("  socat -d -d -v TCP-LISTEN:9443,reuseaddr,fork TCP:{listen_addr},proxyproto=TCP4:203.0.113.10:54321:198.51.100.5:443");
    println!("  # Terminal 2: connect through the forwarder");
    println!(
        "  openssl s_client -connect 127.0.0.1:9443 -servername example.com -alpn h2,http/1.1"
    );
    println!("  # Requires socat built with proxyproto support (v1.7.4 or newer).");

    let listener = TcpListener::bind(listen_addr).await?;

    loop {
        let (tcp_stream, addr) = listener.accept().await?;
        println!("New connection from: {}", addr);

        tokio::spawn(async move {
            if let Err(err) = handle_connection(tcp_stream, addr).await {
                println!("Connection handling error for {}: {:?}", addr, err);
            }
        });
    }
}

async fn handle_connection(tcp_stream: TcpStream, addr: SocketAddr) -> io::Result<()> {
    // Convert to Pingora Stream first so we can reuse the IO stack.
    let mut stream: Stream = tcp_stream.into();

    let proxy_header = proxy_protocol::consume_proxy_header(&mut stream)
        .await
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;

    println!("Consumed proxy header");

    if let Some(header) = &proxy_header {
        log_proxy_header(header);
    } else {
        println!("No PROXY header detected; using peer {}", addr);
    }

    // Wrap the stream with ClientHelloWrapper
    let mut wrapper = ClientHelloWrapper::new(stream);

    // Extract ClientHello before TLS handshake
    #[cfg(unix)]
    match wrapper.extract_client_hello() {
        Ok(Some(hello)) => {
            println!("\n=== ClientHello Information ===");
            println!(
                "TLS Version: {:?}",
                hello.tls_version.map(|v| format!("0x{:04x}", v))
            );
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

    Ok(())
}

fn log_proxy_header(header: &ProxyHeader) {
    println!("\n=== PROXY protocol header ===");
    match header {
        ProxyHeader::Version1 { addresses } => match addresses {
            wire_proxy_protocol::version1::ProxyAddresses::Unknown => {
                println!("Version 1: addresses unknown");
            }
            wire_proxy_protocol::version1::ProxyAddresses::Ipv4 {
                source,
                destination,
            } => {
                println!("Version 1: source {}, destination {}", source, destination);
            }
            wire_proxy_protocol::version1::ProxyAddresses::Ipv6 {
                source,
                destination,
            } => {
                println!("Version 1: source {}, destination {}", source, destination);
            }
        },
        ProxyHeader::Version2 {
            command,
            transport_protocol,
            addresses,
            extensions,
        } => {
            println!(
                "Version 2: command = {:?}, transport = {:?}",
                command, transport_protocol
            );
            match addresses {
                wire_proxy_protocol::version2::ProxyAddresses::Unspec => {
                    println!("Addresses: unspecified");
                }
                wire_proxy_protocol::version2::ProxyAddresses::Ipv4 {
                    source,
                    destination,
                } => {
                    println!("Addresses: source {}, destination {}", source, destination);
                }
                wire_proxy_protocol::version2::ProxyAddresses::Ipv6 {
                    source,
                    destination,
                } => {
                    println!("Addresses: source {}, destination {}", source, destination);
                }
                wire_proxy_protocol::version2::ProxyAddresses::Unix {
                    source,
                    destination,
                } => {
                    let fmt_source = render_unix_address(source);
                    let fmt_destination = render_unix_address(destination);
                    println!(
                        "Addresses: source {}, destination {}",
                        fmt_source, fmt_destination
                    );
                }
            }
            if !extensions.is_empty() {
                println!("Extensions ({} entries) present", extensions.len());
            }
        }
        #[allow(unreachable_patterns)]
        _ => println!("Unsupported PROXY protocol version"),
    }
    println!("=============================\n");
}

fn render_unix_address(raw: &[u8; 108]) -> String {
    let end = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
    String::from_utf8_lossy(&raw[..end]).into_owned()
}
