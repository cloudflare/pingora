use std::collections::VecDeque;
use std::io;
use std::io::ErrorKind;
use std::net::SocketAddr;
use std::os::fd::{AsRawFd, RawFd};
use std::sync::Arc;
use log::{debug, error, trace, warn};
use parking_lot::Mutex;
use quiche::{ConnectionId, Header, RecvInfo, Type};
use ring::hmac::Key;
use ring::rand::SystemRandom;
use tokio::net::UdpSocket;
use tokio::sync::mpsc::channel;
use pingora_error::{BError, Error, ErrorType};
use crate::protocols::l4::quic::{Connection, ConnectionHandle, Crypto, HandshakeResponse, IncomingHandle, IncomingState, Listener, SocketDetails, UdpRecv, CONNECTION_DROP_DEQUE_INITIAL_SIZE, HANDSHAKE_PACKET_BUFFER_SIZE, MAX_IPV6_BUF_SIZE, MAX_IPV6_QUIC_DATAGRAM_SIZE};
use crate::protocols::l4::quic::sendto::{detect_gso, set_txtime_sockopt};

use quiche::Connection as QuicheConnection;

impl TryFrom<UdpSocket> for Listener {
    type Error = BError;

    fn try_from(io: UdpSocket) -> pingora_error::Result<Self, Self::Error> {
        let addr = io.local_addr()
            .map_err(|e| Error::explain(
                ErrorType::SocketError,
                format!("failed to get local address from socket: {}", e)))?;
        let rng = SystemRandom::new();
        let key = Key::generate(ring::hmac::HMAC_SHA256, &rng)
            .map_err(|e| Error::explain(
                ErrorType::InternalError,
                format!("failed to generate listener key: {}", e)))?;

        let settings = crate::protocols::l4::quic::settings::Settings::try_default()?;

        let gso_enabled = detect_gso(&io, MAX_IPV6_QUIC_DATAGRAM_SIZE);
        let pacing_enabled = match set_txtime_sockopt(&io) {
            Ok(_) => {
                debug!("successfully set SO_TXTIME socket option");
                true
            },
            Err(e) => {
                debug!("setsockopt failed {:?}", e);
                false
            },
        };

        Ok(Listener {
            socket: Arc::new(io),
            socket_details: SocketDetails {
                addr,
                gso_enabled,
                pacing_enabled,
            },

            config: settings.get_config(),
            crypto: Crypto {
                key
            },

            connections: Default::default(),
            drop_connections: Arc::new(Mutex::new(VecDeque::with_capacity(CONNECTION_DROP_DEQUE_INITIAL_SIZE)))
        })
    }
}

