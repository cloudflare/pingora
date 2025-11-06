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

//! TLS ClientHello parsing using MSG_PEEK

use log::{debug, warn};
use std::io;

const TLS_CONTENT_TYPE_HANDSHAKE: u8 = 0x16;
const TLS_HANDSHAKE_TYPE_CLIENT_HELLO: u8 = 0x01;

// TLS extensions
const EXT_SERVER_NAME: u16 = 0x0000;
const EXT_ALPN: u16 = 0x0010;

/// Parsed ClientHello information
#[derive(Debug, Clone, Default)]
pub struct ClientHello {
    /// Server Name Indication (SNI) from the ClientHello
    pub sni: Option<String>,
    /// Application-Layer Protocol Negotiation (ALPN) protocols
    pub alpn: Vec<String>,
    /// TLS version from ClientHello
    pub tls_version: Option<u16>,
    /// Raw ClientHello bytes
    pub raw: Vec<u8>,
}

impl ClientHello {
    /// Parse ClientHello from raw bytes
    pub fn parse(data: &[u8]) -> Option<Self> {
        if data.len() < 5 {
            return None;
        }

        // Check TLS record header
        if data[0] != TLS_CONTENT_TYPE_HANDSHAKE {
            debug!("Not a TLS handshake record: {:02x}", data[0]);
            return None;
        }

        // TLS version (2 bytes)
        let tls_version = u16::from_be_bytes([data[1], data[2]]);

        // Record length (2 bytes)
        let record_len = u16::from_be_bytes([data[3], data[4]]) as usize;

        if data.len() < 5 + record_len {
            debug!(
                "Incomplete TLS record: have {} bytes, need {}",
                data.len(),
                5 + record_len
            );
            return None;
        }

        let record_data = &data[5..5 + record_len];
        if record_data.is_empty() {
            return None;
        }

        // Check handshake type
        if record_data[0] != TLS_HANDSHAKE_TYPE_CLIENT_HELLO {
            debug!("Not a ClientHello: {:02x}", record_data[0]);
            return None;
        }

        let mut hello = ClientHello {
            tls_version: Some(tls_version),
            raw: data[..5 + record_len].to_vec(),
            ..Default::default()
        };

        // Parse ClientHello structure
        if let Some((sni, alpn)) = Self::parse_extensions(record_data) {
            hello.sni = sni;
            hello.alpn = alpn;
        }

        Some(hello)
    }

    fn parse_extensions(data: &[u8]) -> Option<(Option<String>, Vec<String>)> {
        // Skip:
        // - Handshake type (1 byte)
        // - Handshake length (3 bytes)
        // - Client version (2 bytes)
        // - Random (32 bytes)
        if data.len() < 38 {
            return None;
        }

        let mut pos = 38;

        // Session ID length (1 byte)
        if pos >= data.len() {
            return None;
        }
        let session_id_len = data[pos] as usize;
        pos += 1 + session_id_len;

        // Cipher suites length (2 bytes)
        if pos + 2 > data.len() {
            return None;
        }
        let cipher_suites_len = u16::from_be_bytes([data[pos], data[pos + 1]]) as usize;
        pos += 2 + cipher_suites_len;

        // Compression methods length (1 byte)
        if pos >= data.len() {
            return None;
        }
        let compression_len = data[pos] as usize;
        pos += 1 + compression_len;

        // Extensions length (2 bytes)
        if pos + 2 > data.len() {
            return None;
        }
        let extensions_len = u16::from_be_bytes([data[pos], data[pos + 1]]) as usize;
        pos += 2;

        if pos + extensions_len > data.len() {
            return None;
        }

        let extensions_end = pos + extensions_len;
        let mut sni = None;
        let mut alpn = Vec::new();

        // Parse extensions
        while pos + 4 <= extensions_end {
            let ext_type = u16::from_be_bytes([data[pos], data[pos + 1]]);
            let ext_len = u16::from_be_bytes([data[pos + 2], data[pos + 3]]) as usize;
            pos += 4;

            if pos + ext_len > extensions_end {
                break;
            }

            match ext_type {
                EXT_SERVER_NAME => {
                    if let Some(name) = Self::parse_sni(&data[pos..pos + ext_len]) {
                        sni = Some(name);
                    }
                }
                EXT_ALPN => {
                    alpn = Self::parse_alpn(&data[pos..pos + ext_len]);
                }
                _ => {}
            }

            pos += ext_len;
        }

        Some((sni, alpn))
    }

