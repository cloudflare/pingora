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

//! QUIC Listener using cloudflare/quiche

use crate::protocols::l4::stream::Stream as L4Stream;
use crate::protocols::{ConnectionState as ConnectionStateTrait, QuicConnectionState};
use parking_lot::Mutex;
use pingora_error::{Error, ErrorType, Result};
use std::fmt::{Debug, Formatter};
use std::io;
use std::net::SocketAddr;
use std::os::fd::{AsRawFd, RawFd};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::UdpSocket;

pub struct Listener {
    socket: Arc<UdpSocket>,
}

impl From<UdpSocket> for Listener {
    fn from(io: UdpSocket) -> Self {
        Listener {
            socket: Arc::new(io),
        }
    }
}

impl Listener {
    pub(crate) async fn accept(&self) -> std::io::Result<(L4Stream, SocketAddr)> {
        // TODO: SocketAddr should be remote addr
        let addr = self.socket.local_addr()?;

        Ok((
            QuicConnection {
                socket: self.socket.clone(),
                state: Arc::new(Mutex::new(QuicConnectionState::Incoming(
                    PreHandshakeState {
                        socket: self.socket.clone(),
                    },
                ))),
            }
            .into(),
            addr,
        ))
    }

    pub(super) fn get_raw_fd(&self) -> RawFd {
        self.socket.as_raw_fd()
    }
}

impl AsRawFd for QuicConnection {
    fn as_raw_fd(&self) -> RawFd {
        self.socket.as_raw_fd()
    }
}

impl Debug for Listener {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Listener")
            .field("io", &self.socket)
            .finish()
    }
}

pub(crate) struct QuicConnection {
    socket: Arc<UdpSocket>,
    state: Arc<Mutex<QuicConnectionState>>,
}

impl QuicConnection {
    pub(crate) fn local_addr(&self) -> io::Result<SocketAddr> {
        self.socket.local_addr()
    }
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
    ) -> Poll<Result<usize, std::io::Error>> {
        todo!()
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        todo!()
    }

    fn poll_shutdown(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), io::Error>> {
        todo!()
    }
}

#[allow(unused_variables)] // TODO: remove
impl AsyncRead for QuicConnection {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        todo!()
    }
}

#[allow(unused)] // TODO: remove
pub enum ConnectionState {
    Incoming(PreHandshakeState),
    Established(EstablishedState),
}

#[allow(unused)] // TODO: remove
pub struct PreHandshakeState {
    socket: Arc<UdpSocket>,
}

#[allow(unused)] // TODO: remove
pub struct EstablishedState {
    socket: Arc<UdpSocket>,
}

impl ConnectionStateTrait for QuicConnection {
    fn quic_connection_state(&self) -> Option<Arc<Mutex<QuicConnectionState>>> {
        Some(self.state.clone())
    }
}

impl Into<Option<PreHandshakeState>> for ConnectionState {
    fn into(self) -> Option<PreHandshakeState> {
        match self {
            ConnectionState::Incoming(s) => Some(s),
            ConnectionState::Established(_) => None,
        }
    }
}

impl Into<Option<EstablishedState>> for ConnectionState {
    fn into(self) -> Option<EstablishedState> {
        match self {
            ConnectionState::Incoming(_) => None,
            ConnectionState::Established(s) => Some(s),
        }
    }
}

pub(crate) fn handshake(state: Arc<Mutex<ConnectionState>>) -> Result<()> {
    let state = state.lock();

    let Some(_s) = state.into() else {
        debug_assert!(false, "quic::handshake on already established connection");
        return Err(Error::new(ErrorType::HandshakeError));
    };

    Ok(())
}
