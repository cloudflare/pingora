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

//! Quic Connector

use crate::protocols::l4::quic::Connection;
use crate::protocols::l4::quic::{detect_gso_pacing, Crypto, SocketDetails, MAX_IPV6_BUF_SIZE};
use log::{debug, error, trace};
use parking_lot::Mutex;
use pingora_error::{ErrorType, OrErr, Result};
use quiche::Connection as QuicheConnection;
use quiche::{h3, ConnectionId, RecvInfo};
use std::sync::Arc;
use tokio::net::UdpSocket;
use tokio::sync::Notify;
use tokio::task::JoinHandle;

/// corresponds to a new outgoing (connector) connection before the handshake is completed
pub struct HandshakeState {
    //pub(crate) connection_id: ConnectionId<'static>,
    pub(crate) socket_details: SocketDetails,
    pub(crate) crypto: Crypto,
}

/// can be used to wait for network data or trigger network sending
pub struct EstablishedState {
    pub(crate) connection_id: ConnectionId<'static>,
    pub(crate) connection: Arc<Mutex<QuicheConnection>>,

    pub(crate) http3_config: Arc<h3::Config>,

    /// is used to wait for new data received on the connection
    /// (e.g. after [`quiche::h3::Connection.poll()`] returned [`quiche::h3::Error::Done`])
    pub(crate) rx_notify: Arc<Notify>,
    /// is used to trigger a transmit loop which sends all connection data until [`quiche::h3::Error::Done`]
    pub(crate) tx_notify: Arc<Notify>,

    pub(crate) socket: Arc<UdpSocket>,
    /// handle for the ConnectionTx task
    pub(crate) tx_handle: JoinHandle<Result<()>>,
    /// handle for the ConnectionRx task
    pub(crate) rx_handle: JoinHandle<Result<()>>,
}

impl Connection {
    pub fn initiate(io: UdpSocket) -> Result<Self> {
        let local_addr = io.local_addr().explain_err(ErrorType::SocketError, |e| {
            format!("failed to get local address from socket: {}", e)
        })?;
        let peer_addr = io.peer_addr().explain_err(ErrorType::SocketError, |e| {
            format!("failed to get peer address from socket: {}", e)
        })?;

        let (gso_enabled, pacing_enabled) = detect_gso_pacing(&io);
        Ok(Self::OutgoingHandshake(HandshakeState {
            crypto: Crypto::new()?, // TODO:: custom crypto or cid generation/validation
            socket_details: SocketDetails {
                io: Arc::new(io),
                local_addr,
                peer_addr: Some(peer_addr),
                gso_enabled,
                pacing_enabled,
            },
        }))
    }
}

/// connections receive task receives data from the UDP socket into the [`quiche::Connection`]
/// the task notifies the `rx_notify` when data was received from network for the connection
pub struct ConnectionRx {
    pub(crate) socket_details: SocketDetails,

    pub(crate) connection_id: ConnectionId<'static>,
    pub(crate) connection: Arc<Mutex<QuicheConnection>>,

    pub(crate) rx_notify: Arc<Notify>,
    pub(crate) tx_notify: Arc<Notify>,
}

impl ConnectionRx {
    /// start the `rx` task, consumes the struct
    ///
    /// is stopped within the `Drop` implementation of the corresponding [`Connection`]
    pub async fn start(self) -> Result<()> {
        let socket = self.socket_details.io;
        let local_addr = self.socket_details.local_addr;
        let conn_id = self.connection_id;

        // support ip switching on local & peer address would require socket re-binding
        let mut buf = [0u8; MAX_IPV6_BUF_SIZE];
        debug!("connection {:?} rx read", conn_id);
        'read: loop {
            // TODO: replace with recv.await()
            let (size, recv_info) = match socket.try_recv_from(&mut buf) {
                Ok((size, from)) => {
                    trace!(
                        "connection {:?} network received from={} length={}",
                        conn_id,
                        from,
                        size
                    );
                    let recv_info = RecvInfo {
                        from,
                        to: local_addr,
                    };
                    (size, recv_info)
                }
                Err(e) => {
                    if e.kind() == std::io::ErrorKind::WouldBlock {
                        socket
                            .readable()
                            .await
                            .explain_err(ErrorType::ReadError, |_| {
                                "failed to wait for readable network socket"
                            })?;
                        continue 'read;
                    }
                    return Err(e).explain_err(ErrorType::ReadError, |_| {
                        "failed to receive from network socket"
                    })?;
                }
            };
            {
                let mut conn = self.connection.lock();
                match conn.recv(&mut buf[..size], recv_info) {
                    Ok(_size) => {
                        debug!("connection {:?} received {}", conn_id, size);
                        self.rx_notify.notify_waiters();
                        self.tx_notify.notify_waiters();
                    }
                    Err(e) => {
                        // If an error occurs while processing data, the connection is closed with
                        // the appropriate error code, using the transport’s close() method.

                        // send the close() event
                        self.tx_notify.notify_waiters();

                        error!("H3 connection {:?} closed with error {:?}.", conn_id, e);
                        return Err(e).explain_err(ErrorType::ReadError, |_| {
                            "failed to receive data from socket on connection"
                        });
                    }
                }
            }
        }
    }
}

impl Drop for EstablishedState {
    fn drop(&mut self) {
        if !self.rx_handle.is_finished() {
            self.rx_handle.abort();
            debug!("connection {:?} stopped rx task", self.connection_id)
        }
        if !self.tx_handle.is_finished() {
            self.tx_handle.abort();
            debug!("connection {:?} stopped rx task", self.connection_id)
        }
    }
}
