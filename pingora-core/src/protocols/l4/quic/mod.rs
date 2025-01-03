use std::collections::HashMap;
use std::{io, mem};
use std::fmt::{Debug, Formatter};
use std::net::SocketAddr;
use std::os::fd::{AsRawFd, RawFd};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll, Waker};
use log::{debug, error, trace, warn};
use parking_lot::Mutex;
use quiche::{Config, ConnectionId, Header, RecvInfo, Type};
use ring::hmac::Key;
use ring::rand::SystemRandom;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::UdpSocket;
use tokio::sync::mpsc::{channel, Receiver, Sender};
use tokio::sync::mpsc::error::TryRecvError;
use tokio::sync::Notify;
use pingora_error::{BError, Error, ErrorType};
use quiche::Connection as QuicheConnection;
use settings::Settings as QuicSettings;

#[allow(unused)] // TODO: remove
mod sendto;
mod id_token;
pub(crate) mod tls_handshake;
mod settings;

use crate::protocols::ConnectionState;
use crate::protocols::l4::stream::Stream as L4Stream;

// UDP header 8 bytes, IPv4 Header 20 bytes
//pub const MAX_IPV4_BUF_SIZE: usize = 65507;
// UDP header 8 bytes, IPv6 Header 40 bytes
pub const MAX_IPV6_BUF_SIZE: usize = 65487;

// 1500(Ethernet) - 20(IPv4 header) - 8(UDP header) = 1472.
//pub const MAX_IPV4_UDP_PACKET_SIZE: usize = 1472;
// 1500(Ethernet) - 40(IPv6 header) - 8(UDP header) = 1452
pub const MAX_IPV6_UDP_PACKET_SIZE: usize = 1452;

//pub const MAX_IPV4_QUIC_DATAGRAM_SIZE: usize = 1370;
pub const MAX_IPV6_QUIC_DATAGRAM_SIZE: usize = 1350;

const HANDSHAKE_PACKET_BUFFER_SIZE: usize = 64;

pub struct Listener {
    socket: Arc<UdpSocket>,
    socket_addr: SocketAddr,

    config: Arc<Mutex<Config>>,
    crypto: Crypto,

    connections: Mutex<HashMap<ConnectionId<'static>, ConnectionHandle>>,
}

pub struct Crypto {
    key: Key,
}

pub enum Connection {
    Incoming(IncomingState),
    Established(EstablishedState),
}

pub struct IncomingState {
    id: ConnectionId<'static>,
    config: Arc<Mutex<Config>>,

    socket: Arc<UdpSocket>,
    udp_rx: Receiver<UdpRecv>,
    response_tx: Sender<HandshakeResponse>,

    dgram: UdpRecv,

    ignore: bool,
    reject: bool
}

pub struct EstablishedState {
    socket: Arc<UdpSocket>,
    connection: Arc<Mutex<QuicheConnection>>,
    tx_notify: Arc<Notify>,
    rx_waker: Arc<Mutex<Option<Waker>>>
}

pub enum ConnectionHandle {
    Incoming(IncomingHandle),
    Established(EstablishedHandle),
}

impl Debug for ConnectionHandle {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_str("ConnectionHandle")?;
        match self {
            ConnectionHandle::Incoming(_) => f.write_str("::Incoming"),
            ConnectionHandle::Established(_) => f.write_str("::Established"),
        }
    }
}

pub struct IncomingHandle {
    udp_tx: Sender<UdpRecv>,
    response_rx: Receiver<HandshakeResponse>,
}

pub(crate) enum HandshakeResponse {
    Established(EstablishedHandle),
    Ignored,
    Rejected,
    // TODO: TimedOut,
}

#[derive(Clone)]
pub struct EstablishedHandle {
    connection: Arc<Mutex<QuicheConnection>>,
    rx_waker: Arc<Mutex<Option<Waker>>>,
    tx_notify: Arc<Notify>,
}

pub struct UdpRecv {
    pub(crate) pkt: Vec<u8>,
    pub(crate) header: Header<'static>,
    pub(crate) recv_info: RecvInfo,
}

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

        let settings = QuicSettings::try_default()?;

        Ok(Listener {
            socket: Arc::new(io),
            socket_addr: addr,

            config: settings.get_config(),
            crypto: Crypto {
                key
            },

            connections: Default::default(),
        })
    }
}

