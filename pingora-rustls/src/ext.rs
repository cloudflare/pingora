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

//! Extended functionalities for rustls

use rustls::client::ClientConnection;
use rustls::server::ServerConnection;
use rustls::Error;

/// Export keying material from a TLS client connection
///
/// Derives keying material for application use in accordance with RFC 5705.
///
/// See [export_keying_material](https://docs.rs/rustls/latest/rustls/struct.ConnectionCommon.html#method.export_keying_material).
pub fn ssl_export_keying_material(
    conn: &ClientConnection,
    out: &mut [u8],
    label: &str,
    context: Option<&[u8]>,
) -> Result<(), Error> {
    let output = out.to_vec();
    let result = conn.export_keying_material(output, label.as_bytes(), context)?;
    out.copy_from_slice(&result);
    Ok(())
}

/// Export keying material from a TLS server connection
///
/// Derives keying material for application use in accordance with RFC 5705.
///
/// See [export_keying_material](https://docs.rs/rustls/latest/rustls/struct.ConnectionCommon.html#method.export_keying_material).
pub fn ssl_export_keying_material_server(
    conn: &ServerConnection,
    out: &mut [u8],
    label: &str,
    context: Option<&[u8]>,
) -> Result<(), Error> {
    let output = out.to_vec();
    let result = conn.export_keying_material(output, label.as_bytes(), context)?;
    out.copy_from_slice(&result);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustls::{ClientConfig, RootCertStore, ServerConfig};
    use std::sync::Arc;

    #[test]
    fn test_ssl_export_keying_material_client_exists() {
        // This test verifies that ssl_export_keying_material function exists
        // and has the correct signature. Actual functional testing requires
        // an established TLS connection.
        let root_store = RootCertStore::empty();
        let config = Arc::new(
            ClientConfig::builder()
                .with_root_certificates(root_store)
                .with_no_client_auth(),
        );
        let server_name = "example.com".try_into().unwrap();
        let conn = ClientConnection::new(config, server_name).unwrap();
        let mut out = [0u8; 32];

        // This will fail since there's no established connection, but verifies
        // the function signature is correct
        let _ = ssl_export_keying_material(&conn, &mut out, "test", None);
    }

    #[test]
    fn test_ssl_export_keying_material_server_exists() {
        // This test verifies that ssl_export_keying_material_server function exists
        // and has the correct signature. Actual functional testing requires
        // an established TLS connection.
        let config = Arc::new(
            ServerConfig::builder()
                .with_no_client_auth()
                .with_cert_resolver(Arc::new(rustls::server::ResolvesServerCertUsingSni::new())),
        );
        let conn = ServerConnection::new(config).unwrap();
        let mut out = [0u8; 32];

        // This will fail since there's no established connection, but verifies
        // the function signature is correct
        let _ = ssl_export_keying_material_server(&conn, &mut out, "test", None);
    }
}
