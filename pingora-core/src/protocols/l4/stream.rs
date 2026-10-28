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

//! Transport layer connection

use async_trait::async_trait;
use bytes::BytesMut;
use futures::FutureExt;
use log::{debug, error};

use pingora_error::{ErrorType::*, OrErr, Result};
use std::future::Future;
#[cfg(target_os = "linux")]
use std::io::IoSliceMut;
#[cfg(unix)]
use std::os::unix::io::AsRawFd;
#[cfg(windows)]
use std::os::windows::io::AsRawSocket;
use std::pin::{pin, Pin};
use std::sync::Arc;
use std::task::{ready, Context, Poll};
use std::time::{Duration, Instant, SystemTime};
#[cfg(target_os = "linux")]
use tokio::io::Interest;
use tokio::io::{self, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufWriter, ReadBuf};
use tokio::net::TcpStream;
#[cfg(unix)]
use tokio::net::UnixStream;

use crate::listeners::ConnectionPermit;
use crate::protocols::l4::ext::{set_tcp_keepalive, TcpKeepalive};
use crate::protocols::l4::virt;
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
    Virtual(virt::VirtualSocketStream),
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
                RawStream::Virtual(s) => Pin::new_unchecked(s).poll_read(cx, buf),
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
                RawStream::Virtual(s) => Pin::new_unchecked(s).poll_write(cx, buf),
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
                RawStream::Virtual(s) => Pin::new_unchecked(s).poll_flush(cx),
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
                RawStream::Virtual(s) => Pin::new_unchecked(s).poll_shutdown(cx),
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
                RawStream::Virtual(s) => Pin::new_unchecked(s).poll_write_vectored(cx, bufs),
            }
        }
    }

    fn is_write_vectored(&self) -> bool {
        match self {
            RawStream::Tcp(s) => s.is_write_vectored(),
            #[cfg(unix)]
            RawStream::Unix(s) => s.is_write_vectored(),
            RawStream::Virtual(s) => s.is_write_vectored(),
        }
    }
}

#[cfg(unix)]
impl AsRawFd for RawStream {
    fn as_raw_fd(&self) -> std::os::unix::io::RawFd {
        match self {
            RawStream::Tcp(s) => s.as_raw_fd(),
            RawStream::Unix(s) => s.as_raw_fd(),
            RawStream::Virtual(_) => -1, // Virtual stream does not have a real fd
        }
    }
}

#[cfg(windows)]
impl AsRawSocket for RawStream {
    fn as_raw_socket(&self) -> std::os::windows::io::RawSocket {
        match self {
            RawStream::Tcp(s) => s.as_raw_socket(),
            // Virtual stream does not have a real socket, return INVALID_SOCKET (!0)
            RawStream::Virtual(_) => !0,
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
            reusable_cmsg_space: nix::cmsg_space!(nix::sys::socket::Timestamps),
        }
    }

