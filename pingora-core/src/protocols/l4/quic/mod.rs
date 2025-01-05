use std::collections::HashMap;
use std::{io, mem};
use std::fmt::{Debug, Formatter};
use std::net::SocketAddr;
use std::os::fd::{AsRawFd, RawFd};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use log::{debug, error, trace, warn};
use parking_lot::Mutex;
use quiche::{Config, ConnectionId, Header, RecvInfo, Stats, Type};
use ring::hmac::Key;
use ring::rand::SystemRandom;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::UdpSocket;
use tokio::sync::mpsc::{channel, Receiver, Sender};
use tokio::sync::mpsc::error::TryRecvError;
use tokio::sync::Notify;
use pingora_error::{BError, Error, ErrorType, OrErr, Result};
use quiche::Connection as QuicheConnection;
use tokio::task::JoinHandle;
use settings::Settings as QuicSettings;

#[allow(unused)] // TODO: remove
mod sendto;
mod id_token;
pub(crate) mod tls_handshake;
mod settings;

use crate::protocols::ConnectionState;
use crate::protocols::l4::quic::sendto::send_to;
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
    tx_handle: JoinHandle<Result<()>>,

    pub connection: Arc<Mutex<QuicheConnection>>,
    pub tx_notify: Arc<Notify>,
    pub rx_notify: Arc<Notify>,
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
    rx_notify: Arc<Notify>,
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

        debug!("endpoint rx loop");
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
                                    e.rx_notify.notify_waiters();
                                    e.tx_notify.notify_one();
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
    async fn establish(&mut self, state: EstablishedState) -> Result<()> {
        if cfg!(test) {
            let conn = state.connection.lock();
            debug_assert!(conn.is_established() || conn.is_in_early_data(),
                          "connection must be established or ready for data")
        }
        match self {
            Connection::Incoming(s) => {
                /*
                // consume packets that potentially arrived during state transition
                while !s.udp_rx.is_empty() {
                    error!("consuming {} packets which arrived during state transition", s.udp_rx.len());
                    let mut dgram= s.udp_rx.recv().await;
                    if let Some(mut dgram) = dgram {
                        let mut qconn = state.connection.lock();
                        qconn.recv(&mut dgram.pkt.as_mut_slice(), dgram.recv_info).explain_err(
                            ErrorType::ReadError,
                            |e| format!("receiving dgram on quic connection failed with {:?}", e))?;
                    }
                }
                s.udp_rx.close();
                */
                let _ = mem::replace(self, Connection::Established(state));
                Ok(())
            }
            Connection::Established(_) => Err(Error::explain(
                ErrorType::InternalError,
                "establishing connection only possible on incoming connection"))
        }
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        match self {
            Connection::Incoming(_) => {}
            Connection::Established(s) => {
                if !s.tx_handle.is_finished() {
                    s.tx_handle.abort();
                    trace!("stopped connection tx task");
                }
            }
        }
    }
}

struct ConnectionTx {
    socket: Arc<UdpSocket>,

    connection: Arc<Mutex<QuicheConnection>>,
    connection_id: String,

    tx_notify: Arc<Notify>,
    tx_stats: TxBurst,

    gso_enabled: bool,
    pacing_enabled: bool,
}

impl ConnectionTx {
    async fn start_tx(mut self) -> Result<()> {
        let id = self.connection_id;
        let mut out = [0u8;MAX_IPV6_BUF_SIZE];

        let mut finished_sending = false;
        debug!("connection tx write");
        'write: loop {
            // update stats from connection
            let max_send_burst = {
                let conn = self.connection.lock();
                self.tx_stats.max_send_burst(conn.stats(), conn.send_quantum())
            };
            let mut total_write = 0;
            let mut dst_info = None;

            // fill tx buffer with connection data
            trace!("total_write={}, max_send_burst={}", total_write, max_send_burst);
            'fill: while total_write < max_send_burst {
                let send = {
                    let mut conn = self.connection.lock();
                    conn.send(&mut out[total_write..max_send_burst])
                };

                let (size, send_info) = match send {
                    Ok((size, info)) => {
                        debug!("connection sent to={:?}, length={}", info.to, size);
                        (size, info)
                    },
                    Err(e) => {
                        if e == quiche::Error::Done {
                            trace!("connection send finished");
                            finished_sending = true;
                            break 'fill;
                        }
                        error!("connection send error: {:?}", e);
                        /* TODO: close connection
                            let mut conn = self.connection.lock();
                            conn.close(false, 0x1, b"fail").ok();
                         */
                        break 'write Err(Error::explain(
                            ErrorType::WriteError,
                            format!("Connection {:?} send data to network failed with {:?}", id, e)));
                    }
                };

                total_write += size;
                // Use the first packet time to send, not the last.
                let _ = dst_info.get_or_insert(send_info);
            }

            if total_write == 0 || dst_info.is_none() {
                debug!("nothing to send, waiting for notification...");
                self.tx_notify.notified().await;
                continue;
            }
            let dst_info = dst_info.unwrap();

            // send to network
            if let Err(e) = send_to(
                &self.socket,
                &out[..total_write],
                &dst_info,
                self.tx_stats.max_datagram_size,
                self.pacing_enabled,
                self.gso_enabled,
            ).await {
                if e.kind() == io::ErrorKind::WouldBlock {
                    error!("network socket would block");
                    continue
                }
                break 'write Err(Error::explain(
                    ErrorType::WriteError,
                    format!("network send failed with {:?}", e)));
            }
            trace!("network sent to={} bytes={}", dst_info.to, total_write);

            if finished_sending {
                debug!("sending finished, waiting for notification...");
                self.tx_notify.notified().await
            }
        }
    }
}

pub struct TxBurst {
    loss_rate: f64,
    max_send_burst: usize,
    max_datagram_size: usize
}

impl TxBurst {
    fn new(max_send_udp_payload_size: usize) -> Self {
        Self {
            loss_rate: 0.0,
            max_send_burst: MAX_IPV6_BUF_SIZE,
            max_datagram_size: max_send_udp_payload_size,
        }
    }

    fn max_send_burst(&mut self, stats: Stats, send_quantum: usize) -> usize {
        // Reduce max_send_burst by 25% if loss is increasing more than 0.1%.
        let loss_rate = stats.lost as f64 / stats.sent as f64;

        if loss_rate > self.loss_rate + 0.001 {
            self.max_send_burst = self.max_send_burst / 4 * 3;
            // Minimum bound of 10xMSS.
            self.max_send_burst =
                self.max_send_burst.max(self.max_datagram_size * 10);
            self.loss_rate = loss_rate;
        }

        send_quantum.min(self.max_send_burst) /
            self.max_datagram_size * self.max_datagram_size
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
        // FIXME: this is called on l4::Stream::drop()
        // correlates to the connection, check if stopping tx loop for connection & final flush is feasible
        Poll::Ready(Ok(()))
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