    fn parse_sni(data: &[u8]) -> Option<String> {
        if data.len() < 5 {
            return None;
        }

        // Server Name List Length (2 bytes)
        let list_len = u16::from_be_bytes([data[0], data[1]]) as usize;
        if data.len() < 2 + list_len {
            return None;
        }

        // Server Name Type (1 byte) - should be 0 for hostname
        if data[2] != 0 {
            return None;
        }

        // Server Name Length (2 bytes)
        let name_len = u16::from_be_bytes([data[3], data[4]]) as usize;
        if data.len() < 5 + name_len {
            return None;
        }

        // Server Name
        String::from_utf8(data[5..5 + name_len].to_vec()).ok()
    }

    fn parse_alpn(data: &[u8]) -> Vec<String> {
        if data.len() < 2 {
            return Vec::new();
        }

        // ALPN Extension Data Length (2 bytes)
        let list_len = u16::from_be_bytes([data[0], data[1]]) as usize;
        if data.len() < 2 + list_len {
            return Vec::new();
        }

        let mut pos = 2;
        let end = 2 + list_len;
        let mut protocols = Vec::new();

        while pos < end {
            if pos >= data.len() {
                break;
            }
            let proto_len = data[pos] as usize;
            pos += 1;

            if pos + proto_len > data.len() || pos + proto_len > end {
                break;
            }

            if let Ok(proto) = String::from_utf8(data[pos..pos + proto_len].to_vec()) {
                protocols.push(proto);
            }
            pos += proto_len;
        }

        protocols
    }
}

#[cfg(unix)]
use std::os::unix::io::AsRawFd;

/// Peek at the ClientHello from a TCP stream using MSG_PEEK
#[cfg(unix)]
pub fn peek_client_hello<S: AsRawFd>(stream: &S) -> io::Result<Option<ClientHello>> {
    use nix::sys::socket::{recv, MsgFlags};

    // Maximum size for a TLS record is 16KB + 5 bytes header
    // We allocate enough space to capture most ClientHello messages
    let mut buf = vec![0u8; 16384 + 5];

    match recv(stream.as_raw_fd(), &mut buf, MsgFlags::MSG_PEEK) {
        Ok(size) => {
            if size == 0 {
                return Ok(None);
            }

            debug!("Peeked {} bytes from socket", size);
            Ok(ClientHello::parse(&buf[..size]))
        }
        Err(e) => {
            warn!("Failed to peek at socket: {:?}", e);
            Err(e.into())
        }
    }
}