impl Listener {
    pub(crate) async fn accept(&mut self) -> io::Result<(crate::protocols::l4::stream::Stream, SocketAddr)> {
        let mut rx_buf = [0u8; MAX_IPV6_BUF_SIZE];

        debug!("endpoint rx loop");
        'read: loop {
            // receive from network and parse Quic header
            let (size, from) = match self.socket.try_recv_from(&mut rx_buf) {
                Ok((size, from)) => (size, from),
                Err(e) => {
                    if e.kind() == ErrorKind::WouldBlock {
                        // no more UDP packets to read for now, wait  for new packets
                        self.socket.readable().await?;
                        continue 'read;
                    } else {
                        return Err(e)
                    }
                }
            };

            // cleanup connections
            {
                let mut drop_conn = self.drop_connections.lock();
                while let Some(drop_id) = drop_conn.pop_front() {
                    match self.connections.remove(&drop_id) {
                        None => warn!("failed to remove connection handle {:?} from connections", drop_id),
                        Some(_) => debug!("removed connection handle {:?} from connections", drop_id)
                    }
                }
            }

            // parse the Quic packet's header
            let header = match Header::from_slice(rx_buf[..size].as_mut(), quiche::MAX_CONN_ID_LEN) {
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

            // send to corresponding connection
            let mut handle;
            handle = self.connections.get_mut(&conn_id);
            if handle.is_none() {
                conn_id = Self::gen_cid(&self.crypto.key, &header);
                handle = self.connections.get_mut(&conn_id);
            };

            trace!("connection {:?} network received from={} length={}", conn_id, from, size);

            if let Some(handle) = handle {
                debug!("existing connection {:?} {:?} {:?}", conn_id, handle, header);
                let mut established_handle = None;
                match handle {
                    ConnectionHandle::Incoming(i) => {
                        let resp;
                        {
                            resp = i.response.lock().take();
                        }
                        if let Some(resp) = resp {
                            match resp {
                                HandshakeResponse::Established(e) => {
                                    debug!("connection {:?} received HandshakeResponse::Established", conn_id);
                                    // receive data into existing connection
                                    established_handle = Some(e);
                                }
                                HandshakeResponse::Ignored
                                | HandshakeResponse::Rejected => {
                                    self.connections.remove(&header.dcid);
                                    continue 'read
                                }
                            }
                        } else {
                            udp_tx = Some(i.udp_tx.clone());
                        }
                    }
                    ConnectionHandle::Established(e) => {
                        // receive data into existing connection
                        match Self::recv_connection(&conn_id, e.connection.as_ref(), &mut rx_buf[..size], recv_info) {
                            Ok(_len) => {
                                e.rx_notify.notify_waiters();
                                e.tx_notify.notify_waiters();
                                continue 'read;
                            }
                            Err(e) => {
                                // TODO: take action on errors, e.g close connection, send & remove
                                break 'read Err(e);
                            }
                        }
                    }
                }
                if let Some(e) = established_handle {
                    match Self::recv_connection(&conn_id, e.connection.as_ref(), &mut rx_buf[..size], recv_info) {
                        Ok(_len) => {
                            e.rx_notify.notify_waiters();
                            e.tx_notify.notify_waiters();
                            // transition connection
                            handle.establish(e);
                            continue 'read;
                        }
                        Err(e) => {
                            // TODO: take action on errors, e.g close connection, send & remove
                            break 'read Err(e);
                        }
                    }
                }
            };
            if let Some(udp_tx) = udp_tx {
                // receive data on UDP channel
                match udp_tx.send(UdpRecv {
                    pkt: rx_buf[..size].to_vec(),
                    header,
                    recv_info,
                }).await {
                    Ok(()) => {},
                    Err(e) => warn!("sending dgram to connection {:?} failed with error: {}", conn_id, e)
                }
                continue 'read;
            }


            if header.ty != Type::Initial {
                debug!("Quic packet type is not \"Initial\". Header: {:?}. Continuing...", header);
                continue 'read;
            }

            // create incoming connection & handle
            let (udp_tx, udp_rx) = channel::<UdpRecv>(HANDSHAKE_PACKET_BUFFER_SIZE);
            let response = Arc::new(Mutex::new(None));

            debug!("new incoming connection {:?}", conn_id);
            let connection = Connection::Incoming(IncomingState {
                connection_id: conn_id.clone(),
                config: self.config.clone(),
                drop_connection: self.drop_connections.clone(),

                socket: self.socket.clone(),
                socket_details: self.socket_details.clone(),
                udp_rx,
                response: response.clone(),

                dgram: UdpRecv {
                    pkt: rx_buf[..size].to_vec(),
                    header,
                    recv_info,
                },

                ignore: false,
                reject: false,
            });
            let handle = ConnectionHandle::Incoming(IncomingHandle {
                udp_tx,
                response,
            });

            self.connections.insert(conn_id, handle);
            return Ok((connection.into(), from))
        }
    }

    fn recv_connection(conn_id: &ConnectionId<'_>, conn: &Mutex<QuicheConnection>, mut rx_buf: &mut [u8], recv_info: RecvInfo) -> io::Result<usize> {
        let size = rx_buf.len();
        let mut conn = conn.lock();
        match conn.recv(&mut rx_buf, recv_info) {
            Ok(len) => {
                debug!("connection {:?} received data length={}", conn_id, len);
                debug_assert_eq!(size, len, "size received on connection not equal to len received from network.");
                Ok(len)
            }
            Err(e) => {
                error!("connection {:?} receive error {:?}", conn_id, e);
                Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    format!("Connection could not receive network data for {:?}. {:?}",
                            conn.destination_id(), e)))
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

    pub(super) fn get_raw_fd(&self) -> RawFd {
        self.socket.as_raw_fd()
    }
}

#[cfg(unix)]
impl AsRawFd for crate::protocols::l4::listener::Listener {
    fn as_raw_fd(&self) -> RawFd {
        match &self {
            Self::Quic(l) => l.get_raw_fd(),
            Self::Tcp(l) => l.as_raw_fd(),
            Self::Unix(l) => l.as_raw_fd(),
        }
    }
}