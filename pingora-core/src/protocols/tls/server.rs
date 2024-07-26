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

//! TLS server specific implementation

use std::pin::Pin;

use async_trait::async_trait;
use log::warn;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};

use pingora_error::Result;

use crate::protocols::tls::TlsStream;
use crate::protocols::{Shutdown, IO};

#[cfg(not(feature = "rustls"))]
use crate::tls::ssl::SslRef;

/// The APIs to customize things like certificate during TLS server side handshake
#[async_trait]
pub trait TlsAccept {
    // TODO: return error?
    /// This function is called in the middle of a TLS handshake. Structs who implement this function
    /// should provide tls certificate and key to the [SslRef] via [ext::ssl_use_certificate] and [ext::ssl_use_private_key].
    #[cfg(not(feature = "rustls"))]
    async fn certificate_callback(&self, _ssl: &mut SslRef) -> () {
        // does nothing by default
    }
}

pub type TlsAcceptCallbacks = Box<dyn TlsAccept + Send + Sync>;

#[async_trait]
impl<S> Shutdown for TlsStream<S>
where
    S: AsyncRead + AsyncWrite + Sync + Unpin + Send,
{
    async fn shutdown(&mut self) {
        match <Self as AsyncWriteExt>::shutdown(self).await {
            Ok(()) => {}
            Err(e) => {
                warn!("TLS shutdown failed, {e}");
            }
        }
    }
}

#[async_trait]
impl Shutdown for Box<dyn IO + Send> {
    async fn shutdown(&mut self) {
        match <Self as AsyncWriteExt>::shutdown(self).await {
            Ok(()) => {}
            Err(e) => {
                warn!("TLS shutdown failed, {e}");
            }
        }
    }
}

/// Resumable TLS server side handshake.
#[async_trait]
pub trait ResumableAccept {
    /// Start a resumable TLS accept handshake.
    ///
    /// * `Ok(true)` when the handshake is finished
    /// * `Ok(false)`` when the handshake is paused midway
    ///
    /// For now, accept will only pause when a certificate is needed.
    async fn start_accept(self: Pin<&mut Self>) -> Result<bool>;

    /// Continue the TLS handshake
    ///
    /// This function should be called after the certificate is provided.
    async fn resume_accept(self: Pin<&mut Self>) -> Result<()>;
}
