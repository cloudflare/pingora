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

//! Wrapper for IO streams that extracts ClientHello using MSG_PEEK

use crate::protocols::tls::client_hello::{peek_client_hello, ClientHello};
use crate::protocols::{
    GetProxyDigest, GetSocketDigest, GetTimingDigest, Peek, Shutdown, Ssl, TimingDigest, UniqueID,
    UniqueIDType,
};
use async_trait::async_trait;
use log::debug;
use std::fmt::Debug;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

#[cfg(unix)]
use std::os::unix::io::AsRawFd;

/// Wrapper around an IO stream that extracts ClientHello before TLS handshake
pub struct ClientHelloWrapper<T> {
    inner: T,
    client_hello: Option<Arc<ClientHello>>,
    hello_extracted: bool,
}

impl<T> ClientHelloWrapper<T> {
    /// Create a new wrapper without extracting ClientHello yet
    pub fn new(inner: T) -> Self {
        ClientHelloWrapper {
            inner,
            client_hello: None,
            hello_extracted: false,
        }
    }

    /// Get the extracted ClientHello if available
    pub fn client_hello(&self) -> Option<Arc<ClientHello>> {
        self.client_hello.clone()
    }

    /// Get a reference to the inner stream
    pub fn get_ref(&self) -> &T {
        &self.inner
    }

    /// Get a mutable reference to the inner stream
    pub fn get_mut(&mut self) -> &mut T {
        &mut self.inner
    }

    /// Consume the wrapper and return the inner stream
    pub fn into_inner(self) -> T {
        self.inner
    }
}

#[cfg(unix)]
impl<T: AsRawFd> ClientHelloWrapper<T> {
    /// Attempt to extract ClientHello using MSG_PEEK
    /// This should be called before starting TLS handshake
    pub fn extract_client_hello(&mut self) -> io::Result<Option<Arc<ClientHello>>> {
        if self.hello_extracted {
            return Ok(self.client_hello.clone());
        }

        self.hello_extracted = true;

        match peek_client_hello(&self.inner) {
            Ok(Some(hello)) => {
                debug!(
                    "Extracted ClientHello: SNI={:?}, ALPN={:?}",
                    hello.sni, hello.alpn
                );
                let hello_arc = Arc::new(hello);
                self.client_hello = Some(hello_arc.clone());
                Ok(Some(hello_arc))
            }
            Ok(None) => {
                debug!("No ClientHello detected in stream");
                Ok(None)
            }
            Err(e) => {
                debug!("Failed to extract ClientHello: {:?}", e);
                // Don't fail the connection, just return None
                Ok(None)
            }
        }
    }
}

#[cfg(unix)]
impl<T: AsRawFd + AsyncRead + Unpin> ClientHelloWrapper<T> {
    /// Async extraction of ClientHello without blocking
    /// This polls the socket until data is available, then extracts ClientHello
    pub async fn extract_client_hello_async(&mut self) -> io::Result<Option<Arc<ClientHello>>> {
        if self.hello_extracted {
            return Ok(self.client_hello.clone());
        }

        // Poll until socket is readable
        use futures::FutureExt;
        use std::future::Future;
        use std::pin::Pin;
        use std::task::Context;

        struct ExtractFuture<'a, T: AsRawFd + AsyncRead + Unpin> {
            wrapper: Pin<&'a mut ClientHelloWrapper<T>>,
        }

        impl<'a, T: AsRawFd + AsyncRead + Unpin> Future for ExtractFuture<'a, T> {
            type Output = io::Result<Option<Arc<ClientHello>>>;

            fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
                let wrapper = self.wrapper.as_mut().get_mut();

                if wrapper.hello_extracted {
                    return Poll::Ready(Ok(wrapper.client_hello.clone()));
                }

                // Check if socket is ready for reading by polling with a zero-sized buffer
                let mut buf = ReadBuf::new(&mut []);
                match Pin::new(&mut wrapper.inner).poll_read(cx, &mut buf) {
                    Poll::Ready(Ok(_)) => {
                        // Socket is ready, try to extract
                        wrapper.hello_extracted = true;
                        match peek_client_hello(&wrapper.inner) {
                            Ok(Some(hello)) => {
                                debug!(
                                    "Async extracted ClientHello: SNI={:?}, ALPN={:?}",
                                    hello.sni, hello.alpn
                                );
                                let hello_arc = Arc::new(hello);
                                wrapper.client_hello = Some(hello_arc.clone());
                                Poll::Ready(Ok(Some(hello_arc)))
                            }
                            Ok(None) => {
                                debug!("No ClientHello detected in stream");
                                Poll::Ready(Ok(None))
                            }
                            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                                // Not ready yet, reset flag and continue polling
                                wrapper.hello_extracted = false;
                                Poll::Pending
                            }
                            Err(e) => {
                                debug!("Failed to extract ClientHello: {:?}", e);
                                Poll::Ready(Ok(None))
                            }
                        }
                    }
                    Poll::Ready(Err(e)) if e.kind() == io::ErrorKind::WouldBlock => {
                        // WouldBlock means not ready yet, continue polling
                        Poll::Pending
                    }
                    Poll::Ready(Err(e)) => {
                        debug!("Socket error while waiting for ClientHello: {:?}", e);
                        Poll::Ready(Ok(None))
                    }
                    Poll::Pending => Poll::Pending,
                }
            }
        }

        ExtractFuture {
            wrapper: Pin::new(self),
        }
        .await
    }
}