impl Listener {
    pub(crate) async fn accept(&self) -> io::Result<(L4Stream, SocketAddr)> {
        let mut rx_buf = [0u8; MAX_IPV6_BUF_SIZE];

        trace!("endpoint rx loop");
        'read: loop {
            // receive from network and parse Quic header
            let (size, from) = self.socket.recv_from(&mut rx_buf).await?;

            // parse the Quic packet's header
            let header = match Header::from_slice(rx_buf[..size].as_mut(), quiche::MAX_CONN_ID_LEN) {
                Ok(hdr) => hdr,
                Err(e) => {
                    warn!("Parsing Quic packet header failed with error: {:?}.", e);
                    trace!("Dropped packet due to invalid header. Continuing...");
                    continue 'read;
                }
            };
            trace!("dgram received from={} length={}", from, size);

            // TODO: allow for connection id updates during lifetime
            // connection needs to be able to update source_ids() or destination_ids()

            let recv_info = RecvInfo {
                to: self.socket_addr,
                from,
            };

            let mut conn_id = header.dcid.clone();
            let mut udp_tx = None;
            {
                let mut connections = self.connections.lock();
                // send to corresponding connection
                let mut handle;
                handle = connections.get_mut(&conn_id);
                if handle.is_none() {
                    conn_id = Self::gen_cid(&self.crypto.key, &header);
                    handle = connections.get_mut(&conn_id);
                };
                if let Some(handle) = handle {
                    trace!("existing connection {:?} {:?}", conn_id, handle);
                    match handle {
                        ConnectionHandle::Incoming(i) => {
                            match i.response_rx.try_recv() {
                                Ok(msg) => {
                                    match msg {
                                        HandshakeResponse::Established(e) => {
                                            // receive data into existing connection
                                            Self::recv_connection(e.connection.as_ref(), &mut rx_buf[..size], recv_info)?;
                                            // transition connection
                                            handle.establish(e)
                                        }
                                        HandshakeResponse::Ignored
                                        | HandshakeResponse::Rejected => {
                                            connections.remove(&header.dcid);
                                            continue 'read
                                        }
                                    }
                                }
                                Err(e) => {
                                    match e {
                                        TryRecvError::Empty => {
                                            udp_tx = Some(i.udp_tx.clone());
                                        }
                                        TryRecvError::Disconnected => {
                                            warn!("dropping connection {:?} handshake response channel receiver disconnected.", &header.dcid);
                                            connections.remove(&header.dcid);
                                        }
                                    };
                                }
                            }
                        }
                        ConnectionHandle::Established(e) => {
                            // receive data into existing connection
                            match Self::recv_connection(e.connection.as_ref(), &mut rx_buf[..size], recv_info) {
                                Ok(_len) => {
                                    e.tx_notify.notify_one();

                                    let mut rx_waker = e.rx_waker.lock();
                                    if let Some(waker) = rx_waker.take() {
                                        waker.wake_by_ref();
                                    }
                                }
                                Err(e) => {
                                    // TODO: take action on errors, e.g close connection, send & remove
                                    break 'read Err(e);
                                }
                            }
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
            let (response_tx, response_rx) = channel::<HandshakeResponse>(1);

            trace!("new incoming connection {:?}", conn_id);
            let connection = Connection::Incoming(IncomingState {
                id: conn_id.clone(),
                config: self.config.clone(),

                socket: self.socket.clone(),
                udp_rx,
                response_tx,

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
                response_rx,
            });

            {
                let mut connections = self.connections.lock();
                connections.insert(conn_id, handle);
            }

            return Ok((connection.into(), from))
        }
    }

    fn recv_connection(conn: &Mutex<QuicheConnection>, mut rx_buf: &mut [u8], recv_info: RecvInfo) -> io::Result<usize> {
        let size = rx_buf.len();
        let mut conn = conn.lock();
        match conn.recv(&mut rx_buf, recv_info) {
            Ok(len) => {
                debug!("connection received: length={}", len);
                debug_assert_eq!(size, len, "size received on connection not equal to len received from network.");
                Ok(len)
            }
            Err(e) => {
                error!("connection receive error: {:?}", e);
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

impl ConnectionHandle {
    fn establish(&mut self, handle: EstablishedHandle) {
        match self {
            ConnectionHandle::Incoming(_) => {
                let _ = mem::replace(self, ConnectionHandle::Established(handle));
            }
            ConnectionHandle::Established(_) => {}
        }
    }
}

impl Connection {
    fn establish(&mut self, state: EstablishedState) {
        match self {
            Connection::Incoming(_) => {
                let _ = mem::replace(self, Connection::Established(state));
            }
            Connection::Established(_) => {}
        }
    }
}

impl AsRawFd for Connection {
    fn as_raw_fd(&self) -> RawFd {
        match self {
            Connection::Incoming(s) => s.socket.as_raw_fd(),
            Connection::Established(s) => s.socket.as_raw_fd()
        }
    }
}

impl Debug for Listener {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Listener")
            .field("io", &self.socket)
            .finish()
    }
}


impl Connection {
    pub(crate) fn local_addr(&self) -> io::Result<SocketAddr> {
        match self {
            Connection::Incoming(s) => s.socket.local_addr(),
            Connection::Established(s) => s.socket.local_addr()
        }
    }
}

impl Debug for Connection {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QuicConnection").finish()
    }
}

#[allow(unused_variables)] // TODO: remove
impl AsyncWrite for Connection {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<pingora_error::Result<usize, std::io::Error>> {
        todo!()
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<pingora_error::Result<(), io::Error>> {
        todo!()
    }

    fn poll_shutdown(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<pingora_error::Result<(), io::Error>> {
        todo!()
    }
}

#[allow(unused_variables)] // TODO: remove
impl AsyncRead for Connection {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        todo!()
    }
}

impl ConnectionState for Connection {
    fn quic_connection_state(&mut self) -> Option<&mut Connection> {
        Some(self)
    }

    fn is_quic_connection(&self) -> bool {
        true
    }
}
