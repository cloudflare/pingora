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

//! Utilities for consuming the HAProxy PROXY protocol preface from transport streams.

use bytes::Bytes;
use log::{debug, trace};
use pingora_error::{Error, ErrorType, OrErr, Result};
use tokio::io::AsyncReadExt;

use crate::protocols::l4::stream::Stream;

/// Re-export the parsed header type from the underlying `proxy_protocol` crate for convenience.
pub use proxy_protocol::ProxyHeader;

/// Maximum number of bytes a PROXY protocol v1 header can occupy, including CRLF.
pub const MAX_PROXY_V1_HEADER_LEN: usize = 108;
/// Maximum number of bytes a PROXY protocol v2 header can occupy (16 bytes header + 64k body).
pub const MAX_PROXY_HEADER_LEN: usize = 16 + 65_535;

const PROXY_V2_SIGNATURE: [u8; 12] = [
    0x0D, 0x0A, 0x0D, 0x0A, 0x00, 0x0D, 0x0A, 0x51, 0x55, 0x49, 0x54, 0x0A,
];

/// Error type used when a PROXY protocol header is malformed or exceeds limits.
const PROXY_PROTOCOL_ERROR: ErrorType = ErrorType::Custom("ProxyProtocolError");

/// Detection result when examining a byte buffer for a PROXY protocol prefix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProxyDetection {
    /// Buffer does not start with a PROXY protocol preface.
    NotProxy,
    /// More bytes are needed to determine if this is a PROXY header.
    NeedsMore,
    /// Buffer matches a PROXY preface but is malformed.
    Invalid,
    /// Buffer contains an entire PROXY header of the specified length.
    HeaderLength(usize),
}

/// Inspect the provided data slice to determine whether it starts with a PROXY protocol header.
pub fn detect_proxy_header(data: &[u8]) -> ProxyDetection {
    if data.is_empty() {
        return ProxyDetection::NeedsMore;
    }

    match data[0] {
        b'P' => detect_v1(data),
        sig if sig == PROXY_V2_SIGNATURE[0] => detect_v2(data),
        _ => ProxyDetection::NotProxy,
    }
}

fn detect_v1(data: &[u8]) -> ProxyDetection {
    const PREFIX: &[u8] = b"PROXY";

    if data.len() < PREFIX.len() {
        return ProxyDetection::NeedsMore;
    }
    if !data.starts_with(PREFIX) {
        return ProxyDetection::NotProxy;
    }

    // Search for CRLF terminator.
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

    ProxyDetection::NeedsMore
}

fn detect_v2(data: &[u8]) -> ProxyDetection {
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

    ProxyDetection::HeaderLength(total_len)
}

/// Consume and parse a PROXY protocol header from the provided transport stream if present.
///
/// If the stream does not start with a PROXY header, the bytes that were read for detection are
/// rewound so subsequent consumers observe the original payload.
///
/// # Errors
/// Returns an error if a malformed PROXY header is encountered or IO errors occur.
pub async fn consume_proxy_header(stream: &mut Stream) -> Result<Option<ProxyHeader>> {
    let mut buffer = Vec::with_capacity(128);
    let mut chunk = [0u8; 512];

    loop {
        if buffer.len() > MAX_PROXY_HEADER_LEN {
            debug!(
                "PROXY header exceeded limit ({} bytes)",
                MAX_PROXY_HEADER_LEN
            );
            return Error::e_explain(
                PROXY_PROTOCOL_ERROR,
                format!("PROXY header exceeds {MAX_PROXY_HEADER_LEN} bytes"),
            );
        }

        let read = stream
            .read(&mut chunk)
            .await
            .or_err(ErrorType::ReadError, "while reading PROXY header probe")?;

        if read == 0 {
            if buffer.is_empty() {
                trace!("Stream EOF with no PROXY preface detected");
                return Ok(None);
            } else {
                debug!("Stream closed while reading PROXY header");
                return Error::e_explain(PROXY_PROTOCOL_ERROR, "Incomplete PROXY protocol header");
            }
        }

        buffer.extend_from_slice(&chunk[..read]);
        match detect_proxy_header(&buffer) {
            ProxyDetection::NotProxy => {
                trace!("Stream does not use PROXY protocol");
                stream.rewind(&buffer);
                return Ok(None);
            }
            ProxyDetection::NeedsMore => {
                continue;
            }
            ProxyDetection::Invalid => {
                debug!("Malformed PROXY protocol header detected");
                return Error::e_explain(PROXY_PROTOCOL_ERROR, "Malformed PROXY protocol header");
            }
            ProxyDetection::HeaderLength(header_len) => {
                debug!("Detected PROXY header of length {header_len}");

                let mut header_bytes = buffer;
                let leftover = header_bytes.split_off(header_len);
                if !leftover.is_empty() {
                    stream.rewind(&leftover);
                }

                let mut bytes = Bytes::from(header_bytes);
                let header = proxy_protocol::parse(&mut bytes).map_err(|err| {
                    Error::because(
                        PROXY_PROTOCOL_ERROR,
                        "Failed to parse PROXY protocol header",
                        err,
                    )
                })?;

                return Ok(Some(header));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocols::l4::stream::Stream;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    #[test]
    fn detect_v1_header_len() {
        let header = b"PROXY TCP4 203.0.113.10 198.51.100.5 54321 443\r\n";
        assert_eq!(
            detect_proxy_header(header),
            ProxyDetection::HeaderLength(header.len())
        );
    }

    #[test]
    fn detect_v2_header_len() {
        let mut header = Vec::from(PROXY_V2_SIGNATURE);
        // version 2 header: ver/cmd, fam, len, address (ipv4) + ports
        header.extend_from_slice(&[0x21, 0x11]);
        header.extend_from_slice(&[0x00, 0x0c]);
        header.extend_from_slice(&[203, 0, 113, 10, 198, 51, 100, 5, 0xD4, 0x31, 0x01, 0xBB]);
        assert_eq!(
            detect_proxy_header(&header),
            ProxyDetection::HeaderLength(header.len())
        );
    }

    #[tokio::test]
    async fn consume_proxy_header_v1() {
        use proxy_protocol::version1::ProxyAddresses;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let writer = tokio::spawn(async move {
            let mut client = tokio::net::TcpStream::connect(addr).await.unwrap();
            client
                .write_all(b"PROXY TCP4 203.0.113.10 198.51.100.5 54321 443\r\nHELLO")
                .await
                .unwrap();
        });

        let (socket, _) = listener.accept().await.unwrap();
        let mut stream: Stream = socket.into();

        let header = consume_proxy_header(&mut stream)
            .await
            .expect("proxy header parse should succeed")
            .expect("header should exist");

        if let ProxyHeader::Version1 { addresses } = header {
            if let ProxyAddresses::Ipv4 {
                source,
                destination,
            } = addresses
            {
                assert_eq!(source.ip().octets(), [203, 0, 113, 10]);
                assert_eq!(destination.ip().octets(), [198, 51, 100, 5]);
                assert_eq!(source.port(), 54321);
                assert_eq!(destination.port(), 443);
            } else {
                panic!("expected IPv4 addresses");
            }
        } else {
            panic!("expected v1 header");
        }

        let mut remaining = String::new();
        stream.read_to_string(&mut remaining).await.unwrap();
        assert_eq!(remaining, "HELLO");

        writer.await.unwrap();
    }
}