    /// Wait before allocating read-ahead storage for real sockets. Virtual
    /// streams expose only AsyncRead, so their poll_read registers the waker.
    fn poll_read_ready(&self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match &self.stream {
            RawStream::Tcp(stream) => stream.poll_read_ready(cx),
            #[cfg(unix)]
            RawStream::Unix(stream) => stream.poll_read_ready(cx),
            RawStream::Virtual(_) => Poll::Ready(Ok(())),
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
                RawStream::Virtual(s) => Pin::new_unchecked(s).poll_read(cx, buf),
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
                    RawStream::Virtual(s) => return Pin::new_unchecked(s).poll_read(cx, buf),
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

                    rs_wrapper.reusable_cmsg_space.fill(0);

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
                                .cmsgs()?
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
            RawStream::Virtual(s) => unsafe { Pin::new_unchecked(s).poll_read(cx, buf) },
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
                RawStream::Virtual(s) => Pin::new_unchecked(s).poll_write(cx, buf),
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
                RawStream::Virtual(s) => Pin::new_unchecked(s).poll_flush(cx),
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
                RawStream::Virtual(s) => Pin::new_unchecked(s).poll_shutdown(cx),
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
                RawStream::Virtual(s) => Pin::new_unchecked(s).poll_write_vectored(cx, bufs),
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

/// The default L4 read buffer size.
///
/// Large read buffering helps reducing syscalls with little trade-off. The SSL
/// layer always does "small" reads in 16k chunks (TLS record size), so L4 read
/// buffering helps a lot. The 32 KiB default fits jemalloc's default tcache limit
/// and matches the HTTP/1 body read buffer.
pub const DEFAULT_L4_READ_BUFFER_SIZE: usize = 32 * 1024;

/// The default L4 write buffer size.
///
/// Small write buffering matches a typical MSS. Too large a write buffer delays
/// real-time communication. This buffering effectively implements something
/// similar to Nagle's algorithm, but user space can control when to flush.
pub const DEFAULT_L4_WRITE_BUFFER_SIZE: usize = 1460;

/// L4 read and write buffer sizing.
///
/// Leaving either side as `None` preserves Pingora's default for that side.
/// Setting either side to `Some(0)` disables buffering for that direction.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct L4BufferSettings {
    /// Read buffer size in bytes. `None` uses [`DEFAULT_L4_READ_BUFFER_SIZE`].
    pub read: Option<usize>,
    /// Write buffer size in bytes. `None` uses [`DEFAULT_L4_WRITE_BUFFER_SIZE`].
    pub write: Option<usize>,
}

impl L4BufferSettings {
    /// Create settings with both read and write buffer sizes set explicitly.
    pub fn new(read: usize, write: usize) -> Self {
        Self {
            read: Some(read),
            write: Some(write),
        }
    }

    /// Create settings that disable both read and write buffering.
    pub fn unbuffered() -> Self {
        Self::new(0, 0)
    }

    /// Set the read buffer size.
    pub fn read(mut self, read: usize) -> Self {
        self.read = Some(read);
        self
    }

    /// Set the write buffer size.
    pub fn write(mut self, write: usize) -> Self {
        self.write = Some(write);
        self
    }

    /// Resolved read buffer size after applying defaults.
    pub fn read_capacity(&self) -> usize {
        self.read.unwrap_or(DEFAULT_L4_READ_BUFFER_SIZE)
    }

    /// Resolved write buffer size after applying defaults.
    pub fn write_capacity(&self) -> usize {
        self.write.unwrap_or(DEFAULT_L4_WRITE_BUFFER_SIZE)
    }
}

/// Read-ahead storage whose debug output omits buffered contents.
#[derive(Default)]
struct ReadBuffer {
    storage: BytesMut,
}

impl std::fmt::Debug for ReadBuffer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReadBuffer")
            .field("len", &self.storage.len())
            .field("capacity", &self.storage.capacity())
            .finish()
    }
}

impl ReadBuffer {
    fn new(capacity: usize) -> Self {
        Self {
            storage: BytesMut::with_capacity(capacity),
        }
    }
}

/// Read-ahead storage over a write buffer. Read storage is acquired only when
/// attempting a ready read. A full refill can reuse its allocation after being
/// consumed; a drained short refill, Pending, EOF, or an error releases it.
/// If the caller stops reading after a full refill, its empty allocation stays
/// until another read. A zero capacity disables buffering in that direction.
#[derive(Debug)]
struct BufferedStream {
    writer: BufWriter<RawStreamWrapper>,
    read_capacity: usize,
    read_buf: ReadBuffer,
    read_pos: usize,
}

impl BufferedStream {
    fn with_capacity(read: usize, write: usize, stream: RawStreamWrapper) -> Self {
        Self {
            writer: BufWriter::with_capacity(write, stream),
            read_capacity: read,
            read_buf: ReadBuffer::default(),
            read_pos: 0,
        }
    }

    fn writer(&self) -> &BufWriter<RawStreamWrapper> {
        &self.writer
    }

    fn writer_mut(&mut self) -> &mut BufWriter<RawStreamWrapper> {
        &mut self.writer
    }

    fn get_ref(&self) -> &RawStreamWrapper {
        self.writer().get_ref()
    }

    fn get_mut(&mut self) -> &mut RawStreamWrapper {
        self.writer_mut().get_mut()
    }

    /// Return the raw stream. Anything left in either buffer is lost.
    fn into_inner(self) -> RawStreamWrapper {
        self.writer.into_inner()
    }
}

