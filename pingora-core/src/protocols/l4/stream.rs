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

//! Transport layer connection

use async_trait::async_trait;
use futures::FutureExt;
use log::{debug, error};
use pingora_error::{ErrorType::*, OrErr, Result};
use std::os::unix::io::AsRawFd;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::SystemTime;
use tokio::io::{self, AsyncRead, AsyncWrite, AsyncWriteExt, BufStream, ReadBuf};
use tokio::net::{TcpStream, UnixStream};

use crate::protocols::raw_connect::ProxyDigest;
use crate::protocols::{
    GetProxyDigest, GetSocketDigest, GetTimingDigest, Shutdown, SocketDigest, Ssl, TimingDigest,
    UniqueID,
};
use crate::upstreams::peer::Tracer;

#[derive(Debug)]
enum RawStream {
    Tcp(TcpStream),
    Unix(UnixStream),
}

impl AsyncRead for RawStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        // Safety: Basic enum pin projection
        unsafe {
            match &mut Pin::get_unchecked_mut(self) {
                RawStream::Tcp(s) => Pin::new_unchecked(s).poll_read(cx, buf),
                RawStream::Unix(s) => Pin::new_unchecked(s).poll_read(cx, buf),
            }
        }
    }
}

impl AsyncWrite for RawStream {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context, buf: &[u8]) -> Poll<io::Result<usize>> {
        // Safety: Basic enum pin projection
        unsafe {
            match &mut Pin::get_unchecked_mut(self) {
                RawStream::Tcp(s) => Pin::new_unchecked(s).poll_write(cx, buf),
                RawStream::Unix(s) => Pin::new_unchecked(s).poll_write(cx, buf),
            }
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context) -> Poll<io::Result<()>> {
        // Safety: Basic enum pin projection
        unsafe {
            match &mut Pin::get_unchecked_mut(self) {
                RawStream::Tcp(s) => Pin::new_unchecked(s).poll_flush(cx),
                RawStream::Unix(s) => Pin::new_unchecked(s).poll_flush(cx),
            }
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context) -> Poll<io::Result<()>> {
        // Safety: Basic enum pin projection
        unsafe {
            match &mut Pin::get_unchecked_mut(self) {
                RawStream::Tcp(s) => Pin::new_unchecked(s).poll_shutdown(cx),
                RawStream::Unix(s) => Pin::new_unchecked(s).poll_shutdown(cx),
            }
        }
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[std::io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        // Safety: Basic enum pin projection
        unsafe {
            match &mut Pin::get_unchecked_mut(self) {
                RawStream::Tcp(s) => Pin::new_unchecked(s).poll_write_vectored(cx, bufs),
                RawStream::Unix(s) => Pin::new_unchecked(s).poll_write_vectored(cx, bufs),
            }
        }
    }

    fn is_write_vectored(&self) -> bool {
        match self {
            RawStream::Tcp(s) => s.is_write_vectored(),
            RawStream::Unix(s) => s.is_write_vectored(),
        }
    }
}

impl AsRawFd for RawStream {
    fn as_raw_fd(&self) -> std::os::unix::io::RawFd {
        match self {
            RawStream::Tcp(s) => s.as_raw_fd(),
            RawStream::Unix(s) => s.as_raw_fd(),
        }
    }
}

// Large read buffering helps reducing syscalls with little trade-off
// Ssl layer always does "small" reads in 16k (TLS record size) so L4 read buffer helps a lot.
const BUF_READ_SIZE: usize = 64 * 1024;
// Small write buf to match MSS. Too large write buf delays real time communication.
// This buffering effectively implements something similar to Nagle's algorithm.
// The benefit is that user space can control when to flush, where Nagle's can't be controlled.
// And userspace buffering reduce both syscalls and small packets.
const BUF_WRITE_SIZE: usize = 1460;

// NOTE: with writer buffering, users need to call flush() to make sure the data is actually
// sent. Otherwise data could be stuck in the buffer forever or get lost when stream is closed.

/// A concrete type for transport layer connection + extra fields for logging
#[derive(Debug)]
pub struct Stream {
    stream: BufStream<RawStream>,
    buffer_write: bool,
    proxy_digest: Option<Arc<ProxyDigest>>,
    socket_digest: Option<Arc<SocketDigest>>,
    /// When this connection is established
    pub established_ts: SystemTime,
    /// The distributed tracing object for this stream
    pub tracer: Option<Tracer>,
}

impl Stream {
    /// set TCP nodelay for this connection if `self` is TCP
    pub fn set_nodelay(&mut self) -> Result<()> {
        if let RawStream::Tcp(s) = &self.stream.get_ref() {
            s.set_nodelay(true)
                .or_err(ConnectError, "failed to set_nodelay")?;
        }
        Ok(())
    }
}

impl From<TcpStream> for Stream {
    fn from(s: TcpStream) -> Self {
        Stream {
            stream: BufStream::with_capacity(BUF_READ_SIZE, BUF_WRITE_SIZE, RawStream::Tcp(s)),
            buffer_write: true,
            established_ts: SystemTime::now(),
            proxy_digest: None,
            socket_digest: None,
            tracer: None,
        }
    }
}

impl From<UnixStream> for Stream {
    fn from(s: UnixStream) -> Self {
        Stream {
            stream: BufStream::with_capacity(BUF_READ_SIZE, BUF_WRITE_SIZE, RawStream::Unix(s)),
            buffer_write: true,
            established_ts: SystemTime::now(),
            proxy_digest: None,
            socket_digest: None,
            tracer: None,
        }
    }
}

impl AsRawFd for Stream {
    fn as_raw_fd(&self) -> std::os::unix::io::RawFd {
        self.stream.get_ref().as_raw_fd()
    }
}

impl UniqueID for Stream {
    fn id(&self) -> i32 {
        self.as_raw_fd()
    }
}

impl Ssl for Stream {}

#[async_trait]
impl Shutdown for Stream {
    async fn shutdown(&mut self) {
        AsyncWriteExt::shutdown(self).await.unwrap_or_else(|e| {
            debug!("Failed to shutdown connection: {:?}", e);
        });
    }
}

impl GetTimingDigest for Stream {
    fn get_timing_digest(&self) -> Vec<Option<TimingDigest>> {
        let mut digest = Vec::with_capacity(2); // expect to have both L4 stream and TLS layer
        digest.push(Some(TimingDigest {
            established_ts: self.established_ts,
        }));
        digest
    }
}

impl GetProxyDigest for Stream {
    fn get_proxy_digest(&self) -> Option<Arc<ProxyDigest>> {
        self.proxy_digest.clone()
    }

    fn set_proxy_digest(&mut self, digest: ProxyDigest) {
        self.proxy_digest = Some(Arc::new(digest));
    }
}

impl GetSocketDigest for Stream {
    fn get_socket_digest(&self) -> Option<Arc<SocketDigest>> {
        self.socket_digest.clone()
    }

    fn set_socket_digest(&mut self, socket_digest: SocketDigest) {
        self.socket_digest = Some(Arc::new(socket_digest))
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        if let Some(t) = self.tracer.as_ref() {
            t.0.on_disconnected();
        }
        /* use nodelay/local_addr function to detect socket status */
        let ret = match &self.stream.get_ref() {
            RawStream::Tcp(s) => s.nodelay().err(),
            RawStream::Unix(s) => s.local_addr().err(),
        };
        if let Some(e) = ret {
            match e.kind() {
                tokio::io::ErrorKind::Other => {
                    if let Some(ecode) = e.raw_os_error() {
                        if ecode == 9 {
                            // Or we could panic here
                            error!("Crit: socket {:?} is being double closed", self.stream);
                        }
                    }
                }
                _ => {
                    debug!("Socket is already broken {:?}", e);
                }
            }
        } else {
            // try flush the write buffer. We use now_or_never() because
            // 1. Drop cannot be async
            // 2. write should usually be ready, unless the buf is full.
            let _ = self.flush().now_or_never();
        }
        debug!("Dropping socket {:?}", self.stream);
    }
}

impl AsyncRead for Stream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_read(cx, buf)
    }
}

