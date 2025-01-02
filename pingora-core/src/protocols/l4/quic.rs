use crate::protocols::l4::stream::Stream as L4Stream;
use std::fmt::{Debug, Formatter};
use std::io;
use std::io::Error;
use std::net::SocketAddr;
use std::os::fd::{AsRawFd, RawFd};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::UdpSocket;

pub struct Listener {
    io: Arc<UdpSocket>,
}

impl From<UdpSocket> for Listener {
    fn from(io: UdpSocket) -> Self {
        Listener { io: Arc::new(io) }
    }
}

impl Listener {
    pub(crate) async fn accept(&self) -> io::Result<(L4Stream, SocketAddr)> {
        // TODO: SocketAddr should be remote addr
        let addr = self.io.local_addr()?;

        Ok((
            QuicConnection {
                io: self.io.clone(),
            }
            .into(),
            addr,
        ))
    }

    pub(super) fn get_raw_fd(&self) -> RawFd {
        self.io.as_raw_fd()
    }
}

impl AsRawFd for QuicConnection {
    fn as_raw_fd(&self) -> RawFd {
        self.io.as_raw_fd()
    }
}

impl Debug for Listener {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Listener").field("io", &self.io).finish()
    }
}

pub(crate) struct QuicConnection {
    pub(crate) io: Arc<UdpSocket>,
}

impl Debug for QuicConnection {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QuicConnection").finish()
    }
}

#[allow(unused_variables)] // TODO: remove
impl AsyncWrite for QuicConnection {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, Error>> {
        todo!()
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
        todo!()
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
        todo!()
    }
}

#[allow(unused_variables)] // TODO: remove
impl AsyncRead for QuicConnection {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        todo!()
    }
}
