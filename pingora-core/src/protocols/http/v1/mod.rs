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

//! HTTP/1.x implementation

pub(crate) mod body;
pub mod client;
pub mod common;
pub(crate) mod header;
pub mod server;

/// Test utilities shared across HTTP/1.x unit tests
#[cfg(test)]
pub(crate) mod test_util {
    use std::pin::Pin;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::task::{Context, Poll};
    use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
    use tokio_test::io::Mock;

    /// A wrapper around [`Mock`] that counts flush calls.
    ///
    /// `tokio_test::io::Mock`'s `poll_flush` always returns `Ready(Ok(()))`,
    /// so we can't detect flush calls via mock alone. This wrapper counts them.
    #[derive(Debug)]
    pub(crate) struct FlushTrackingMock {
        inner: Mock,
        flush_count: Arc<AtomicUsize>,
    }

    impl FlushTrackingMock {
        pub(crate) fn new(mock: Mock) -> (Self, Arc<AtomicUsize>) {
            let flush_count = Arc::new(AtomicUsize::new(0));
            (
                FlushTrackingMock {
                    inner: mock,
                    flush_count: flush_count.clone(),
                },
                flush_count,
            )
        }

        pub(crate) fn flush_count(counter: &Arc<AtomicUsize>) -> usize {
            counter.load(Ordering::Relaxed)
        }
    }

    impl AsyncRead for FlushTrackingMock {
        fn poll_read(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<std::io::Result<()>> {
            Pin::new(&mut self.get_mut().inner).poll_read(cx, buf)
        }
    }

    impl AsyncWrite for FlushTrackingMock {
        fn poll_write(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            Pin::new(&mut self.get_mut().inner).poll_write(cx, buf)
        }

        fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            let this = self.get_mut();
            let result = Pin::new(&mut this.inner).poll_flush(cx);
            if let Poll::Ready(Ok(())) = &result {
                this.flush_count.fetch_add(1, Ordering::Relaxed);
            }
            result
        }

        fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
        }
    }

    // Implement IO-required traits so FlushTrackingMock can be used as Box<dyn IO>
    // in HttpSession tests (server.rs).
    use crate::protocols::{
        raw_connect::ProxyDigest, GetProxyDigest, GetSocketDigest, GetTimingDigest, Peek, Shutdown,
        SocketDigest, Ssl, TimingDigest, UniqueID, UniqueIDType,
    };

    #[async_trait::async_trait]
    impl Shutdown for FlushTrackingMock {
        async fn shutdown(&mut self) -> () {}
    }
    impl UniqueID for FlushTrackingMock {
        fn id(&self) -> UniqueIDType {
            0
        }
    }
    impl Ssl for FlushTrackingMock {}
    impl GetTimingDigest for FlushTrackingMock {
        fn get_timing_digest(&self) -> Vec<Option<TimingDigest>> {
            vec![]
        }
    }
    impl GetProxyDigest for FlushTrackingMock {
        fn get_proxy_digest(&self) -> Option<Arc<ProxyDigest>> {
            None
        }
    }
    impl GetSocketDigest for FlushTrackingMock {
        fn get_socket_digest(&self) -> Option<Arc<SocketDigest>> {
            None
        }
    }
    impl Peek for FlushTrackingMock {}
}
