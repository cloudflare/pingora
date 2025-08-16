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

//! Transport layer connection

use async_trait::async_trait;
use futures::FutureExt;
use log::{debug, error};

use pingora_error::{ErrorType::*, OrErr, Result};
#[cfg(target_os = "linux")]
use std::io::IoSliceMut;
#[cfg(unix)]
use std::os::unix::io::AsRawFd;
#[cfg(windows)]
use std::os::windows::io::AsRawSocket;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant, SystemTime};
#[cfg(target_os = "linux")]
use tokio::io::Interest;
use tokio::io::{self, AsyncRead, AsyncWrite, AsyncWriteExt, BufStream, ReadBuf};
use tokio::net::TcpStream;
#[cfg(unix)]
use tokio::net::UnixStream;

use crate::protocols::l4::ext::{set_tcp_keepalive, TcpKeepalive};
use crate::protocols::raw_connect::ProxyDigest;
use crate::protocols::{
    GetProxyDigest, GetSocketDigest, GetTimingDigest, Peek, Shutdown, SocketDigest, Ssl,
    TimingDigest, UniqueID, UniqueIDType,
};
use crate::upstreams::peer::Tracer;

#[derive(Debug)]
enum RawStream {
    Tcp(TcpStream),
    #[cfg(unix)]
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
                #[cfg(unix)]
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
                #[cfg(unix)]
                RawStream::Unix(s) => Pin::new_unchecked(s).poll_write(cx, buf),
            }
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context) -> Poll<io::Result<()>> {
        // Safety: Basic enum pin projection
        unsafe {
            match &mut Pin::get_unchecked_mut(self) {
                RawStream::Tcp(s) => Pin::new_unchecked(s).poll_flush(cx),
                #[cfg(unix)]
                RawStream::Unix(s) => Pin::new_unchecked(s).poll_flush(cx),
            }
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context) -> Poll<io::Result<()>> {
        // Safety: Basic enum pin projection
        unsafe {
            match &mut Pin::get_unchecked_mut(self) {
                RawStream::Tcp(s) => Pin::new_unchecked(s).poll_shutdown(cx),
                #[cfg(unix)]
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
                #[cfg(unix)]
                RawStream::Unix(s) => Pin::new_unchecked(s).poll_write_vectored(cx, bufs),
            }
        }
    }

    fn is_write_vectored(&self) -> bool {
        match self {
            RawStream::Tcp(s) => s.is_write_vectored(),
            #[cfg(unix)]
            RawStream::Unix(s) => s.is_write_vectored(),
        }
    }
}

#[cfg(unix)]
impl AsRawFd for RawStream {
    fn as_raw_fd(&self) -> std::os::unix::io::RawFd {
        match self {
            RawStream::Tcp(s) => s.as_raw_fd(),
            RawStream::Unix(s) => s.as_raw_fd(),
        }
    }
}

#[cfg(windows)]
impl AsRawSocket for RawStream {
    fn as_raw_socket(&self) -> std::os::windows::io::RawSocket {
        match self {
            RawStream::Tcp(s) => s.as_raw_socket(),
        }
    }
}

#[derive(Debug)]
struct RawStreamWrapper {
    pub(crate) stream: RawStream,
    /// store the last rx timestamp of the stream.
    pub(crate) rx_ts: Option<SystemTime>,
    /// enable reading rx timestamp
    #[cfg(target_os = "linux")]
    pub(crate) enable_rx_ts: bool,
    #[cfg(target_os = "linux")]
    /// This can be reused across multiple recvmsg calls. The cmsg buffer may
    /// come from old sockets created by older version of pingora and so,
    /// this vector can only grow.
    reusable_cmsg_space: Vec<u8>,
}

impl RawStreamWrapper {
    pub fn new(stream: RawStream) -> Self {
        RawStreamWrapper {
            stream,
            rx_ts: None,
            #[cfg(target_os = "linux")]
            enable_rx_ts: false,
            #[cfg(target_os = "linux")]
            reusable_cmsg_space: nix::cmsg_space!(nix::sys::time::TimeSpec),
        }
    }

