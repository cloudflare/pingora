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

//! Extended functionalities for s2n-tls

use s2n_tls::connection::Connection;
use s2n_tls::error::Error;

/// Export keying material from a TLS connection
///
/// Derives keying material for application use in accordance with RFC 5705.
///
/// Note: Currently only available with TLS 1.3 connections.
///
/// See [tls_exporter](https://docs.rs/s2n-tls/latest/s2n_tls/connection/struct.Connection.html#method.tls_exporter).
pub fn ssl_export_keying_material(
    conn: &Connection,
    out: &mut [u8],
    label: &str,
    context: Option<&[u8]>,
) -> Result<(), Error> {
    let context_bytes = context.unwrap_or(&[]);
    conn.tls_exporter(label.as_bytes(), context_bytes, out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use s2n_tls::config::Builder;
    use s2n_tls::enums::Mode;

    #[test]
    fn test_ssl_export_keying_material_exists() {
        // This test verifies that ssl_export_keying_material function exists
        // and has the correct signature. Actual functional testing requires
        // an established TLS connection.
        let config = Builder::new().build().unwrap();
        let mut conn = s2n_tls::connection::Connection::new(Mode::Client);
        conn.set_config(config).unwrap();
        let mut out = [0u8; 32];

        // This will fail since there's no established connection, but verifies
        // the function signature is correct
        let _ = ssl_export_keying_material(&conn, &mut out, "test", None);
    }
}