impl AsyncWrite for Stream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        if self.buffer_write {
            Pin::new(&mut self.stream).poll_write(cx, buf)
        } else {
            Pin::new(&mut self.stream.get_mut()).poll_write(cx, buf)
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_shutdown(cx)
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[std::io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        if self.buffer_write {
            Pin::new(&mut self.stream).poll_write_vectored(cx, bufs)
        } else {
            Pin::new(&mut self.stream.get_mut()).poll_write_vectored(cx, bufs)
        }
    }

    fn is_write_vectored(&self) -> bool {
        if self.buffer_write {
            self.stream.is_write_vectored() // it is true
        } else {
            self.stream.get_ref().is_write_vectored()
        }
    }
}

pub mod async_write_vec {
    use bytes::Buf;
    use futures::ready;
    use std::future::Future;
    use std::io::IoSlice;
    use std::pin::Pin;
    use std::task::{Context, Poll};
    use tokio::io;
    use tokio::io::AsyncWrite;

    /*
        the missing write_buf https://github.com/tokio-rs/tokio/pull/3156#issuecomment-738207409
        https://github.com/tokio-rs/tokio/issues/2610
        In general vectored write is lost when accessing the trait object: Box<S: AsyncWrite>
    */

    #[must_use = "futures do nothing unless you `.await` or poll them"]
    pub struct WriteVec<'a, W, B> {
        writer: &'a mut W,
        buf: &'a mut B,
    }

    pub trait AsyncWriteVec {
        fn poll_write_vec<B: Buf>(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _buf: &mut B,
        ) -> Poll<io::Result<usize>>;

        fn write_vec<'a, B>(&'a mut self, src: &'a mut B) -> WriteVec<'a, Self, B>
        where
            Self: Sized,
            B: Buf,
        {
            WriteVec {
                writer: self,
                buf: src,
            }
        }
    }

    impl<W, B> Future for WriteVec<'_, W, B>
    where
        W: AsyncWriteVec + Unpin,
        B: Buf,
    {
        type Output = io::Result<usize>;

        fn poll(mut self: Pin<&mut Self>, ctx: &mut Context<'_>) -> Poll<io::Result<usize>> {
            let me = &mut *self;
            Pin::new(&mut *me.writer).poll_write_vec(ctx, me.buf)
        }
    }

    /* from https://github.com/tokio-rs/tokio/blob/master/tokio-util/src/lib.rs#L177 */
    impl<T> AsyncWriteVec for T
    where
        T: AsyncWrite,
    {
        fn poll_write_vec<B: Buf>(
            self: Pin<&mut Self>,
            ctx: &mut Context,
            buf: &mut B,
        ) -> Poll<io::Result<usize>> {
            const MAX_BUFS: usize = 64;

            if !buf.has_remaining() {
                return Poll::Ready(Ok(0));
            }

            let n = if self.is_write_vectored() {
                let mut slices = [IoSlice::new(&[]); MAX_BUFS];
                let cnt = buf.chunks_vectored(&mut slices);
                ready!(self.poll_write_vectored(ctx, &slices[..cnt]))?
            } else {
                ready!(self.poll_write(ctx, buf.chunk()))?
            };

            buf.advance(n);

            Poll::Ready(Ok(n))
        }
    }
}

pub use async_write_vec::AsyncWriteVec;
