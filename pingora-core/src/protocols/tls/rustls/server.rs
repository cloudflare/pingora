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

//! Rustls TLS server specific implementation

use crate::listeners::TlsAcceptCallbacks;
use crate::protocols::tls::rustls::TlsStream;
use crate::protocols::IO;
use crate::{listeners::tls::Acceptor, protocols::Shutdown};
use async_trait::async_trait;
use log::warn;
use pingora_error::{ErrorType::*, OrErr, Result};
use std::pin::Pin;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};

impl<S: AsyncRead + AsyncWrite + Send + Unpin> TlsStream<S> {
    async fn start_accept(mut self: Pin<&mut Self>) -> Result<bool> {
        // TODO: suspend cert callback
        let res = self.accept().await;

        match res {
            Ok(()) => Ok(true),
            Err(e) => {
                if e.etype == TLSWantX509Lookup {
                    Ok(false)
                } else {
                    Err(e)
                }
            }
        }
    }

    async fn resume_accept(mut self: Pin<&mut Self>) -> Result<()> {
        // TODO: unblock cert callback
        self.accept().await
    }
}

async fn prepare_tls_stream<S: IO>(acceptor: &Acceptor, io: S) -> Result<TlsStream<S>> {
    TlsStream::from_acceptor(acceptor, io)
        .await
        .explain_err(TLSHandshakeFailure, |e| format!("tls stream error: {e}"))
}

/// Perform TLS handshake for the given connection with the given configuration
pub async fn handshake<S: IO>(acceptor: &Acceptor, io: S) -> Result<TlsStream<S>> {
    let mut stream = prepare_tls_stream(acceptor, io).await?;
    stream
        .accept()
        .await
        .explain_err(TLSHandshakeFailure, |e| format!("TLS accept() failed: {e}"))?;
    Ok(stream)
}

/// Perform TLS handshake for the given connection with the given configuration and callbacks
/// callbacks are currently not supported within pingora Rustls and are ignored
pub async fn handshake_with_callback<S: IO>(
    acceptor: &Acceptor,
    io: S,
    _callbacks: &TlsAcceptCallbacks,
) -> Result<TlsStream<S>> {
    let mut tls_stream = prepare_tls_stream(acceptor, io).await?;
    let done = Pin::new(&mut tls_stream).start_accept().await?;
    if !done {
        // TODO: verify if/how callback in handshake can be done using Rustls
        warn!("Callacks are not supported with feature \"rustls\".");

        Pin::new(&mut tls_stream)
            .resume_accept()
            .await
            .explain_err(TLSHandshakeFailure, |e| format!("TLS accept() failed: {e}"))?;
    }

    Ok(tls_stream)
}

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

#[ignore]
#[tokio::test]
async fn test_async_cert() {
    todo!("callback support and test for Rustls")
}