impl AsyncRead for BufferedStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = &mut *self;
        if buf.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }

        if this.read_pos < this.read_buf.storage.len() {
            let count = buf
                .remaining()
                .min(this.read_buf.storage.len() - this.read_pos);
            buf.put_slice(&this.read_buf.storage[this.read_pos..this.read_pos + count]);
            this.read_pos += count;
            if this.read_pos == this.read_buf.storage.len() {
                if this.read_buf.storage.len() == this.read_buf.storage.capacity() {
                    this.read_buf.storage.clear();
                } else {
                    this.read_buf = ReadBuffer::default();
                }
                this.read_pos = 0;
            }
            return Poll::Ready(Ok(()));
        }

        // Preserve BufReader's bypass for destinations at least as large as
        // the configured read-ahead capacity, including unbuffered reads.
        if buf.remaining() >= this.read_capacity {
            this.read_buf = ReadBuffer::default();
            return Pin::new(&mut this.writer).poll_read(cx, buf);
        }

        // Keep reusable storage local so any Pending or error, including from
        // the readiness check, drops it before returning to the caller.
        let mut storage = std::mem::take(&mut this.read_buf);
        ready!(this.writer.get_ref().poll_read_ready(cx))?;
        if storage.storage.capacity() == 0 {
            storage = ReadBuffer::new(this.read_capacity);
        }
        // Read through the wrapper so RX timestamps and virtual streams keep
        // their normal behavior. Stale readiness may still return Pending;
        // the local allocation is then dropped before yielding.
        // read_buf fills spare capacity without zeroing the whole destination.
        let read = ready!(pin!(this.writer.read_buf(&mut storage.storage)).poll(cx))?;
        let count = read.min(buf.remaining());
        buf.put_slice(&storage.storage[..count]);
        if count < read {
            this.read_buf = storage;
            this.read_pos = count;
        }
        Poll::Ready(Ok(()))
    }
}

// Keep write buffering independent of the lifetime of read-ahead storage.
impl AsyncWrite for BufferedStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(self.writer_mut()).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<io::Result<()>> {
        Pin::new(self.writer_mut()).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<io::Result<()>> {
        Pin::new(self.writer_mut()).poll_shutdown(cx)
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[std::io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        Pin::new(self.writer_mut()).poll_write_vectored(cx, bufs)
    }

    fn is_write_vectored(&self) -> bool {
        self.writer().is_write_vectored()
    }
}

// NOTE: with writer buffering, users need to call flush() to make sure the data is actually
// sent. Otherwise data could be stuck in the buffer forever or get lost when stream is closed.

/// A concrete type for transport layer connection + extra fields for logging
#[derive(Debug)]
pub struct Stream {
    // Use `Option` to be able to swap to adjust the buffer size. Always safe to unwrap
    stream: Option<BufferedStream>,
    // the data put back at the front of the read buffer, in order to replay the read
    rewind_read_buf: Vec<Vec<u8>>,
    buffer_write: bool,
    proxy_digest: Option<Arc<ProxyDigest>>,
    socket_digest: Option<Arc<SocketDigest>>,
    /// When this connection is established
    pub established_ts: SystemTime,
    establishment_duration: Option<Duration>,
    offload_wait_duration: Option<Duration>,
    /// The distributed tracing object for this stream
    pub tracer: Option<Tracer>,
    read_pending_time: AccumulatedDuration,
    write_pending_time: AccumulatedDuration,
    /// Last rx timestamp associated with the last recvmsg call.
    pub rx_ts: Option<SystemTime>,
    /// Counts an accepted connection against its endpoint's limits. Declared after `stream`, so
    /// the socket closes before the connection stops counting.
    connection_permit: ConnectionPermit,
}

impl Stream {
    fn stream(&self) -> &BufferedStream {
        self.stream.as_ref().expect("stream should always be set")
    }

    fn stream_mut(&mut self) -> &mut BufferedStream {
        self.stream.as_mut().expect("stream should always be set")
    }

    /// Count this stream against its endpoint's connection limit until it is dropped.
    pub(crate) fn set_connection_permit(&mut self, permit: ConnectionPermit) {
        self.connection_permit = permit;
    }

    /// Record transport connection timing for this stream.
    pub(crate) fn set_establishment_timing(
        &mut self,
        establishment_duration: Duration,
        offload_wait_duration: Option<Duration>,
    ) {
        self.establishment_duration = Some(establishment_duration);
        self.offload_wait_duration = offload_wait_duration;
    }

