//! Provides [`VirtualSocketStream`].

use std::{
    pin::Pin,
    sync::atomic::{AtomicU32, Ordering},
    task::{Context, Poll},
};

use tokio::io::{AsyncRead, AsyncWrite};

use super::ext::TcpKeepalive;
use crate::protocols::UniqueIDType;

/// A limited set of socket options that can be set on a [`VirtualSocket`].
#[non_exhaustive]
#[derive(Debug, Clone)]
pub enum VirtualSockOpt {
    NoDelay,
    KeepAlive(TcpKeepalive),
}

/// A "virtual" socket that supports async read and write operations.
pub trait VirtualSocket: AsyncRead + AsyncWrite + Unpin + Send + Sync + std::fmt::Debug {
    /// Set a socket option.
    fn set_socket_option(&self, opt: VirtualSockOpt) -> std::io::Result<()>;
}

/// Wrapper around any type implementing  [`VirtualSocket`].
#[derive(Debug)]
pub struct VirtualSocketStream {
    pub(crate) socket: Box<dyn VirtualSocket>,
    id: UniqueIDType,
}

impl VirtualSocketStream {
    pub fn new(socket: Box<dyn VirtualSocket>) -> Self {
        Self {
            socket,
            id: next_id(),
        }
    }

    #[inline]
    pub fn set_socket_option(&self, opt: VirtualSockOpt) -> std::io::Result<()> {
        self.socket.set_socket_option(opt)
    }

    /// The unique ID of this stream.
    ///
    /// Virtual streams have no real fd, so IDs are allocated from a range no
    /// real fd or socket handle can occupy. This keeps pooled virtual streams
    /// distinguishable from each other and from real connections.
    pub fn id(&self) -> UniqueIDType {
        self.id
    }
}

/// Allocate IDs counting down from -2: real fds are non-negative and -1 is the
/// "no fd" sentinel, so this range can never collide with either.
#[cfg(unix)]
fn next_id() -> UniqueIDType {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    // stay within [0, i32::MAX - 1] so the ID below stays within [i32::MIN, -2]
    let n = NEXT.fetch_add(1, Ordering::Relaxed) % (i32::MAX as u32);
    -2 - (n as i32)
}

/// Allocate IDs counting down from usize::MAX - 1: real socket handles are
/// small values and usize::MAX is INVALID_SOCKET, so this range can never
/// collide with either.
#[cfg(windows)]
fn next_id() -> UniqueIDType {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    usize::MAX - 1 - (NEXT.fetch_add(1, Ordering::Relaxed) as usize)
}

impl AsyncRead for VirtualSocketStream {
    #[inline]
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut *self.get_mut().socket).poll_read(cx, buf)
    }
}

impl AsyncWrite for VirtualSocketStream {
    #[inline]
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut *self.get_mut().socket).poll_write(cx, buf)
    }

    #[inline]
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut *self.get_mut().socket).poll_flush(cx)
    }

    #[inline]
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut *self.get_mut().socket).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use tokio::io::{AsyncReadExt, AsyncWriteExt as _};

    use crate::protocols::l4::stream::Stream;

    use super::*;

    #[derive(Debug)]
    struct StaticVirtualSocket {
        content: Vec<u8>,
        read_pos: usize,
        write_buf: Arc<Mutex<Vec<u8>>>,
    }

    impl AsyncRead for StaticVirtualSocket {
        fn poll_read(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &mut tokio::io::ReadBuf<'_>,
        ) -> Poll<std::io::Result<()>> {
            debug_assert!(self.read_pos <= self.content.len());

            let remaining = self.content.len() - self.read_pos;
            if remaining == 0 {
                return Poll::Ready(Ok(()));
            }

            let to_read = std::cmp::min(remaining, buf.remaining());
            buf.put_slice(&self.content[self.read_pos..self.read_pos + to_read]);
            self.read_pos += to_read;

            Poll::Ready(Ok(()))
        }
    }

    impl AsyncWrite for StaticVirtualSocket {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            // write to internal buffer
            let this = self.get_mut();
            this.write_buf.lock().unwrap().extend_from_slice(buf);
            Poll::Ready(Ok(buf.len()))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    impl VirtualSocket for StaticVirtualSocket {
        fn set_socket_option(&self, _opt: VirtualSockOpt) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Basic test that ensures reading and writing works with a virtual socket.
    //
    /// Mostly just ensures that construction works and the plumbing is correct.
    #[tokio::test]
    async fn test_stream_virtual() {
        let content = b"hello virtual world";
        let write_buf = Arc::new(Mutex::new(Vec::new()));
        let mut stream = Stream::from(VirtualSocketStream::new(Box::new(StaticVirtualSocket {
            content: content.to_vec(),
            read_pos: 0,
            write_buf: write_buf.clone(),
        })));

        let mut buf = Vec::new();
        let out = stream.read_to_end(&mut buf).await.unwrap();
        assert_eq!(out, content.len());
        assert_eq!(buf, content);

        stream.write_all(content).await.unwrap();
        assert_eq!(write_buf.lock().unwrap().as_slice(), content);
    }
}
