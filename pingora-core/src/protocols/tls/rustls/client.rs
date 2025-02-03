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

//! Rustls TLS client specific implementation

use crate::protocols::tls::rustls::TlsStream;
use crate::protocols::IO;
use pingora_error::ErrorType::TLSHandshakeFailure;
use pingora_error::{Error, OrErr, Result};
use pingora_rustls::TlsConnector;

// Perform the TLS handshake for the given connection with the given configuration
pub async fn handshake<S: IO>(
    connector: &TlsConnector,
    domain: &str,
    io: S,
) -> Result<TlsStream<S>> {
    let mut stream = TlsStream::from_connector(connector, domain, io)
        .await
        .or_err(TLSHandshakeFailure, "tls stream error")?;

    let handshake_result = stream.connect().await;
    match handshake_result {
        Ok(()) => Ok(stream),
        Err(e) => {
            let context = format!("TLS connect() failed: {e}, SNI: {domain}");
            Error::e_explain(TLSHandshakeFailure, context)
        }
    }
}
