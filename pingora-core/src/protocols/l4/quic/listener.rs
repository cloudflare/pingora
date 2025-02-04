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

//! Quic Listener

use crate::protocols::l4::quic::id_token::generate_incoming_cid;
use crate::protocols::l4::quic::QuicHttp3Configs;
use crate::protocols::l4::quic::{
    detect_gso_pacing, Connection, Crypto, SocketDetails, CONNECTION_DROP_DEQUE_INITIAL_SIZE,
    MAX_IPV6_BUF_SIZE,
};
use crate::protocols::l4::stream::Stream;
use log::{debug, trace, warn};
use parking_lot::Mutex;
use pingora_error::{BError, ErrorType, OrErr};
use quiche::{h3, Connection as QuicheConnection, ConnectionId, Header, RecvInfo, Type};
use std::collections::{HashMap, VecDeque};
use std::fmt::{Debug, Formatter};
use std::io::ErrorKind;
use std::net::SocketAddr;
use std::os::fd::{AsRawFd, RawFd};
use std::sync::Arc;
use std::time::Duration;
use std::{io, mem};
use tokio::net::UdpSocket;
use tokio::sync::mpsc::{channel, Receiver, Sender};
use tokio::sync::Notify;
use tokio::task::JoinHandle;

/// max. amount of [`UdpRecv`] messages on the `tokio::sync::mpsc::channel`
const HANDSHAKE_PACKET_BUFFER_SIZE: usize = 64;

/// corresponds to a new incoming (listener) connection before the handshake is completed
pub struct HandshakeState {
    pub(crate) connection_id: ConnectionId<'static>,
    pub(crate) configs: QuicHttp3Configs,
    pub(crate) drop_connection: Arc<Mutex<VecDeque<ConnectionId<'static>>>>,

    pub(crate) socket_details: SocketDetails,
    pub(crate) udp_rx: Receiver<UdpRecv>,
    pub(crate) response: Arc<Mutex<Option<HandshakeResponse>>>,

    pub(crate) dgram: UdpRecv,

    pub(crate) ignore: bool,
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
    pub(crate) tx_handle: JoinHandle<pingora_error::Result<()>>,
    pub(crate) drop_connection: Arc<Mutex<VecDeque<ConnectionId<'static>>>>,
}

/// A [`IncomingConnectionHandle`] corresponds to an Incoming [`Connection`].
/// For further details please refer to [`Connection`].
pub enum IncomingConnectionHandle {
    /// new connection handle during handshake
    Handshake(HandshakeHandle),
    /// transitioned once the handshake is successful ([`quiche::Connection::is_established`])
    Established(EstablishedHandle),
}

impl Debug for IncomingConnectionHandle {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_str("IncomingConnectionHandle")?;
        match self {
            IncomingConnectionHandle::Handshake(_) => f.write_str("::Handshake"),
            IncomingConnectionHandle::Established(_) => f.write_str("::Established"),
        }
    }
}

/// used to forward data from the UDP socket during the handshake
pub struct HandshakeHandle {
    udp_tx: Sender<UdpRecv>,
    response: Arc<Mutex<Option<HandshakeResponse>>>,
}

pub(crate) enum HandshakeResponse {
    Established(EstablishedHandle),
    Ignored,
    // TODO: TimedOut
}

/// is used to forward data from the UDP socket to the Quic connection
#[derive(Clone)]
pub struct EstablishedHandle {
    pub(crate) connection_id: ConnectionId<'static>,
    pub(crate) connection: Arc<Mutex<QuicheConnection>>,
    pub(crate) rx_notify: Arc<Notify>,
    pub(crate) tx_notify: Arc<Notify>,
}

/// the message format used on the [`tokio::sync::mpsc::channel`] during the handshake phase
pub struct UdpRecv {
    pub(crate) pkt: Vec<u8>,
    pub(crate) header: Header<'static>,
    pub(crate) recv_info: RecvInfo,
}

/// The [`Listener`] contains a [`HashMap`] linking [`quiche::ConnectionId`] to [`IncomingConnectionHandle`]
/// the `Listener::accept` method returns Incoming [`Connection`]s and is responsible to forward
/// network UDP packets to the according `Incoming [`Connection`] through the corresponding
/// [`IncomingConnectionHandle`].
///
/// In the [`IncomingConnectionHandle::Handshake`] state the UDP packets are forwarded through a
/// [`tokio::sync::mpsc::channel`].
// Once the state is [`ConnectionHandle::Established`] the packets are directly received on
// the [`quiche::Connection`].
pub struct Listener {
    socket_details: SocketDetails,