    /// set TCP nodelay for this connection if `self` is TCP
    pub fn set_nodelay(&mut self) -> Result<()> {
        match &self.stream_mut().get_mut().stream {
            RawStream::Tcp(s) => {
                s.set_nodelay(true)
                    .or_err(ConnectError, "failed to set_nodelay")?;
            }
            RawStream::Virtual(s) => {
                s.set_socket_option(virt::VirtualSockOpt::NoDelay)
                    .or_err(ConnectError, "failed to set_nodelay on virtual socket")?;
            }
            _ => (),
        }
        Ok(())
    }

    /// set TCP keepalive settings for this connection if `self` is TCP
    pub fn set_keepalive(&mut self, ka: &TcpKeepalive) -> Result<()> {
        match &self.stream_mut().get_mut().stream {
            RawStream::Tcp(s) => {
                debug!("Setting tcp keepalive");
                set_tcp_keepalive(s, ka)?;
            }
            RawStream::Virtual(s) => {
                s.set_socket_option(virt::VirtualSockOpt::KeepAlive(ka.clone()))
                    .or_err(ConnectError, "failed to set_keepalive on virtual socket")?;
            }
            _ => (),
        }
        Ok(())
    }

    #[cfg(target_os = "linux")]
    pub fn set_rx_timestamp(&mut self) -> Result<()> {
        use nix::sys::socket::{setsockopt, sockopt, TimestampingFlag};

        if let RawStream::Tcp(s) = &self.stream_mut().get_mut().stream {
            let timestamp_options = TimestampingFlag::SOF_TIMESTAMPING_RX_SOFTWARE
                | TimestampingFlag::SOF_TIMESTAMPING_SOFTWARE;
            setsockopt(&s, sockopt::Timestamping, &timestamp_options)
                .or_err(InternalError, "failed to set SOF_TIMESTAMPING_RX_SOFTWARE")?;
            self.stream_mut().get_mut().enable_rx_ts(true);
        }

        Ok(())
    }

    #[cfg(not(target_os = "linux"))]
    pub fn set_rx_timestamp(&mut self) -> io::Result<()> {
        Ok(())
    }

    /// Put some data back to the head of the stream to be read again.
    ///
    /// This is useful when you've read data to detect a protocol (e.g., PROXY protocol)
    /// but the data wasn't what you expected, so you need to "unread" it.
    pub fn rewind(&mut self, data: &[u8]) {
        if !data.is_empty() {
            self.rewind_read_buf.push(data.to_vec());
        }
    }

