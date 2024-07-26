// Copyright 2024 Cloudflare, Inc.
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

//! BoringSSL & OpenSSL TLS client specific implementation

use pingora_error::{Error, ErrorType::*, OrErr, Result};

use crate::protocols::tls::boringssl_openssl::TlsStream;
use crate::protocols::IO;
use crate::tls::ssl::ConnectConfiguration;

/// Perform the TLS handshake for the given connection with the given configuration
pub async fn handshake<S: IO>(
    conn_config: ConnectConfiguration,
    domain: &str,
    io: S,
) -> Result<TlsStream<S>> {
    let ssl = conn_config
        .into_ssl(domain)
        .explain_err(TLSHandshakeFailure, |e| format!("tls config error: {e}"))?;
    let mut stream = TlsStream::new(ssl, io)
        .explain_err(TLSHandshakeFailure, |e| format!("tls stream error: {e}"))?;

    stream.connect().await.map_err(|e| {
        let err_msg = format!("TLS connect() failed: {e}, SNI: {domain}");
        if let Some(context) = e.context {
            Error::explain(e.etype, format!("{}, {}", err_msg, context.as_str()))
        } else {
            Error::explain(e.etype, err_msg)
        }
    })?;
    Ok(stream)
}