    #[cfg(target_os = "linux")]
    pub fn enable_rx_ts(&mut self, enable_rx_ts: bool) {
        self.enable_rx_ts = enable_rx_ts;
    }
}

impl AsyncRead for RawStreamWrapper {
    #[cfg(not(target_os = "linux"))]
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        // Safety: Basic enum pin projection
        unsafe {
            let rs_wrapper = Pin::get_unchecked_mut(self);
            match &mut rs_wrapper.stream {
                RawStream::Tcp(s) => Pin::new_unchecked(s).poll_read(cx, buf),
                #[cfg(unix)]
                RawStream::Unix(s) => Pin::new_unchecked(s).poll_read(cx, buf),
            }
        }
    }

    #[cfg(target_os = "linux")]
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        use futures::ready;
        use nix::sys::socket::{recvmsg, ControlMessageOwned, MsgFlags, SockaddrStorage};

        // if we do not need rx timestamp, then use the standard path
        if !self.enable_rx_ts {
            // Safety: Basic enum pin projection
            unsafe {
                let rs_wrapper = Pin::get_unchecked_mut(self);
                match &mut rs_wrapper.stream {
                    RawStream::Tcp(s) => return Pin::new_unchecked(s).poll_read(cx, buf),
                    RawStream::Unix(s) => return Pin::new_unchecked(s).poll_read(cx, buf),
                }
            }
        }

        // Safety: Basic pin projection to get mutable stream
        let rs_wrapper = unsafe { Pin::get_unchecked_mut(self) };
        match &mut rs_wrapper.stream {
            RawStream::Tcp(s) => {
                loop {
                    ready!(s.poll_read_ready(cx))?;
                    // Safety: maybe uninitialized bytes will only be passed to recvmsg
                    let b = unsafe {
                        &mut *(buf.unfilled_mut() as *mut [std::mem::MaybeUninit<u8>]
                            as *mut [u8])
                    };
                    let mut iov = [IoSliceMut::new(b)];
                    rs_wrapper.reusable_cmsg_space.clear();

                    match s.try_io(Interest::READABLE, || {
                        recvmsg::<SockaddrStorage>(
                            s.as_raw_fd(),
                            &mut iov,
                            Some(&mut rs_wrapper.reusable_cmsg_space),
                            MsgFlags::empty(),
                        )
                        .map_err(|errno| errno.into())
                    }) {
                        Ok(r) => {
                            if let Some(ControlMessageOwned::ScmTimestampsns(rtime)) = r
                                .cmsgs()
                                .find(|i| matches!(i, ControlMessageOwned::ScmTimestampsns(_)))
                            {
                                // The returned timestamp is a real (i.e. not monotonic) timestamp
                                // https://docs.kernel.org/networking/timestamping.html
                                rs_wrapper.rx_ts =
                                    SystemTime::UNIX_EPOCH.checked_add(rtime.system.into());
                            }
                            // Safety: We trust `recvmsg` to have filled up `r.bytes` bytes in the buffer.
                            unsafe {
                                buf.assume_init(r.bytes);
                            }
                            buf.advance(r.bytes);
                            return Poll::Ready(Ok(()));
                        }
                        Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => continue,
                        Err(e) => return Poll::Ready(Err(e)),
                    }
                }
            }
            // Unix RX timestamp only works with datagram for now, so we do not care about it
            RawStream::Unix(s) => unsafe { Pin::new_unchecked(s).poll_read(cx, buf) },
        }
    }
}