    configs: QuicHttp3Configs,
    crypto: Crypto,

    connections: HashMap<ConnectionId<'static>, IncomingConnectionHandle>,
    drop_connections: Arc<Mutex<VecDeque<ConnectionId<'static>>>>,
}

impl Listener {
    pub(crate) async fn accept(&mut self) -> io::Result<(Stream, SocketAddr)> {
        let mut rx_buf = [0u8; MAX_IPV6_BUF_SIZE];

        debug!("endpoint rx loop");
        'read: loop {
            // receive from network and parse Quic header
            let (size, from) = tokio::select! {
                biased;
                res = self.socket_details.io.recv_from(&mut rx_buf) => { res? },
                _housekeeping_tick = tokio::time::sleep(Duration::from_millis(100)) => {
                    // avoid stuck connections when traffic stops
                    self.housekeeping_connections_drop();
                    continue;
                }
            };

            self.housekeeping_connections_drop();

            // parse the Quic packet's header
            let header = match Header::from_slice(rx_buf[..size].as_mut(), quiche::MAX_CONN_ID_LEN)
            {
                Ok(hdr) => hdr,
                Err(e) => {
                    warn!("Parsing Quic packet header failed with error: {:?}.", e);
                    trace!("Dropped packet due to invalid header. Continuing...");
                    continue 'read;
                }
            };

            // TODO: allow for connection id updates during lifetime
            // connection needs to be able to update source_ids() or destination_ids()

            let recv_info = RecvInfo {
                to: self.socket_details.local_addr,
                from,
            };

            let mut conn_id = header.dcid.clone();
            let mut established_handle = None;
            // send to corresponding connection
            let mut handle;
            handle = self.connections.get_mut(&conn_id);
            if handle.is_none() {
                conn_id = generate_incoming_cid(&self.crypto.key, &header);
                handle = self.connections.get_mut(&conn_id);
            };

            trace!(
                "connection {:?} network received from={} length={}",
                conn_id,
                from,
                size
            );

            if let Some(handle) = handle {
                debug!(
                    "existing connection {:?} {:?} {:?}",
                    conn_id, handle, header
                );
                let mut needs_establish = None;
                match handle {
                    IncomingConnectionHandle::Handshake(i) => {
                        {
                            // hold on to the lock while writing to udp_tx to avoid dropping packets
                            // during establishing the connection
                            let mut resp = i.response.lock();
                            if let Some(resp) = resp.take() {
                                match resp {
                                    HandshakeResponse::Established(e) => {
                                        debug!(
                                            "connection {:?} received HandshakeResponse::Established",
                                            conn_id
                                        );
                                        // receive data into existing connection
                                        established_handle = Some(e.clone());
                                        needs_establish = Some(e);
                                    }
                                    HandshakeResponse::Ignored => {
                                        // drop connection
                                        //self.connections.remove(&header.dcid);
                                        let mut drop_connections = self.drop_connections.lock();
                                        drop_connections.push_back(header.dcid);
                                        continue 'read;
                                    }
                                }
                            } else {
                                // receive data on UDP channel
                                // use try_send as sync method to avoid await point while holding lock
                                match i.udp_tx
                                    .try_send(UdpRecv {
                                        pkt: rx_buf[..size].to_vec(),
                                        header,
                                        recv_info,
                                    })
                                {
                                    Ok(()) => {}
                                    Err(e) => warn!(
                                        "sending dgram to connection {:?} failed with error: {}, dropping dgram",
                                        conn_id, e
                                    ),
                                }
                                continue 'read;
                            }
                        }
                    }
                    IncomingConnectionHandle::Established(e) => {
                        established_handle = Some(e.clone());
                    }
                }
                if let Some(e) = needs_establish {
                    handle.establish(e)
                }
            };

            // receive data into existing connection
            if let Some(e) = established_handle {
                match Self::recv_connection(
                    &conn_id,
                    e.connection.as_ref(),
                    &mut rx_buf[..size],
                    recv_info,
                ) {
                    Ok(_len) => {
                        e.rx_notify.notify_waiters();
                        e.tx_notify.notify_waiters();
                        // TODO: handle path events
                        continue 'read;
                    }
                    Err(e) => {
                        // TODO: take action on errors, e.g close connection, send & remove
                        break 'read Err(e);
                    }
                }
            }

            if header.ty != Type::Initial {
                debug!(
                    "Quic packet type is not \"Initial\". Header: {:?}. Continuing...",
                    header
                );
                continue 'read;
            }

            // create incoming connection & handle
            let (udp_tx, udp_rx) = channel::<UdpRecv>(HANDSHAKE_PACKET_BUFFER_SIZE);
            let response = Arc::new(Mutex::new(None));

            debug!("new incoming connection {:?}", conn_id);
            let connection = Connection::IncomingHandshake(HandshakeState {
                connection_id: conn_id.clone(),
                drop_connection: self.drop_connections.clone(),

                configs: self.configs.clone(),

                socket_details: self.socket_details.clone(),
                udp_rx,
                response: response.clone(),

                dgram: UdpRecv {
                    pkt: rx_buf[..size].to_vec(),
                    header,
                    recv_info,
                },

                ignore: false,
            });
            let handle = IncomingConnectionHandle::Handshake(HandshakeHandle { udp_tx, response });

            self.connections.insert(conn_id, handle);
            return Ok((connection.into(), from));
        }
    }

    fn housekeeping_connections_drop(&mut self) {
        // cleanup connections
        let mut drop_conn = self.drop_connections.lock();
        while let Some(drop_id) = drop_conn.pop_front() {
            match self.connections.remove(&drop_id) {
                None => warn!(
                    "failed to remove connection handle {:?} from connections",
                    drop_id
                ),
                Some(_) => {
                    debug!("removed connection handle {:?} from connections", drop_id)
                }
            }
        }
    }

    fn recv_connection(
        conn_id: &ConnectionId<'_>,
        conn: &Mutex<QuicheConnection>,
        rx_buf: &mut [u8],
        recv_info: RecvInfo,
    ) -> io::Result<usize> {
        let size = rx_buf.len();
        let mut conn = conn.lock();
        match conn.recv(rx_buf, recv_info) {
            Ok(len) => {
                debug!("connection {:?} received data length={}", conn_id, len);
                debug_assert_eq!(
                    size, len,
                    "size received on connection not equal to len received from network."
                );
                Ok(len)
            }
            Err(e) => {
                trace!("connection {:?} receive error {:?}", conn_id, e);
                Err(io::Error::new(
                    ErrorKind::BrokenPipe,
                    format!(
                        "Connection could not receive network data for {:?}. {:?}",
                        conn.destination_id(),
                        e
                    ),
                ))
            }
        }
    }

    pub(crate) fn get_raw_fd(&self) -> RawFd {
        self.socket_details.io.as_raw_fd()
    }
}