    /// Set the read and write buffer sizes
    /// It is only set later because of the malloc overhead in critical accept() path
    pub(crate) fn set_buffer(&mut self, buffer: L4BufferSettings) {
        use std::mem;
        // Since the buffers can't be resized in place,
        // we take the raw stream out of them and put it in new buffers with the size we want
        let stream = mem::take(&mut self.stream);
        let stream = stream.map(|s| {
            BufferedStream::with_capacity(
                buffer.read_capacity(),
                buffer.write_capacity(),
                s.into_inner(),
            )
        });
        let _ = mem::replace(&mut self.stream, stream);
    }
}

impl From<TcpStream> for Stream {
    fn from(s: TcpStream) -> Self {
        Stream {
            stream: Some(BufferedStream::with_capacity(
                0,
                0,
                RawStreamWrapper::new(RawStream::Tcp(s)),
            )),
            rewind_read_buf: Vec::new(),
            buffer_write: true,
            established_ts: SystemTime::now(),
            establishment_duration: None,
            offload_wait_duration: None,
            proxy_digest: None,
            socket_digest: None,
            tracer: None,
            read_pending_time: AccumulatedDuration::new(),
            write_pending_time: AccumulatedDuration::new(),
            rx_ts: None,
            connection_permit: ConnectionPermit::uncounted(),
        }
    }
}

impl From<virt::VirtualSocketStream> for Stream {
    fn from(s: virt::VirtualSocketStream) -> Self {
        Stream {
            stream: Some(BufferedStream::with_capacity(
                0,
                0,
                RawStreamWrapper::new(RawStream::Virtual(s)),
            )),
            rewind_read_buf: Vec::new(),
            buffer_write: true,
            established_ts: SystemTime::now(),
            establishment_duration: None,
            offload_wait_duration: None,
            proxy_digest: None,
            socket_digest: None,
            tracer: None,
            read_pending_time: AccumulatedDuration::new(),
            write_pending_time: AccumulatedDuration::new(),
            rx_ts: None,
            connection_permit: ConnectionPermit::uncounted(),
        }
    }
}

#[cfg(unix)]
impl From<UnixStream> for Stream {
    fn from(s: UnixStream) -> Self {
        Stream {
            stream: Some(BufferedStream::with_capacity(
                0,
                0,
                RawStreamWrapper::new(RawStream::Unix(s)),
            )),
            rewind_read_buf: Vec::new(),
            buffer_write: true,
            established_ts: SystemTime::now(),
            establishment_duration: None,
            offload_wait_duration: None,
            proxy_digest: None,
            socket_digest: None,
            tracer: None,
            read_pending_time: AccumulatedDuration::new(),
            write_pending_time: AccumulatedDuration::new(),
            rx_ts: None,
            connection_permit: ConnectionPermit::uncounted(),
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
            establishment_duration: self.establishment_duration,
            offload_wait_duration: self.offload_wait_duration,
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
            RawStream::Virtual(_) => {
                // TODO: should this do something?
                None
            }
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
mod read_buffer_tests {
    use super::*;
    use tokio::io::AsyncReadExt;
    use tokio::net::TcpListener;

    async fn tcp_pair(capacity: usize) -> (BufferedStream, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let (peer, accepted) = tokio::join!(
            TcpStream::connect(listener.local_addr().unwrap()),
            listener.accept()
        );
        let stream = RawStreamWrapper::new(RawStream::Tcp(accepted.unwrap().0));
        (
            BufferedStream::with_capacity(capacity, DEFAULT_L4_WRITE_BUFFER_SIZE, stream),
            peer.unwrap(),
        )
    }

    #[tokio::test]
    async fn idle_tcp_has_no_read_allocation_and_cancelled_read_can_resume() {
        let (mut stream, mut peer) = tcp_pair(DEFAULT_L4_READ_BUFFER_SIZE).await;
        assert_eq!(stream.read_buf.storage.capacity(), 0);
        let mut byte = [0; 1];
        for _ in 0..3 {
            assert!(stream.read(&mut byte).now_or_never().is_none());
            assert_eq!(stream.read_buf.storage.capacity(), 0);
        }
        peer.write_all(b"a").await.unwrap();
        stream.read_exact(&mut byte).await.unwrap();
        assert_eq!(&byte, b"a");
        assert_eq!(stream.read_buf.storage.capacity(), 0);
        peer.shutdown().await.unwrap();
        assert_eq!(stream.read(&mut byte).await.unwrap(), 0);
        assert_eq!(stream.read_buf.storage.capacity(), 0);
    }

    #[tokio::test]
    async fn draining_read_ahead_releases_storage_and_preserves_write_buffer() {
        let (mut stream, mut peer) = tcp_pair(DEFAULT_L4_READ_BUFFER_SIZE).await;
        stream.write_all(b"out").await.unwrap();
        assert_eq!(stream.writer.buffer(), b"out");
        peer.write_all(b"abcd").await.unwrap();

        let mut byte = [0; 1];
        stream.read_exact(&mut byte).await.unwrap();
        assert_eq!(&byte, b"a");
        assert_eq!(&stream.read_buf.storage[stream.read_pos..], b"bcd");
        let mut rest = [0; 3];
        stream.read_exact(&mut rest).await.unwrap();
        assert_eq!(&rest, b"bcd");
        assert_eq!(stream.read_buf.storage.capacity(), 0);
        assert_eq!(stream.writer.buffer(), b"out");

        stream.flush().await.unwrap();
        peer.read_exact(&mut rest).await.unwrap();
        assert_eq!(&rest, b"out");
        assert!(stream.read(&mut byte).now_or_never().is_none());
        assert_eq!(stream.read_buf.storage.capacity(), 0);
    }

    #[tokio::test]
    async fn full_read_ahead_reuses_storage_until_a_short_read() {
        let socket = std::io::Cursor::new(b"abcdefghijklmnopqrst".to_vec());
        let raw = RawStreamWrapper::new(RawStream::Virtual(virt::VirtualSocketStream::new(
            Box::new(socket),
        )));
        let mut stream = BufferedStream::with_capacity(8, 0, raw);
        let mut part = [0; 4];
        stream.read_exact(&mut part).await.unwrap();
        assert_eq!(&part, b"abcd");
        let allocation = stream.read_buf.storage.as_ptr();
        stream.read_exact(&mut part).await.unwrap();
        assert_eq!(&part, b"efgh");
        assert_eq!(stream.read_buf.storage.capacity(), 8);
        assert!(stream.read_buf.storage.is_empty());

        stream.read_exact(&mut part).await.unwrap();
        assert_eq!(&part, b"ijkl");
        assert_eq!(stream.read_buf.storage.as_ptr(), allocation);
        stream.read_exact(&mut part).await.unwrap();
        assert_eq!(&part, b"mnop");
        assert_eq!(stream.read_buf.storage.capacity(), 8);
        stream.read_exact(&mut part).await.unwrap();
        assert_eq!(&part, b"qrst");
        assert_eq!(stream.read_buf.storage.capacity(), 0);
    }

    #[tokio::test]
    async fn full_read_ahead_is_released_when_tcp_read_blocks() {
        let (mut stream, mut peer) = tcp_pair(4).await;
        peer.write_all(b"abcd").await.unwrap();
        let mut part = [0; 2];
        stream.read_exact(&mut part).await.unwrap();
        assert_eq!(&part, b"ab");
        stream.read_exact(&mut part).await.unwrap();
        assert_eq!(&part, b"cd");
        assert_eq!(stream.read_buf.storage.capacity(), 4);
        assert!(stream.read(&mut part).now_or_never().is_none());
        assert_eq!(stream.read_buf.storage.capacity(), 0);

        peer.write_all(b"ef").await.unwrap();
        stream.read_exact(&mut part).await.unwrap();
        assert_eq!(&part, b"ef");
        assert_eq!(stream.read_buf.storage.capacity(), 0);
    }

    #[tokio::test]
    async fn full_read_ahead_is_released_on_eof_or_error() {
        for error in [false, true] {
            let mut builder = tokio_test::io::Builder::new();
            builder.read(b"abcd");
            if error {
                builder.read_error(io::Error::from(io::ErrorKind::BrokenPipe));
            }
            let socket = builder.build();
            drop(builder);
            let raw = RawStreamWrapper::new(RawStream::Virtual(virt::VirtualSocketStream::new(
                Box::new(socket),
            )));
            let mut stream = BufferedStream::with_capacity(4, 0, raw);
            let mut part = [0; 2];
            stream.read_exact(&mut part).await.unwrap();
            assert_eq!(&part, b"ab");
            stream.read_exact(&mut part).await.unwrap();
            assert_eq!(&part, b"cd");
            assert_eq!(stream.read_buf.storage.capacity(), 4);
            let result = stream.read(&mut part).await;
            if error {
                assert_eq!(result.unwrap_err().kind(), io::ErrorKind::BrokenPipe);
            } else {
                assert_eq!(result.unwrap(), 0);
            }
            assert_eq!(stream.read_buf.storage.capacity(), 0);
        }
    }

    #[tokio::test]
    async fn full_read_ahead_is_released_when_bypassed() {
        let socket = std::io::Cursor::new(b"abcdefghijkl".to_vec());
        let raw = RawStreamWrapper::new(RawStream::Virtual(virt::VirtualSocketStream::new(
            Box::new(socket),
        )));
        let mut stream = BufferedStream::with_capacity(4, 0, raw);
        let mut part = [0; 2];
        stream.read_exact(&mut part).await.unwrap();
        stream.read_exact(&mut part).await.unwrap();
        assert_eq!(stream.read_buf.storage.capacity(), 4);
        let mut rest = [0; 8];
        stream.read_exact(&mut rest).await.unwrap();
        assert_eq!(&rest, b"efghijkl");
        assert_eq!(stream.read_buf.storage.capacity(), 0);
    }

    #[tokio::test]
    async fn zero_and_small_read_capacities_and_empty_destinations() {
        for capacity in [0, 1, 2, DEFAULT_L4_READ_BUFFER_SIZE] {
            let (mut stream, mut peer) = tcp_pair(capacity).await;
            assert_eq!(stream.read(&mut []).await.unwrap(), 0);
            assert_eq!(stream.read_buf.storage.capacity(), 0);
            peer.write_all(b"abcd").await.unwrap();
            let mut body = [0; 4];
            stream.read_exact(&mut body).await.unwrap();
            assert_eq!(&body, b"abcd");
            assert_eq!(stream.read_buf.storage.capacity(), 0);
        }
    }

    impl virt::VirtualSocket for tokio::io::DuplexStream {
        fn set_socket_option(&self, _opt: virt::VirtualSockOpt) -> io::Result<()> {
            Ok(())
        }
    }

    impl virt::VirtualSocket for std::io::Cursor<Vec<u8>> {
        fn set_socket_option(&self, _opt: virt::VirtualSockOpt) -> io::Result<()> {
            Ok(())
        }
    }

    impl virt::VirtualSocket for tokio_test::io::Mock {
        fn set_socket_option(&self, _opt: virt::VirtualSockOpt) -> io::Result<()> {
            Ok(())
        }
    }

    /// Compare the previous reader with the new policy using TLS-sized reads.
    /// Run alone: cargo test -p pingora-core --release benchmark_transport_read_buffers -- --ignored --nocapture
    #[tokio::test]
    #[ignore = "interactive read-buffer benchmark"]
    async fn benchmark_transport_read_buffers() {
        async fn transfer(mut reader: impl AsyncRead + Unpin) -> Duration {
            let start = Instant::now();
            let mut buf = [0; 16 * 1024];
            let mut total = 0;
            loop {
                let read = reader.read(&mut buf).await.unwrap();
                if read == 0 {
                    break;
                }
                total += read;
                std::hint::black_box(&buf[..read]);
            }
            assert_eq!(total, 128 * 1024 * 1024);
            start.elapsed()
        }

        fn source() -> RawStreamWrapper {
            RawStreamWrapper::new(RawStream::Virtual(virt::VirtualSocketStream::new(
                Box::new(std::io::Cursor::new(vec![b'x'; 128 * 1024 * 1024])),
            )))
        }

        for repetition in 1..=3 {
            let old = tokio::io::BufReader::with_capacity(
                DEFAULT_L4_READ_BUFFER_SIZE,
                BufWriter::with_capacity(DEFAULT_L4_WRITE_BUFFER_SIZE, source()),
            );
            let old = transfer(old).await;
            let new = BufferedStream::with_capacity(
                DEFAULT_L4_READ_BUFFER_SIZE,
                DEFAULT_L4_WRITE_BUFFER_SIZE,
                source(),
            );
            let new = transfer(new).await;
            println!(
                "transport/128MiB repetition={repetition} previous_ms={:.1} candidate_ms={:.1}",
                old.as_secs_f64() * 1000.0,
                new.as_secs_f64() * 1000.0,
            );
        }
    }

    #[tokio::test]
    async fn virtual_stream_releases_storage_on_pending_and_wakes_on_data() {
        let (socket, mut peer) = tokio::io::duplex(64);
        let stream = RawStreamWrapper::new(RawStream::Virtual(virt::VirtualSocketStream::new(
            Box::new(socket),
        )));
        let mut stream = BufferedStream::with_capacity(DEFAULT_L4_READ_BUFFER_SIZE, 0, stream);
        let mut byte = [0; 1];
        assert!(stream.read(&mut byte).now_or_never().is_none());
        assert_eq!(stream.read_buf.storage.capacity(), 0);
        let (read, write) = tokio::time::timeout(Duration::from_secs(1), async {
            tokio::join!(stream.read_exact(&mut byte), async {
                tokio::task::yield_now().await;
                peer.write_all(b"a").await
            })
        })
        .await
        .unwrap();
        read.unwrap();
        write.unwrap();
        assert_eq!(&byte, b"a");
        assert_eq!(stream.read_buf.storage.capacity(), 0);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unix_stream_waits_without_storage_and_resumes() {
        let (socket, mut peer) = UnixStream::pair().unwrap();
        let stream = RawStreamWrapper::new(RawStream::Unix(socket));
        let mut stream = BufferedStream::with_capacity(DEFAULT_L4_READ_BUFFER_SIZE, 0, stream);
        let mut byte = [0; 1];
        assert!(stream.read(&mut byte).now_or_never().is_none());
        assert_eq!(stream.read_buf.storage.capacity(), 0);
        peer.write_all(b"a").await.unwrap();
        stream.read_exact(&mut byte).await.unwrap();
        assert_eq!(&byte, b"a");
        assert_eq!(stream.read_buf.storage.capacity(), 0);
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
