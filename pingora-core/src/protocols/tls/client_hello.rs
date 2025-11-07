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

// TLS extensions (RFC 8446, RFC 6066, etc.)
const EXT_SERVER_NAME: u16 = 0x0000;
const EXT_SUPPORTED_GROUPS: u16 = 0x000a; // RFC 4492
const EXT_SIGNATURE_ALGORITHMS: u16 = 0x000d; // RFC 5246
const EXT_ALPN: u16 = 0x0010;
const EXT_SESSION_TICKET: u16 = 0x0023;
const EXT_KEY_SHARE: u16 = 0x0033; // TLS 1.3
const EXT_PRE_SHARED_KEY: u16 = 0x0029; // TLS 1.3
const EXT_PSK_KEY_EXCHANGE_MODES: u16 = 0x002d; // TLS 1.3
const EXT_SUPPORTED_VERSIONS: u16 = 0x002b; // TLS 1.3
const EXT_COOKIE: u16 = 0x002c; // TLS 1.3

/// Supported elliptic curve groups
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NamedGroup {
    /// secp256r1 (P-256)
    Secp256r1,
    /// secp384r1 (P-384)
    Secp384r1,
    /// secp521r1 (P-521)
    Secp521r1,
    /// x25519
    X25519,
    /// x448
    X448,
    /// ffdhe2048
    Ffdhe2048,
    /// ffdhe3072
    Ffdhe3072,
    /// ffdhe4096
    Ffdhe4096,
    /// ffdhe6144
    Ffdhe6144,
    /// ffdhe8192
    Ffdhe8192,
    /// Unknown group
    Unknown(u16),
}

impl NamedGroup {
    fn from_u16(value: u16) -> Self {
        match value {
            0x0017 => NamedGroup::Secp256r1,
            0x0018 => NamedGroup::Secp384r1,
            0x0019 => NamedGroup::Secp521r1,
            0x001d => NamedGroup::X25519,
            0x001e => NamedGroup::X448,
            0x0100 => NamedGroup::Ffdhe2048,
            0x0101 => NamedGroup::Ffdhe3072,
            0x0102 => NamedGroup::Ffdhe4096,
            0x0103 => NamedGroup::Ffdhe6144,
            0x0104 => NamedGroup::Ffdhe8192,
            _ => NamedGroup::Unknown(value),
        }
    }
}

/// Signature algorithm
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignatureAlgorithm {
    /// Hash algorithm
    pub hash: u8,
    /// Signature algorithm
    pub signature: u8,
}

/// TLS 1.3 specific extensions
#[derive(Debug, Clone, Default)]
pub struct Tls13Extensions {
    /// Supported versions (TLS 1.3)
    pub supported_versions: Vec<u16>,
    /// Key share groups (TLS 1.3)
    pub key_share_groups: Vec<NamedGroup>,
    /// PSK key exchange modes (TLS 1.3)
    pub psk_key_exchange_modes: Vec<u8>,
    /// Cookie (TLS 1.3)
    pub cookie: Option<Vec<u8>>,
    /// Pre-shared key identities (TLS 1.3)
    pub psk_identities: Vec<Vec<u8>>,
}