impl TryFrom<(UdpSocket, QuicHttp3Configs)> for Listener {
    type Error = BError;

    fn try_from(
        (io, configs): (UdpSocket, QuicHttp3Configs),
    ) -> pingora_error::Result<Self, Self::Error> {
        let addr = io.local_addr().explain_err(ErrorType::SocketError, |e| {
            format!("failed to get local address from socket: {}", e)
        })?;

        let (gso_enabled, pacing_enabled) = detect_gso_pacing(&io);

        Ok(Listener {
            socket_details: SocketDetails {
                io: Arc::new(io),
                local_addr: addr,
                peer_addr: None,
                gso_enabled,
                pacing_enabled,
            },

            configs,
            crypto: Crypto::new()?,

            connections: Default::default(),
            drop_connections: Arc::new(Mutex::new(VecDeque::with_capacity(
                CONNECTION_DROP_DEQUE_INITIAL_SIZE,
            ))),
        })
    }
}

impl Debug for Listener {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Listener")
            .field("io", &self.socket_details.io)
            .finish()
    }
}

impl IncomingConnectionHandle {
    fn establish(&mut self, handle: EstablishedHandle) {
        match self {
            IncomingConnectionHandle::Handshake(_) => {
                debug!("connection handle {:?} established", handle.connection_id);
                let _ = mem::replace(self, IncomingConnectionHandle::Established(handle));
            }
            IncomingConnectionHandle::Established(_) => {}
        }
    }
}
