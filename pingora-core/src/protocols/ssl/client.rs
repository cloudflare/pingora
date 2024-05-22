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

//! TLS client specific implementation

use super::SslStream;
use crate::protocols::raw_connect::ProxyDigest;
use crate::protocols::{
    GetProxyDigest, GetSocketDigest, GetTimingDigest, SocketDigest, TimingDigest, IO,
};
use crate::tls::{ssl, ssl::ConnectConfiguration, ssl_sys::X509_V_ERR_INVALID_CALL};

use pingora_error::{Error, ErrorType::*, OrErr, Result};
use std::sync::Arc;
use std::time::Duration;

/// Perform the TLS handshake for the given connection with the given configuration
pub async fn handshake<S: IO>(
    conn_config: ConnectConfiguration,
    domain: &str,
    io: S,
) -> Result<SslStream<S>> {
    let ssl = conn_config
        .into_ssl(domain)
        .explain_err(TLSHandshakeFailure, |e| format!("ssl config error: {e}"))?;
    let mut stream = SslStream::new(ssl, io)
        .explain_err(TLSHandshakeFailure, |e| format!("ssl stream error: {e}"))?;
    let handshake_result = stream.connect().await;
    match handshake_result {
        Ok(()) => Ok(stream),
        Err(e) => {
            let context = format!("TLS connect() failed: {e}, SNI: {domain}");
            match e.code() {
                ssl::ErrorCode::SSL => {
                    // Unify the return type of `verify_result` for openssl
                    #[cfg(not(feature = "boringssl"))]
                    fn verify_result<S>(stream: SslStream<S>) -> Result<(), i32> {
                        match stream.ssl().verify_result().as_raw() {
                            crate::tls::ssl_sys::X509_V_OK => Ok(()),
                            e => Err(e),
                        }
                    }

                    // Unify the return type of `verify_result` for boringssl
                    #[cfg(feature = "boringssl")]
                    fn verify_result<S>(stream: SslStream<S>) -> Result<(), i32> {
                        stream.ssl().verify_result().map_err(|e| e.as_raw())
                    }

                    match verify_result(stream) {
                        Ok(()) => Error::e_explain(TLSHandshakeFailure, context),
                        // X509_V_ERR_INVALID_CALL in case verify result was never set
                        Err(X509_V_ERR_INVALID_CALL) => {
                            Error::e_explain(TLSHandshakeFailure, context)
                        }
                        _ => Error::e_explain(InvalidCert, context),
                    }
                }
                /* likely network error, but still mark as TLS error */
                _ => Error::e_explain(TLSHandshakeFailure, context),
            }
        }
    }
}

impl<S> GetTimingDigest for SslStream<S>
where
    S: GetTimingDigest,
{
    fn get_timing_digest(&self) -> Vec<Option<TimingDigest>> {
        let mut ts_vec = self.get_ref().get_timing_digest();
        ts_vec.push(Some(self.timing.clone()));
        ts_vec
    }
    fn get_read_pending_time(&self) -> Duration {
        self.get_ref().get_read_pending_time()
    }

    fn get_write_pending_time(&self) -> Duration {
        self.get_ref().get_write_pending_time()
    }
}

impl<S> GetProxyDigest for SslStream<S>
where
    S: GetProxyDigest,
{
    fn get_proxy_digest(&self) -> Option<Arc<ProxyDigest>> {
        self.get_ref().get_proxy_digest()
    }
}

impl<S> GetSocketDigest for SslStream<S>
where
    S: GetSocketDigest,
{
    fn get_socket_digest(&self) -> Option<Arc<SocketDigest>> {
        self.get_ref().get_socket_digest()
    }
    fn set_socket_digest(&mut self, socket_digest: SocketDigest) {
        self.get_mut().set_socket_digest(socket_digest)
    }
}