#[cfg(windows)]
pub fn peek_client_hello<S>(_stream: &S) -> io::Result<Option<ClientHello>> {
    // Windows implementation would use WSARecv with MSG_PEEK
    // For now, return None to indicate feature not supported
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_client_hello_with_sni() {
        // A minimal ClientHello with SNI extension for "example.com" (11 bytes)
        // SNI extension data: List Len (2) + Type (1) + Name Len (2) + Name (11) = 16 bytes
        // Full SNI ext: Type (2) + Len (2) + Data (16) = 20 bytes
        // Extensions: Len (2) + SNI (20) = 22 bytes
        // ClientHello: Version (2) + Random (32) + SessionID (1+0) + CipherSuites (2+4) + Compression (1+1) + Extensions (22) = 65 bytes
        // Handshake: Type (1) + Len (3) + ClientHello (65) = 69 bytes
        let data = vec![
            0x16, // Content Type: Handshake
            0x03, 0x01, // Version: TLS 1.0
            0x00, 0x45, // Record Length (69 bytes)
            0x01, // Handshake Type: ClientHello
            0x00, 0x00, 0x41, // Handshake Length (65 bytes)
            0x03, 0x03, // Client Version: TLS 1.2
            // Random (32 bytes)
            0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07,
            0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f,
            0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17,
            0x18, 0x19, 0x1a, 0x1b, 0x1c, 0x1d, 0x1e, 0x1f,
            0x00, // Session ID Length
            0x00, 0x04, // Cipher Suites Length (4 bytes = 2 cipher suites)
            0x00, 0x2f, 0x00, 0x35, // Cipher suites
            0x01, // Compression Methods Length
            0x00, // Compression method: null
            0x00, 0x14, // Extensions Length (20 bytes = one full SNI extension)
            // SNI Extension
            0x00, 0x00, // Extension Type: server_name (0)
            0x00, 0x10, // Extension Length (16 bytes)
            0x00, 0x0e, // Server Name List Length (14 bytes)
            0x00, // Server Name Type: host_name (0)
            0x00, 0x0b, // Server Name Length (11 bytes)
            // "example.com" (11 bytes)
            0x65, 0x78, 0x61, 0x6d, 0x70, 0x6c, 0x65, 0x2e, 0x63, 0x6f, 0x6d,
        ];

        let hello = ClientHello::parse(&data).expect("Failed to parse ClientHello");
        assert_eq!(hello.sni, Some("example.com".to_string()));
        assert_eq!(hello.tls_version, Some(0x0301));
    }

    #[test]
    fn test_parse_client_hello_with_alpn() {
        // Minimal ClientHello with ALPN
        // Record = 1 (type) + 3 (len) + 2 (version) + 32 (random) + 1 (session) + 2 (cipher len) + 2 (cipher) + 1 (comp len) + 1 (comp) + 2 (ext len) + 18 (ext) = 65 bytes
        let data = vec![
            0x16, // Content Type: Handshake
            0x03, 0x03, // Version: TLS 1.2
            0x00, 0x41, // Record Length (65 bytes - handshake data)
            0x01, // Handshake Type: ClientHello
            0x00, 0x00, 0x3d, // Handshake Length (61 bytes)
            0x03, 0x03, // Client Version: TLS 1.2
            // Random (32 bytes)
            0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07,
            0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f,
            0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17,
            0x18, 0x19, 0x1a, 0x1b, 0x1c, 0x1d, 0x1e, 0x1f,
            0x00, // Session ID Length
            0x00, 0x02, // Cipher Suites Length (2 bytes = 1 cipher suite)
            0x00, 0x2f, // Cipher suite
            0x01, // Compression Methods Length
            0x00, // Compression method: null
            0x00, 0x12, // Extensions Length (18 bytes)
            // ALPN Extension
            0x00, 0x10, // Extension Type: ALPN
            0x00, 0x0e, // Extension Length (14 bytes)
            0x00, 0x0c, // ALPN Extension Length (12 bytes)
            0x02, 0x68, 0x32, // Length prefix (2) + "h2"
            0x08, 0x68, 0x74, 0x74, 0x70, 0x2f, 0x31, 0x2e, 0x31, // Length prefix (8) + "http/1.1"
        ];

        let hello = ClientHello::parse(&data).expect("Failed to parse ClientHello");
        assert_eq!(hello.alpn, vec!["h2", "http/1.1"]);
    }

    #[test]
    fn test_parse_invalid_data() {
        let data = vec![0x00, 0x01, 0x02]; // Not a valid TLS record
        assert!(ClientHello::parse(&data).is_none());
    }

    #[test]
    fn test_parse_non_handshake() {
        let data = vec![
            0x17, // Content Type: Application Data (not handshake)
            0x03, 0x03,
            0x00, 0x10,
        ];
        assert!(ClientHello::parse(&data).is_none());
    }
}

