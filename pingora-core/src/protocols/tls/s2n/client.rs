// Copyright 2026 Cloudflare, Inc.
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

//! S2N client specific implementation

use crate::protocols::tls::{AutoFlushableStream, S2NConnectionBuilder, TlsStream};
use crate::protocols::IO;
use pingora_error::ErrorType::TLSHandshakeFailure;
use pingora_error::{Error, Result};
use pingora_s2n::TlsConnector;

// Perform the TLS handshake for the given connection with the given configuration
pub async fn handshake<S: IO>(
    connector: &TlsConnector<S2NConnectionBuilder>,
    domain: &str,
    stream: S,
) -> Result<TlsStream<S>> {
    // Wrap incoming stream in an auto flushable stream with auto flush enabled because
    // s2n-tls doesn't invoke flush after writing to the connection. This would result in
    // the handshake hanging and timing on streams with write buffering.
    let auto_flushable_stream = AutoFlushableStream::new(stream, true);
    let mut s2n_stream = connector
        .connect(domain, auto_flushable_stream)
        .await
        .map_err(|e| {
            let context = format!("TLS connect() failed: {e}, SNI: {domain}");
            Error::explain(TLSHandshakeFailure, context)
        })?;

    // Disable auto-flush to not interfere with write buffering going forward.
    s2n_stream.get_mut().set_auto_flush(false);
    Ok(TlsStream::from_s2n_stream(s2n_stream))
}