/// Parsed ClientHello information
#[derive(Debug, Clone, Default)]
pub struct ClientHello {
    /// Server Name Indication (SNI) from the ClientHello
    pub sni: Option<String>,
    /// Application-Layer Protocol Negotiation (ALPN) protocols
    pub alpn: Vec<String>,
    /// TLS version from ClientHello
    pub tls_version: Option<u16>,
    /// Supported elliptic curve groups
    pub supported_groups: Vec<NamedGroup>,
    /// Signature algorithms
    pub signature_algorithms: Vec<SignatureAlgorithm>,
    /// Session ticket (if present)
    pub session_ticket: Option<Vec<u8>>,
    /// TLS 1.3 specific extensions
    pub tls13_extensions: Option<Tls13Extensions>,
    /// Raw extension data (for extensions we don't parse)
    pub raw_extensions: std::collections::HashMap<u16, Vec<u8>>,
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
            raw_extensions: std::collections::HashMap::new(),
            ..Default::default()
        };

        // Parse ClientHello structure
        Self::parse_extensions(record_data, &mut hello);

        Some(hello)
    }

    fn parse_extensions(data: &[u8], hello: &mut ClientHello) {
        // Skip:
        // - Handshake type (1 byte)
        // - Handshake length (3 bytes)
        // - Client version (2 bytes)
        // - Random (32 bytes)
        if data.len() < 38 {
            return;
        }

        let mut pos = 38;

        // Session ID length (1 byte)
        if pos >= data.len() {
            return;
        }
        let session_id_len = data[pos] as usize;
        pos += 1 + session_id_len;

        // Cipher suites length (2 bytes)
        if pos + 2 > data.len() {
            return;
        }
        let cipher_suites_len = u16::from_be_bytes([data[pos], data[pos + 1]]) as usize;
        pos += 2 + cipher_suites_len;

        // Compression methods length (1 byte)
        if pos >= data.len() {
            return;
        }
        let compression_len = data[pos] as usize;
        pos += 1 + compression_len;

        // Extensions length (2 bytes)
        if pos + 2 > data.len() {
            return;
        }
        let extensions_len = u16::from_be_bytes([data[pos], data[pos + 1]]) as usize;
        pos += 2;

        if pos + extensions_len > data.len() {
            return;
        }

        let extensions_end = pos + extensions_len;
        let mut tls13_ext = Tls13Extensions::default();
        let mut is_tls13 = false;

        // Check if this is TLS 1.3 by looking for supported_versions extension
        // We'll determine this during parsing

        // Parse extensions
        while pos + 4 <= extensions_end {
            let ext_type = u16::from_be_bytes([data[pos], data[pos + 1]]);
            let ext_len = u16::from_be_bytes([data[pos + 2], data[pos + 3]]) as usize;
            pos += 4;

            if pos + ext_len > extensions_end {
                break;
            }

            let ext_data = &data[pos..pos + ext_len];

            match ext_type {
                EXT_SERVER_NAME => {
                    if let Some(name) = Self::parse_sni(ext_data) {
                        hello.sni = Some(name);
                    }
                }
                EXT_ALPN => {
                    hello.alpn = Self::parse_alpn(ext_data);
                }
                EXT_SUPPORTED_GROUPS => {
                    hello.supported_groups = Self::parse_supported_groups(ext_data);
                }
                EXT_SIGNATURE_ALGORITHMS => {
                    hello.signature_algorithms = Self::parse_signature_algorithms(ext_data);
                }
                EXT_SESSION_TICKET => {
                    hello.session_ticket = Some(ext_data.to_vec());
                }
                EXT_SUPPORTED_VERSIONS => {
                    // TLS 1.3 extension
                    is_tls13 = true;
                    tls13_ext.supported_versions = Self::parse_supported_versions(ext_data);
                }
                EXT_KEY_SHARE => {
                    // TLS 1.3 extension
                    is_tls13 = true;
                    tls13_ext.key_share_groups = Self::parse_key_share(ext_data);
                }
                EXT_PSK_KEY_EXCHANGE_MODES => {
                    // TLS 1.3 extension
                    is_tls13 = true;
                    tls13_ext.psk_key_exchange_modes = Self::parse_psk_key_exchange_modes(ext_data);
                }
                EXT_COOKIE => {
                    // TLS 1.3 extension
                    is_tls13 = true;
                    tls13_ext.cookie = Some(ext_data.to_vec());
                }
                EXT_PRE_SHARED_KEY => {
                    // TLS 1.3 extension
                    is_tls13 = true;
                    tls13_ext.psk_identities = Self::parse_pre_shared_key(ext_data);
                }
                _ => {
                    // Store raw extension data for unknown extensions
                    hello.raw_extensions.insert(ext_type, ext_data.to_vec());
                }
            }

            pos += ext_len;
        }

        if is_tls13 {
            hello.tls13_extensions = Some(tls13_ext);
        }
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

    fn parse_supported_groups(data: &[u8]) -> Vec<NamedGroup> {
        if data.len() < 2 {
            return Vec::new();
        }

        let list_len = u16::from_be_bytes([data[0], data[1]]) as usize;
        if data.len() < 2 + list_len {
            return Vec::new();
        }

        let mut pos = 2;
        let end = 2 + list_len;
        let mut groups = Vec::new();

        while pos + 2 <= end && pos + 2 <= data.len() {
            let group_id = u16::from_be_bytes([data[pos], data[pos + 1]]);
            groups.push(NamedGroup::from_u16(group_id));
            pos += 2;
        }

        groups
    }

    fn parse_signature_algorithms(data: &[u8]) -> Vec<SignatureAlgorithm> {
        if data.len() < 2 {
            return Vec::new();
        }

        let list_len = u16::from_be_bytes([data[0], data[1]]) as usize;
        if data.len() < 2 + list_len {
            return Vec::new();
        }

        let mut pos = 2;
        let end = 2 + list_len;
        let mut algorithms = Vec::new();

        while pos + 2 <= end && pos + 2 <= data.len() {
            algorithms.push(SignatureAlgorithm {
                hash: data[pos],
                signature: data[pos + 1],
            });
            pos += 2;
        }

        algorithms
    }

    fn parse_supported_versions(data: &[u8]) -> Vec<u16> {
        if data.is_empty() {
            return Vec::new();
        }

        let versions_len = data[0] as usize;
        if data.len() < 1 + versions_len {
            return Vec::new();
        }

        let mut pos = 1;
        let end = 1 + versions_len;
        let mut versions = Vec::new();

        while pos + 2 <= end && pos + 2 <= data.len() {
            let version = u16::from_be_bytes([data[pos], data[pos + 1]]);
            versions.push(version);
            pos += 2;
        }

        versions
    }

    fn parse_key_share(data: &[u8]) -> Vec<NamedGroup> {
        if data.len() < 2 {
            return Vec::new();
        }

        let list_len = u16::from_be_bytes([data[0], data[1]]) as usize;
        if data.len() < 2 + list_len {
            return Vec::new();
        }

        let mut pos = 2;
        let end = 2 + list_len;
        let mut groups = Vec::new();

        while pos + 4 <= end && pos + 4 <= data.len() {
            let group_id = u16::from_be_bytes([data[pos], data[pos + 1]]);
            let key_exchange_len = u16::from_be_bytes([data[pos + 2], data[pos + 3]]) as usize;
            groups.push(NamedGroup::from_u16(group_id));
            pos += 4 + key_exchange_len;
        }

        groups
    }

    fn parse_psk_key_exchange_modes(data: &[u8]) -> Vec<u8> {
        if data.is_empty() {
            return Vec::new();
        }

        let modes_len = data[0] as usize;
        if data.len() < 1 + modes_len {
            return Vec::new();
        }

        data[1..1 + modes_len].to_vec()
    }

    fn parse_pre_shared_key(data: &[u8]) -> Vec<Vec<u8>> {
        if data.len() < 2 {
            return Vec::new();
        }

        let identities_len = u16::from_be_bytes([data[0], data[1]]) as usize;
        if data.len() < 2 + identities_len {
            return Vec::new();
        }

        let mut pos = 2;
        let end = 2 + identities_len;
        let mut identities = Vec::new();

        while pos + 2 <= end && pos + 2 <= data.len() {
            let identity_len = u16::from_be_bytes([data[pos], data[pos + 1]]) as usize;
            pos += 2;

            if pos + identity_len > end || pos + identity_len > data.len() {
                break;
            }

            identities.push(data[pos..pos + identity_len].to_vec());
            pos += identity_len;

            // Skip obfuscated_ticket_age (4 bytes)
            if pos + 4 <= end && pos + 4 <= data.len() {
                pos += 4;
            } else {
                break;
            }
        }

        identities
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

    #[test]
    fn test_parse_supported_groups() {
        // ClientHello with supported_groups extension
        // Extension: Type(2) + Len(2) + Data(8) = 12 bytes
        //   Data: Groups List Len(2) + Groups(6) = 8 bytes
        // Extensions: Len(2) + Extension(12) = 14 bytes
        // But parse_extensions needs 63 bytes (pos 47 + extensions_len 16), so we need 2 more bytes
        // Extensions: Len(2) + Extension(12) + Padding(2) = 16 bytes
        // Handshake body: Version(2) + Random(32) + SessionID(1) + CipherSuites(4) + Compression(2) + Extensions(16) = 57 bytes
        // But we need 63 bytes total, so add 2 more bytes to handshake body = 59 bytes
        // Handshake: Type(1) + Len(3) + Body(59) = 63 bytes
        // Record: Header(5) + Handshake(63) = 68 bytes
        let data = vec![
            0x16, // Content Type: Handshake
            0x03, 0x03, // Version: TLS 1.2
            0x00, 0x3f, // Record Length (63 bytes = handshake data)
            0x01, // Handshake Type: ClientHello
            0x00, 0x00, 0x3b, // Handshake Length (59 bytes = body with padding)
            0x03, 0x03, // Client Version: TLS 1.2
            // Random (32 bytes)
            0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07,
            0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f,
            0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17,
            0x18, 0x19, 0x1a, 0x1b, 0x1c, 0x1d, 0x1e, 0x1f,
            0x00, // Session ID Length
            0x00, 0x02, // Cipher Suites Length (2 bytes)
            0x00, 0x2f, // Cipher suite
            0x01, // Compression Methods Length
            0x00, // Compression method: null
            0x00, 0x10, // Extensions Length (16 bytes = 14 + 2 padding)
            // Supported Groups Extension
            0x00, 0x0a, // Extension Type: supported_groups
            0x00, 0x08, // Extension Length (8 bytes)
            0x00, 0x06, // Groups List Length (6 bytes = 3 groups)
            0x00, 0x17, // secp256r1
            0x00, 0x18, // secp384r1
            0x00, 0x1d, // x25519
            0x00, 0x00, // Padding (2 bytes)
            0x00, 0x00, // Additional padding (2 bytes to make total 63 bytes)
        ];

        let hello = ClientHello::parse(&data).expect("Failed to parse ClientHello");
        assert_eq!(hello.supported_groups.len(), 3);
        assert_eq!(hello.supported_groups[0], NamedGroup::Secp256r1);
        assert_eq!(hello.supported_groups[1], NamedGroup::Secp384r1);
        assert_eq!(hello.supported_groups[2], NamedGroup::X25519);
    }

    #[test]
    fn test_parse_signature_algorithms() {
        // ClientHello with signature_algorithms extension
        // Extension: Type(2) + Len(2) + Data(8) = 12 bytes
        //   Data: Algorithms List Len(2) + Algorithms(6) = 8 bytes
        // Extensions: Len(2) + Extension(12) = 14 bytes
        // But parse_extensions needs 61 bytes (pos 47 + extensions_len 14), so we need padding
        // Extensions: Len(2) + Extension(12) + Padding(2) = 16 bytes
        // Handshake body: Version(2) + Random(32) + SessionID(1) + CipherSuites(4) + Compression(2) + Extensions(16) = 57 bytes
        // But we need 63 bytes total, so add 2 more bytes = 59 bytes
        // Handshake: Type(1) + Len(3) + Body(59) = 63 bytes
        // Record: Header(5) + Handshake(63) = 68 bytes
        let data = vec![
            0x16, // Content Type: Handshake
            0x03, 0x03, // Version: TLS 1.2
            0x00, 0x3f, // Record Length (63 bytes = handshake data)
            0x01, // Handshake Type: ClientHello
            0x00, 0x00, 0x3b, // Handshake Length (59 bytes = body with padding)
            0x03, 0x03, // Client Version: TLS 1.2
            // Random (32 bytes)
            0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07,
            0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f,
            0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17,
            0x18, 0x19, 0x1a, 0x1b, 0x1c, 0x1d, 0x1e, 0x1f,
            0x00, // Session ID Length
            0x00, 0x02, // Cipher Suites Length (2 bytes)
            0x00, 0x2f, // Cipher suite
            0x01, // Compression Methods Length
            0x00, // Compression method: null
            0x00, 0x10, // Extensions Length (16 bytes = 14 + 2 padding)
            // Signature Algorithms Extension
            0x00, 0x0d, // Extension Type: signature_algorithms
            0x00, 0x08, // Extension Length (8 bytes)
            0x00, 0x06, // Algorithms List Length (6 bytes = 3 algorithms)
            0x04, 0x01, // SHA256 + RSA
            0x05, 0x01, // SHA384 + RSA
            0x06, 0x01, // SHA512 + RSA
            0x00, 0x00, // Padding (2 bytes)
            0x00, 0x00, // Additional padding (2 bytes)
        ];

        let hello = ClientHello::parse(&data).expect("Failed to parse ClientHello");
        assert_eq!(hello.signature_algorithms.len(), 3);
        assert_eq!(hello.signature_algorithms[0].hash, 0x04);
        assert_eq!(hello.signature_algorithms[0].signature, 0x01);
    }

    #[test]
    fn test_parse_tls13_supported_versions() {
        // ClientHello with TLS 1.3 supported_versions extension
        // Extension: Type(2) + Len(2) + Data(5) = 9 bytes
        //   Data: Versions List Len(1) + Versions(4) = 5 bytes
        // Extensions: Len(2) + Extension(9) = 11 bytes
        // But parse_extensions needs 58 bytes (pos 47 + extensions_len 11), so we need padding
        // Extensions: Len(2) + Extension(9) + Padding(2) = 13 bytes
        // Handshake body: Version(2) + Random(32) + SessionID(1) + CipherSuites(4) + Compression(2) + Extensions(13) = 54 bytes
        // But we need 60 bytes total, so add 2 more bytes = 56 bytes
        // Handshake: Type(1) + Len(3) + Body(56) = 60 bytes
        // Record: Header(5) + Handshake(60) = 65 bytes
        let data = vec![
            0x16, // Content Type: Handshake
            0x03, 0x01, // Version: TLS 1.0 (legacy)
            0x00, 0x3c, // Record Length (60 bytes = handshake data)
            0x01, // Handshake Type: ClientHello
            0x00, 0x00, 0x38, // Handshake Length (56 bytes = body with padding)
            0x03, 0x03, // Client Version: TLS 1.2 (legacy)
            // Random (32 bytes)
            0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07,
            0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f,
            0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17,
            0x18, 0x19, 0x1a, 0x1b, 0x1c, 0x1d, 0x1e, 0x1f,
            0x00, // Session ID Length
            0x00, 0x02, // Cipher Suites Length (2 bytes)
            0x13, 0x01, // TLS 1.3 cipher suite
            0x01, // Compression Methods Length
            0x00, // Compression method: null
            0x00, 0x0d, // Extensions Length (13 bytes = 11 + 2 padding)
            // Supported Versions Extension (TLS 1.3)
            0x00, 0x2b, // Extension Type: supported_versions
            0x00, 0x05, // Extension Length (5 bytes)
            0x04, // Versions List Length (4 bytes = 2 versions)
            0x03, 0x03, // TLS 1.2
            0x03, 0x04, // TLS 1.3
            0x00, 0x00, // Padding (2 bytes)
            0x00, 0x00, // Additional padding (2 bytes)
        ];

        let hello = ClientHello::parse(&data).expect("Failed to parse ClientHello");
        assert!(hello.tls13_extensions.is_some());
        let tls13 = hello.tls13_extensions.as_ref().unwrap();
        assert_eq!(tls13.supported_versions.len(), 2);
        assert_eq!(tls13.supported_versions[0], 0x0303); // TLS 1.2
        assert_eq!(tls13.supported_versions[1], 0x0304); // TLS 1.3
    }

    #[test]
    fn test_parse_tls13_key_share() {
        // ClientHello with TLS 1.3 key_share extension
        // Extension: Type(2) + Len(2) + Data(21) = 25 bytes
        //   Data: Client Shares Len(2) + Share(19) = 21 bytes
        //     Share: Group(2) + Key Exchange Len(2) + Key Exchange(16) = 19 bytes
        // Extensions: Len(2) + Extension(25) = 27 bytes
        // But parse_extensions needs 74 bytes (pos 47 + extensions_len 27), so we need padding
        // Extensions: Len(2) + Extension(25) + Padding(2) = 29 bytes
        // Handshake body: Version(2) + Random(32) + SessionID(1) + CipherSuites(4) + Compression(2) + Extensions(29) = 70 bytes
        // But we need 76 bytes total, so add 2 more bytes = 72 bytes
        // Handshake: Type(1) + Len(3) + Body(72) = 76 bytes
        // Record: Header(5) + Handshake(76) = 81 bytes
        let data = vec![
            0x16, // Content Type: Handshake
            0x03, 0x01, // Version: TLS 1.0 (legacy)
            0x00, 0x4c, // Record Length (76 bytes = handshake data)
            0x01, // Handshake Type: ClientHello
            0x00, 0x00, 0x48, // Handshake Length (72 bytes = body with padding)
            0x03, 0x03, // Client Version: TLS 1.2 (legacy)
            // Random (32 bytes)
            0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07,
            0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f,
            0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17,
            0x18, 0x19, 0x1a, 0x1b, 0x1c, 0x1d, 0x1e, 0x1f,
            0x00, // Session ID Length
            0x00, 0x02, // Cipher Suites Length (2 bytes)
            0x13, 0x01, // TLS 1.3 cipher suite
            0x01, // Compression Methods Length
            0x00, // Compression method: null
            0x00, 0x1d, // Extensions Length (29 bytes = 27 + 2 padding)
            // Key Share Extension (TLS 1.3)
            0x00, 0x33, // Extension Type: key_share
            0x00, 0x15, // Extension Length (21 bytes)
            0x00, 0x13, // Client Shares Length (19 bytes)
            0x00, 0x1d, // Group: x25519
            0x00, 0x10, // Key Exchange Length (16 bytes)
            // Key exchange data (16 bytes)
            0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07,
            0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f,
            0x00, 0x00, // Padding (2 bytes)
            0x00, 0x00, // Additional padding (2 bytes)
        ];

        let hello = ClientHello::parse(&data).expect("Failed to parse ClientHello");
        assert!(hello.tls13_extensions.is_some());
        let tls13 = hello.tls13_extensions.as_ref().unwrap();
        assert_eq!(tls13.key_share_groups.len(), 1);
        assert_eq!(tls13.key_share_groups[0], NamedGroup::X25519);
    }

    #[test]
    fn test_parse_raw_extensions() {
        // ClientHello with unknown extension
        // Extension: Type(2) + Len(2) + Data(6) = 10 bytes
        // Extensions: Len(2) + Extension(10) = 12 bytes
        // But parse_extensions needs 59 bytes (pos 47 + extensions_len 12), so we need padding
        // Extensions: Len(2) + Extension(10) + Padding(2) = 14 bytes
        // Handshake body: Version(2) + Random(32) + SessionID(1) + CipherSuites(4) + Compression(2) + Extensions(14) = 55 bytes
        // But we need 61 bytes total, so add 2 more bytes = 57 bytes
        // Handshake: Type(1) + Len(3) + Body(57) = 61 bytes
        // Record: Header(5) + Handshake(61) = 66 bytes
        let data = vec![
            0x16, // Content Type: Handshake
            0x03, 0x03, // Version: TLS 1.2
            0x00, 0x3d, // Record Length (61 bytes = handshake data)
            0x01, // Handshake Type: ClientHello
            0x00, 0x00, 0x39, // Handshake Length (57 bytes = body with padding)
            0x03, 0x03, // Client Version: TLS 1.2
            // Random (32 bytes)
            0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07,
            0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f,
            0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17,
            0x18, 0x19, 0x1a, 0x1b, 0x1c, 0x1d, 0x1e, 0x1f,
            0x00, // Session ID Length
            0x00, 0x02, // Cipher Suites Length (2 bytes)
            0x00, 0x2f, // Cipher suite
            0x01, // Compression Methods Length
            0x00, // Compression method: null
            0x00, 0x0e, // Extensions Length (14 bytes = 12 + 2 padding)
            // Unknown Extension (0x9999)
            0x99, 0x99, // Extension Type: unknown
            0x00, 0x06, // Extension Length (6 bytes)
            0x01, 0x02, 0x03, 0x04, 0x05, 0x06, // Extension data
            0x00, 0x00, // Padding (2 bytes)
            0x00, 0x00, // Additional padding (2 bytes)
        ];

        let hello = ClientHello::parse(&data).expect("Failed to parse ClientHello");
        assert!(hello.raw_extensions.contains_key(&0x9999));
        let raw_data = hello.raw_extensions.get(&0x9999).unwrap();
        assert_eq!(raw_data, &vec![0x01, 0x02, 0x03, 0x04, 0x05, 0x06]);
    }
}