impl AsyncWrite for RawStreamWrapper {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context, buf: &[u8]) -> Poll<io::Result<usize>> {
        // Safety: Basic enum pin projection
        unsafe {
            match &mut Pin::get_unchecked_mut(self).stream {
                RawStream::Tcp(s) => Pin::new_unchecked(s).poll_write(cx, buf),
                #[cfg(unix)]
                RawStream::Unix(s) => Pin::new_unchecked(s).poll_write(cx, buf),
            }
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context) -> Poll<io::Result<()>> {
        // Safety: Basic enum pin projection
        unsafe {
            match &mut Pin::get_unchecked_mut(self).stream {
                RawStream::Tcp(s) => Pin::new_unchecked(s).poll_flush(cx),
                #[cfg(unix)]
                RawStream::Unix(s) => Pin::new_unchecked(s).poll_flush(cx),
            }
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context) -> Poll<io::Result<()>> {
        // Safety: Basic enum pin projection
        unsafe {
            match &mut Pin::get_unchecked_mut(self).stream {
                RawStream::Tcp(s) => Pin::new_unchecked(s).poll_shutdown(cx),
                #[cfg(unix)]
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
            match &mut Pin::get_unchecked_mut(self).stream {
                RawStream::Tcp(s) => Pin::new_unchecked(s).poll_write_vectored(cx, bufs),
                #[cfg(unix)]
                RawStream::Unix(s) => Pin::new_unchecked(s).poll_write_vectored(cx, bufs),
            }
        }
    }

    fn is_write_vectored(&self) -> bool {
        self.stream.is_write_vectored()
    }
}

#[cfg(unix)]
impl AsRawFd for RawStreamWrapper {
    fn as_raw_fd(&self) -> std::os::unix::io::RawFd {
        self.stream.as_raw_fd()
    }
}

#[cfg(windows)]
impl AsRawSocket for RawStreamWrapper {
    fn as_raw_socket(&self) -> std::os::windows::io::RawSocket {
        self.stream.as_raw_socket()
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
    // Use `Option` to be able to swap to adjust the buffer size. Always safe to unwrap
    stream: Option<BufStream<RawStreamWrapper>>,
    // the data put back at the front of the read buffer, in order to replay the read
    rewind_read_buf: Vec<Vec<u8>>,
    buffer_write: bool,
    proxy_digest: Option<Arc<ProxyDigest>>,
    socket_digest: Option<Arc<SocketDigest>>,
    /// When this connection is established
    pub established_ts: SystemTime,
    /// The distributed tracing object for this stream
    pub tracer: Option<Tracer>,
    read_pending_time: AccumulatedDuration,
    write_pending_time: AccumulatedDuration,
    /// Last rx timestamp associated with the last recvmsg call.
    pub rx_ts: Option<SystemTime>,
}

impl Stream {
    fn stream(&self) -> &BufStream<RawStreamWrapper> {
        self.stream.as_ref().expect("stream should always be set")
    }

    fn stream_mut(&mut self) -> &mut BufStream<RawStreamWrapper> {
        self.stream.as_mut().expect("stream should always be set")
    }

    /// set TCP nodelay for this connection if `self` is TCP
    pub fn set_nodelay(&mut self) -> Result<()> {
        if let RawStream::Tcp(s) = &self.stream_mut().get_mut().stream {
            s.set_nodelay(true)
                .or_err(ConnectError, "failed to set_nodelay")?;
        }
        Ok(())
    }

    /// set TCP keepalive settings for this connection if `self` is TCP
    pub fn set_keepalive(&mut self, ka: &TcpKeepalive) -> Result<()> {
        if let RawStream::Tcp(s) = &self.stream_mut().get_mut().stream {
            debug!("Setting tcp keepalive");
            set_tcp_keepalive(s, ka)?;
        }
        Ok(())
    }

    #[cfg(target_os = "linux")]
    pub fn set_rx_timestamp(&mut self) -> Result<()> {
        use nix::sys::socket::{setsockopt, sockopt, TimestampingFlag};

        if let RawStream::Tcp(s) = &self.stream_mut().get_mut().stream {
            let timestamp_options = TimestampingFlag::SOF_TIMESTAMPING_RX_SOFTWARE
                | TimestampingFlag::SOF_TIMESTAMPING_SOFTWARE;
            setsockopt(s.as_raw_fd(), sockopt::Timestamping, &timestamp_options)
                .or_err(InternalError, "failed to set SOF_TIMESTAMPING_RX_SOFTWARE")?;
            self.stream_mut().get_mut().enable_rx_ts(true);
        }

        Ok(())
    }

    #[cfg(not(target_os = "linux"))]
    pub fn set_rx_timestamp(&mut self) -> io::Result<()> {
        Ok(())
    }

    /// Put Some data back to the head of the stream to be read again
    pub(crate) fn rewind(&mut self, data: &[u8]) {
        if !data.is_empty() {
            self.rewind_read_buf.push(data.to_vec());
        }
    }

    /// Set the buffer of BufStream
    /// It is only set later because of the malloc overhead in critical accept() path
    pub(crate) fn set_buffer(&mut self) {
        use std::mem;
        // Since BufStream doesn't provide an API to adjust the buf directly,
        // we take the raw stream out of it and put it in a new BufStream with the size we want
        let stream = mem::take(&mut self.stream);
        let stream =
            stream.map(|s| BufStream::with_capacity(BUF_READ_SIZE, BUF_WRITE_SIZE, s.into_inner()));
        let _ = mem::replace(&mut self.stream, stream);
    }
}

impl From<TcpStream> for Stream {
    fn from(s: TcpStream) -> Self {
        Stream {
            stream: Some(BufStream::with_capacity(
                0,
                0,
                RawStreamWrapper::new(RawStream::Tcp(s)),
            )),
            rewind_read_buf: Vec::new(),
            buffer_write: true,
            established_ts: SystemTime::now(),
            proxy_digest: None,
            socket_digest: None,
            tracer: None,
            read_pending_time: AccumulatedDuration::new(),
            write_pending_time: AccumulatedDuration::new(),
            rx_ts: None,
        }
    }
}

#[cfg(unix)]
impl From<UnixStream> for Stream {
    fn from(s: UnixStream) -> Self {
        Stream {
            stream: Some(BufStream::with_capacity(
                0,
                0,
                RawStreamWrapper::new(RawStream::Unix(s)),
            )),
            rewind_read_buf: Vec::new(),
            buffer_write: true,
            established_ts: SystemTime::now(),
            proxy_digest: None,
            socket_digest: None,
            tracer: None,
            read_pending_time: AccumulatedDuration::new(),
            write_pending_time: AccumulatedDuration::new(),
            rx_ts: None,
        }
    }
}

#[cfg(unix)]
impl AsRawFd for Stream {
    fn as_raw_fd(&self) -> std::os::unix::io::RawFd {
        self.stream().get_ref().as_raw_fd()
    }
}

#[cfg(windows)]
impl AsRawSocket for Stream {
    fn as_raw_socket(&self) -> std::os::windows::io::RawSocket {
        self.stream().get_ref().as_raw_socket()
    }
}

#[cfg(unix)]
impl UniqueID for Stream {
    fn id(&self) -> UniqueIDType {
        self.as_raw_fd()
    }
}

#[cfg(windows)]
impl UniqueID for Stream {
    fn id(&self) -> usize {
        self.as_raw_socket() as usize
    }
}

impl Ssl for Stream {}

#[async_trait]
impl Peek for Stream {
    async fn try_peek(&mut self, buf: &mut [u8]) -> std::io::Result<bool> {
        use tokio::io::AsyncReadExt;
        self.read_exact(buf).await?;
        // rewind regardless of what is read
        self.rewind(buf);
        Ok(true)
    }
}

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

    fn get_read_pending_time(&self) -> Duration {
        self.read_pending_time.total
    }

    fn get_write_pending_time(&self) -> Duration {
        self.write_pending_time.total
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
        let ret = match &self.stream().get_ref().stream {
            RawStream::Tcp(s) => s.nodelay().err(),
            #[cfg(unix)]
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
        let result = if !self.rewind_read_buf.is_empty() {
            let data_to_read = self.rewind_read_buf.pop().unwrap(); // safe
            let mut data_to_read = data_to_read.as_slice();
            let result = Pin::new(&mut data_to_read).poll_read(cx, buf);
            // return the remaining data back to the head of rewind_read_buf
            if !data_to_read.is_empty() {
                let remaining_buf = Vec::from(data_to_read);
                self.rewind_read_buf.push(remaining_buf);
            }
            result
        } else {
            Pin::new(&mut self.stream_mut()).poll_read(cx, buf)
        };
        self.read_pending_time.poll_time(&result);
        self.rx_ts = self.stream().get_ref().rx_ts;
        result
    }
}

impl AsyncWrite for Stream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let result = if self.buffer_write {
            Pin::new(&mut self.stream_mut()).poll_write(cx, buf)
        } else {
            Pin::new(&mut self.stream_mut().get_mut()).poll_write(cx, buf)
        };
        self.write_pending_time.poll_write_time(&result, buf.len());
        result
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<io::Result<()>> {
        let result = Pin::new(&mut self.stream_mut()).poll_flush(cx);
        self.write_pending_time.poll_time(&result);
        result
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream_mut()).poll_shutdown(cx)
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[std::io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        let total_size = bufs.iter().fold(0, |acc, s| acc + s.len());

        let result = if self.buffer_write {
            Pin::new(&mut self.stream_mut()).poll_write_vectored(cx, bufs)
        } else {
            Pin::new(&mut self.stream_mut().get_mut()).poll_write_vectored(cx, bufs)
        };

        self.write_pending_time.poll_write_time(&result, total_size);
        result
    }

    fn is_write_vectored(&self) -> bool {
        if self.buffer_write {
            self.stream().is_write_vectored() // it is true
        } else {
            self.stream().get_ref().is_write_vectored()
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

    #[must_use = "futures do nothing unless you `.await` or poll them"]
    pub struct WriteVecAll<'a, W, B> {
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

        fn write_vec_all<'a, B>(&'a mut self, src: &'a mut B) -> WriteVecAll<'a, Self, B>
        where
            Self: Sized,
            B: Buf,
        {
            WriteVecAll {
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

    impl<W, B> Future for WriteVecAll<'_, W, B>
    where
        W: AsyncWriteVec + Unpin,
        B: Buf,
    {
        type Output = io::Result<()>;

        fn poll(mut self: Pin<&mut Self>, ctx: &mut Context<'_>) -> Poll<io::Result<()>> {
            let me = &mut *self;
            while me.buf.has_remaining() {
                let n = ready!(Pin::new(&mut *me.writer).poll_write_vec(ctx, me.buf))?;
                if n == 0 {
                    return Poll::Ready(Err(io::ErrorKind::WriteZero.into()));
                }
            }
            Poll::Ready(Ok(()))
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

#[derive(Debug)]
struct AccumulatedDuration {
    total: Duration,
    last_start: Option<Instant>,
}

impl AccumulatedDuration {
    fn new() -> Self {
        AccumulatedDuration {
            total: Duration::ZERO,
            last_start: None,
        }
    }

    fn start(&mut self) {
        if self.last_start.is_none() {
            self.last_start = Some(Instant::now());
        }
    }

    fn stop(&mut self) {
        if let Some(start) = self.last_start.take() {
            self.total += start.elapsed();
        }
    }

    fn poll_write_time(&mut self, result: &Poll<io::Result<usize>>, buf_size: usize) {
        match result {
            Poll::Ready(Ok(n)) => {
                if *n == buf_size {
                    self.stop();
                } else {
                    // partial write
                    self.start();
                }
            }
            Poll::Ready(Err(_)) => {
                self.stop();
            }
            _ => self.start(),
        }
    }

    fn poll_time(&mut self, result: &Poll<io::Result<()>>) {
        match result {
            Poll::Ready(_) => {
                self.stop();
            }
            _ => self.start(),
        }
    }
}

#[cfg(test)]
#[cfg(target_os = "linux")]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tokio::io::AsyncReadExt;
    use tokio::io::AsyncWriteExt;
    use tokio::net::TcpListener;
    use tokio::sync::Notify;

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn test_rx_timestamp() {
        let message = "hello world".as_bytes();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let notify = Arc::new(Notify::new());
        let notify2 = notify.clone();

        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            notify2.notified().await;
            stream.write_all(message).await.unwrap();
        });

        let mut stream: Stream = TcpStream::connect(addr).await.unwrap().into();
        stream.set_rx_timestamp().unwrap();
        // Receive the message
        // setsockopt for SO_TIMESTAMPING is asynchronous so sleep a little bit
        // to let kernel do the work
        std::thread::sleep(Duration::from_micros(100));
        notify.notify_one();

        let mut buffer = vec![0u8; message.len()];
        let n = stream.read(buffer.as_mut_slice()).await.unwrap();
        assert_eq!(n, message.len());
        assert!(stream.rx_ts.is_some());
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn test_rx_timestamp_standard_path() {
        let message = "hello world".as_bytes();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let notify = Arc::new(Notify::new());
        let notify2 = notify.clone();

        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            notify2.notified().await;
            stream.write_all(message).await.unwrap();
        });

        let mut stream: Stream = TcpStream::connect(addr).await.unwrap().into();
        std::thread::sleep(Duration::from_micros(100));
        notify.notify_one();

        let mut buffer = vec![0u8; message.len()];
        let n = stream.read(buffer.as_mut_slice()).await.unwrap();
        assert_eq!(n, message.len());
        assert!(stream.rx_ts.is_none());
    }

    #[tokio::test]
    async fn test_stream_rewind() {
        let message = b"hello world";
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let notify = Arc::new(Notify::new());
        let notify2 = notify.clone();

        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            notify2.notified().await;
            stream.write_all(message).await.unwrap();
        });

        let mut stream: Stream = TcpStream::connect(addr).await.unwrap().into();

        let rewind_test = b"this is Sparta!";
        stream.rewind(rewind_test);

        // partially read rewind_test because of the buffer size limit
        let mut buffer = vec![0u8; message.len()];
        let n = stream.read(buffer.as_mut_slice()).await.unwrap();
        assert_eq!(n, message.len());
        assert_eq!(buffer, rewind_test[..message.len()]);

        // read the rest of rewind_test
        let n = stream.read(buffer.as_mut_slice()).await.unwrap();
        assert_eq!(n, rewind_test.len() - message.len());
        assert_eq!(buffer[..n], rewind_test[message.len()..]);

        // read the actual data
        notify.notify_one();
        let n = stream.read(buffer.as_mut_slice()).await.unwrap();
        assert_eq!(n, message.len());
        assert_eq!(buffer, message);
    }

    #[tokio::test]
    async fn test_stream_peek() {
        let message = b"hello world";
        dbg!("try peek");
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let notify = Arc::new(Notify::new());
        let notify2 = notify.clone();

        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            notify2.notified().await;
            stream.write_all(message).await.unwrap();
            drop(stream);
        });

        notify.notify_one();

        let mut stream: Stream = TcpStream::connect(addr).await.unwrap().into();
        let mut buffer = vec![0u8; 5];
        assert!(stream.try_peek(&mut buffer).await.unwrap());
        assert_eq!(buffer, message[0..5]);
        let mut buffer = vec![];
        stream.read_to_end(&mut buffer).await.unwrap();
        assert_eq!(buffer, message);
    }

    #[tokio::test]
    async fn test_stream_two_subsequent_peek_calls_before_read() {
        let message = b"abcdefghijklmn";

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let notify = Arc::new(Notify::new());
        let notify2 = notify.clone();

        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            notify2.notified().await;
            stream.write_all(message).await.unwrap();
            drop(stream);
        });

        notify.notify_one();

        let mut stream: Stream = TcpStream::connect(addr).await.unwrap().into();

        // Peek 4 bytes
        let mut buffer = vec![0u8; 4];
        assert!(stream.try_peek(&mut buffer).await.unwrap());
        assert_eq!(buffer, message[0..4]);

        // Peek 2 bytes
        let mut buffer = vec![0u8; 2];
        assert!(stream.try_peek(&mut buffer).await.unwrap());
        assert_eq!(buffer, message[0..2]);

        // Read 1 byte: ['a']
        let mut buffer = vec![0u8; 1];
        stream.read_exact(&mut buffer).await.unwrap();
        assert_eq!(buffer, message[0..1]);

        // Read as many bytes as possible, return 1 byte ['b']
        //  from the first retry buffer chunk
        let mut buffer = vec![0u8; 100];
        let n = stream.read(&mut buffer).await.unwrap();
        assert_eq!(n, 1);
        assert_eq!(buffer[..n], message[1..2]);

        // Read the rest ['cdefghijklmn']
        let mut buffer = vec![];
        stream.read_to_end(&mut buffer).await.unwrap();
        assert_eq!(buffer, message[2..]);
    }
}
