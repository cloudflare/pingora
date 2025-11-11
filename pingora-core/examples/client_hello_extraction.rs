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

use bytes::Bytes;
use pingora_core::protocols::l4::stream::Stream;
use pingora_core::protocols::ClientHelloWrapper;
use proxy_protocol::ProxyHeader;
use std::{io, net::SocketAddr};
use tokio::io::AsyncReadExt;
use tokio::net::{TcpListener, TcpStream};

const PROXY_V2_SIGNATURE: [u8; 12] = [
    0x0D, 0x0A, 0x0D, 0x0A, 0x00, 0x0D, 0x0A, 0x51, 0x55, 0x49, 0x54, 0x0A,
];
const MAX_PROXY_V1_HEADER_LEN: usize = 108;
const MAX_PROXY_HEADER_LEN: usize = 65535 + 16;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::init();

    println!("Starting TLS ClientHello extraction example...");
    println!("Listening on 127.0.0.1:8443");
    println!("\nTo test, use:");
    println!(
        "  openssl s_client -connect 127.0.0.1:8443 -servername example.com -alpn h2,http/1.1"
    );
    println!();

    println!("\nTo test proxy requests:");
    println!("  printf 'PROXY TCP4 203.0.113.10 198.51.100.5 54321 443\r\n' | socat - TCP:127.0.0.1:8443");

    let listener = TcpListener::bind("127.0.0.1:8443").await?;

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

async fn handle_connection(mut tcp_stream: TcpStream, addr: SocketAddr) -> io::Result<()> {
    let proxy_header = consume_proxy_header(&mut tcp_stream).await?;

    println!("Consumed proxy header");

    if let Some(header) = &proxy_header {
        log_proxy_header(header);
    } else {
        println!("No PROXY header detected; using peer {}", addr);
    }

    // Convert to Pingora Stream
    let stream: Stream = tcp_stream.into();

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
            proxy_protocol::version1::ProxyAddresses::Unknown => {
                println!("Version 1: addresses unknown");
            }
            proxy_protocol::version1::ProxyAddresses::Ipv4 {
                source,
                destination,
            } => {
                println!("Version 1: source {}, destination {}", source, destination);
            }
            proxy_protocol::version1::ProxyAddresses::Ipv6 {
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
                proxy_protocol::version2::ProxyAddresses::Unspec => {
                    println!("Addresses: unspecified");
                }
                proxy_protocol::version2::ProxyAddresses::Ipv4 {
                    source,
                    destination,
                } => {
                    println!("Addresses: source {}, destination {}", source, destination);
                }
                proxy_protocol::version2::ProxyAddresses::Ipv6 {
                    source,
                    destination,
                } => {
                    println!("Addresses: source {}, destination {}", source, destination);
                }
                proxy_protocol::version2::ProxyAddresses::Unix {
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

async fn consume_proxy_header(stream: &mut TcpStream) -> io::Result<Option<ProxyHeader>> {
    let mut peek_buf = vec![0u8; MAX_PROXY_HEADER_LEN];

    loop {
        stream.readable().await?;

        let n = match stream.peek(&mut peek_buf).await {
            Ok(n) => n,
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => continue,
            Err(err) => return Err(err),
        };

        if n == 0 {
            return Ok(None);
        }

        match detect_proxy_header(&peek_buf[..n]) {
            ProxyDetection::NotProxy => return Ok(None),
            ProxyDetection::NeedsMore => {
                if n == peek_buf.len() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("PROXY header exceeds {} bytes", MAX_PROXY_HEADER_LEN),
                    ));
                }
                continue;
            }
            ProxyDetection::Invalid => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Malformed PROXY protocol header",
                ));
            }
            ProxyDetection::HeaderLength(header_len) => {
                let mut header_bytes = vec![0u8; header_len];
                stream.read_exact(&mut header_bytes).await?;
                let mut bytes = Bytes::from(header_bytes);
                let header = proxy_protocol::parse(&mut bytes)
                    .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
                return Ok(Some(header));
            }
        }
    }
}

enum ProxyDetection {
    NotProxy,
    NeedsMore,
    Invalid,
    HeaderLength(usize),
}

fn detect_proxy_header(data: &[u8]) -> ProxyDetection {
    if data.is_empty() {
        return ProxyDetection::NeedsMore;
    }

    if data[0] == b'P' {
        if data.len() < b"PROXY".len() {
            return ProxyDetection::NeedsMore;
        }
        if !data.starts_with(b"PROXY") {
            return ProxyDetection::NotProxy;
        }
        if let Some(pos) = data.windows(2).position(|window| window == b"\r\n") {
            let header_len = pos + 2;
            if header_len > MAX_PROXY_V1_HEADER_LEN {
                return ProxyDetection::Invalid;
            }
            return ProxyDetection::HeaderLength(header_len);
        }
        if data.len() >= MAX_PROXY_V1_HEADER_LEN {
            return ProxyDetection::Invalid;
        }
        return ProxyDetection::NeedsMore;
    }

    if data[0] == PROXY_V2_SIGNATURE[0] {
        if data.len() < PROXY_V2_SIGNATURE.len() {
            return ProxyDetection::NeedsMore;
        }
        if data[..PROXY_V2_SIGNATURE.len()] != PROXY_V2_SIGNATURE {
            return ProxyDetection::NotProxy;
        }
        if data.len() < 16 {
            return ProxyDetection::NeedsMore;
        }
        let len = u16::from_be_bytes([data[14], data[15]]) as usize;
        let total_len = 16 + len;
        if total_len > MAX_PROXY_HEADER_LEN {
            return ProxyDetection::Invalid;
        }
        if data.len() < total_len {
            return ProxyDetection::NeedsMore;
        }
        return ProxyDetection::HeaderLength(total_len);
    }

    ProxyDetection::NotProxy
}
