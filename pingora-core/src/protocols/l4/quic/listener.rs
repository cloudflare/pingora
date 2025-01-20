use crate::protocols::l4::quic::{
    detect_gso_pacing, Connection, ConnectionHandle, Crypto, HandshakeResponse, IncomingHandle,
    IncomingState, SocketDetails, UdpRecv, CONNECTION_DROP_DEQUE_INITIAL_SIZE,
    HANDSHAKE_PACKET_BUFFER_SIZE, MAX_IPV6_BUF_SIZE,
};
use log::{debug, error, trace, warn};
use parking_lot::Mutex;
use pingora_error::{BError, ErrorType, OrErr};
use quiche::{ConnectionId, Header, RecvInfo, Type};
use ring::hmac::Key;
use std::collections::{HashMap, VecDeque};
use std::fmt::{Debug, Formatter};
use std::io;
use std::io::ErrorKind;
use std::net::SocketAddr;
use std::os::fd::{AsRawFd, RawFd};
use std::sync::Arc;
use tokio::net::UdpSocket;
use tokio::sync::mpsc::channel;

use crate::protocols::l4::quic::QuicHttp3Configs;
use quiche::Connection as QuicheConnection;

/// The [`Listener`] contains a [`HashMap`] linking [`quiche::ConnectionId`] to [`ConnectionHandle`]
/// the `Listener::accept` method returns [`Connection`]s and is responsible to forward network
/// UDP packets to the according `Connection` through the corresponding [`ConnectionHandle`].
///
/// In the [`ConnectionHandle::Incoming`] state the UDP packets are forwarded through a
/// [`tokio::sync::mpsc::channel`].
// Once the state is [`ConnectionHandle::Established`] the packets are directly received on
// the [`quiche::Connection`].
pub struct Listener {
    socket_details: SocketDetails,

    configs: QuicHttp3Configs,
    crypto: Crypto,

    connections: HashMap<ConnectionId<'static>, ConnectionHandle>,
    drop_connections: Arc<Mutex<VecDeque<ConnectionId<'static>>>>,
}

impl Listener {
    pub(crate) async fn accept(
        &mut self,
    ) -> io::Result<(crate::protocols::l4::stream::Stream, SocketAddr)> {
        let mut rx_buf = [0u8; MAX_IPV6_BUF_SIZE];

        debug!("endpoint rx loop");
        'read: loop {
            // receive from network and parse Quic header
            let (size, from) = match self.socket_details.io.try_recv_from(&mut rx_buf) {
                Ok((size, from)) => (size, from),
                Err(e) => {
                    if e.kind() == ErrorKind::WouldBlock {
                        // no more UDP packets to read for now, wait  for new packets
                        self.socket_details.io.readable().await?;
                        continue 'read;
                    } else {
                        return Err(e);
                    }
                }
            };

            // cleanup connections
            {
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
                to: self.socket_details.addr,
                from,
            };

            let mut conn_id = header.dcid.clone();
            let mut udp_tx = None;
            let mut established_handle = None;
            // send to corresponding connection
            let mut handle;
            handle = self.connections.get_mut(&conn_id);
            if handle.is_none() {
                conn_id = Self::gen_cid(&self.crypto.key, &header);
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
                    ConnectionHandle::Incoming(i) => {
                        let resp;
                        {
                            resp = i.response.lock().take();
                        }
                        if let Some(resp) = resp {
                            match resp {
                                HandshakeResponse::Established(e) => {
                                    debug!(
                                        "connection {:?} received HandshakeResponse::Established",
                                        conn_id
                                    );
                                    established_handle = Some(e.clone());
                                    needs_establish = Some(e);
                                }
                                HandshakeResponse::Ignored => {
                                    // drop connection
                                    self.connections.remove(&header.dcid);
                                    continue 'read;
                                }
                            }
                        } else {
                            udp_tx = Some(i.udp_tx.clone());
                        }
                    }
                    ConnectionHandle::Established(e) => {
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

            // receive data on UDP channel
            if let Some(udp_tx) = udp_tx {
                match udp_tx
                    .send(UdpRecv {
                        pkt: rx_buf[..size].to_vec(),
                        header,
                        recv_info,
                    })
                    .await
                {
                    Ok(()) => {}
                    Err(e) => warn!(
                        "sending dgram to connection {:?} failed with error: {}",
                        conn_id, e
                    ),
                }
                continue 'read;
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
            let connection = Connection::Incoming(IncomingState {
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
            let handle = ConnectionHandle::Incoming(IncomingHandle { udp_tx, response });

            self.connections.insert(conn_id, handle);
            return Ok((connection.into(), from));
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
                error!("connection {:?} receive error {:?}", conn_id, e);
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

    fn gen_cid(key: &Key, hdr: &Header) -> ConnectionId<'static> {
        let conn_id = ring::hmac::sign(key, &hdr.dcid);
        let conn_id = conn_id.as_ref()[..quiche::MAX_CONN_ID_LEN].to_vec();
        let conn_id = ConnectionId::from(conn_id);
        trace!("generated connection id {:?}", conn_id);
        conn_id
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
                addr,
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