impl<T: Debug> Debug for ClientHelloWrapper<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClientHelloWrapper")
            .field("inner", &self.inner)
            .field("has_client_hello", &self.client_hello.is_some())
            .finish()
    }
}

impl<T: AsyncRead + Unpin> AsyncRead for ClientHelloWrapper<T> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl<T: AsyncWrite + Unpin> AsyncWrite for ClientHelloWrapper<T> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write_vectored(cx, bufs)
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }
}

#[async_trait]
impl<T: Shutdown + Send> Shutdown for ClientHelloWrapper<T> {
    async fn shutdown(&mut self) {
        self.inner.shutdown().await
    }
}

impl<T: UniqueID> UniqueID for ClientHelloWrapper<T> {
    fn id(&self) -> UniqueIDType {
        self.inner.id()
    }
}

impl<T: Ssl> Ssl for ClientHelloWrapper<T> {
    fn get_ssl(&self) -> Option<&crate::protocols::TlsRef> {
        self.inner.get_ssl()
    }

    fn get_ssl_digest(&self) -> Option<Arc<crate::protocols::tls::SslDigest>> {
        self.inner.get_ssl_digest()
    }

    fn selected_alpn_proto(&self) -> Option<crate::protocols::ALPN> {
        self.inner.selected_alpn_proto()
    }
}

impl<T: GetTimingDigest> GetTimingDigest for ClientHelloWrapper<T> {
    fn get_timing_digest(&self) -> Vec<Option<TimingDigest>> {
        self.inner.get_timing_digest()
    }

    fn get_read_pending_time(&self) -> std::time::Duration {
        self.inner.get_read_pending_time()
    }

    fn get_write_pending_time(&self) -> std::time::Duration {
        self.inner.get_write_pending_time()
    }
}

impl<T: GetProxyDigest> GetProxyDigest for ClientHelloWrapper<T> {
    fn get_proxy_digest(&self) -> Option<Arc<crate::protocols::raw_connect::ProxyDigest>> {
        self.inner.get_proxy_digest()
    }

    fn set_proxy_digest(&mut self, digest: crate::protocols::raw_connect::ProxyDigest) {
        self.inner.set_proxy_digest(digest)
    }
}

impl<T: GetSocketDigest> GetSocketDigest for ClientHelloWrapper<T> {
    fn get_socket_digest(&self) -> Option<Arc<crate::protocols::SocketDigest>> {
        self.inner.get_socket_digest()
    }

    fn set_socket_digest(&mut self, digest: crate::protocols::SocketDigest) {
        self.inner.set_socket_digest(digest)
    }
}

#[async_trait]
impl<T: Peek + Send> Peek for ClientHelloWrapper<T> {
    async fn try_peek(&mut self, buf: &mut [u8]) -> io::Result<bool> {
        self.inner.try_peek(buf).await
    }
}

#[cfg(unix)]
impl<T: AsRawFd> AsRawFd for ClientHelloWrapper<T> {
    fn as_raw_fd(&self) -> std::os::unix::io::RawFd {
        self.inner.as_raw_fd()
    }
}

#[cfg(windows)]
use std::os::windows::io::AsRawSocket;

#[cfg(windows)]
impl<T: AsRawSocket> AsRawSocket for ClientHelloWrapper<T> {
    fn as_raw_socket(&self) -> std::os::windows::io::RawSocket {
        self.inner.as_raw_socket()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn test_wrapper_creation() {
        let data = vec![0u8; 1024];
        let cursor = Cursor::new(data);
        let wrapper = ClientHelloWrapper::new(cursor);

        assert!(wrapper.client_hello().is_none());
        assert!(!wrapper.hello_extracted);
    }

    #[test]
    fn test_wrapper_unwrap() {
        let data = vec![0u8; 1024];
        let cursor = Cursor::new(data.clone());
        let wrapper = ClientHelloWrapper::new(cursor);

        let inner = wrapper.into_inner();
        assert_eq!(inner.into_inner(), data);
    }
}

