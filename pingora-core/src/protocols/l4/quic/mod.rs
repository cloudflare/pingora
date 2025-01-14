use std::collections::{HashMap, VecDeque};
use std::{io, mem};
use std::fmt::{Debug, Formatter};
use std::net::SocketAddr;
use std::os::fd::{AsRawFd, RawFd};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use log::{debug, error, trace};
use parking_lot::Mutex;
use quiche::{Config, ConnectionId, Header, RecvInfo, Stats};
use ring::hmac::Key;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::UdpSocket;
use tokio::sync::mpsc::{Receiver, Sender};
use tokio::sync::mpsc::error::TryRecvError;
use tokio::sync::Notify;
use pingora_error::{Error, ErrorType, OrErr, Result};
use quiche::Connection as QuicheConnection;
use tokio::task::JoinHandle;

mod sendto;
pub(crate) mod id_token;
pub(crate) mod tls_handshake;
mod settings;
mod listener;

use crate::protocols::ConnectionState;
use crate::protocols::l4::quic::sendto::send_to;

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
const CONNECTION_DROP_DEQUE_INITIAL_SIZE: usize = 1024;

pub struct Listener {
    socket: Arc<UdpSocket>,
    socket_details: SocketDetails,

    config: Arc<Mutex<Config>>,
    crypto: Crypto,

    connections: HashMap<ConnectionId<'static>, ConnectionHandle>,
    drop_connections: Arc<Mutex<VecDeque<ConnectionId<'static>>>>,
}

pub struct Crypto {
    key: Key,
}

pub enum Connection {
    Incoming(IncomingState),
    Established(EstablishedState),
}

pub struct IncomingState {
    pub(crate) connection_id: ConnectionId<'static>,
    pub(crate) config: Arc<Mutex<Config>>,
    pub(crate) drop_connection: Arc<Mutex<VecDeque<ConnectionId<'static>>>>,

    pub(crate) socket: Arc<UdpSocket>,
    pub(crate) socket_details: SocketDetails,
    pub(crate) udp_rx: Receiver<UdpRecv>,
    pub(crate) response: Arc<Mutex<Option<HandshakeResponse>>>,

    pub(crate) dgram: UdpRecv,

    pub(crate) ignore: bool,
    pub(crate) reject: bool
}

#[derive(Clone)]
pub(crate) struct SocketDetails {
    addr: SocketAddr,
    gso_enabled: bool,
    pacing_enabled: bool,
}

pub struct EstablishedState {
    pub(crate) connection_id: ConnectionId<'static>,
    pub(crate) connection: Arc<Mutex<QuicheConnection>>,
    pub(crate) drop_connection: Arc<Mutex<VecDeque<ConnectionId<'static>>>>,
    pub(crate) rx_notify: Arc<Notify>,
    pub(crate) tx_notify: Arc<Notify>,
    pub(crate) socket: Arc<UdpSocket>,
    pub(crate) tx_handle: JoinHandle<Result<()>>,
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
    response: Arc<Mutex<Option<HandshakeResponse>>>,
}

pub(crate) enum HandshakeResponse {
    Established(EstablishedHandle),
    Ignored,
    Rejected,
    // TODO: TimedOut,
}

#[derive(Clone)]
pub struct EstablishedHandle {
    pub(crate) connection_id: ConnectionId<'static>,
    pub(crate) connection: Arc<Mutex<QuicheConnection>>,
    pub(crate) rx_notify: Arc<Notify>,
    pub(crate) tx_notify: Arc<Notify>,
}

pub struct UdpRecv {
    pub(crate) pkt: Vec<u8>,
    pub(crate) header: Header<'static>,
    pub(crate) recv_info: RecvInfo,
}

impl ConnectionHandle {
    fn establish(&mut self, handle: EstablishedHandle) {
        match self {
            ConnectionHandle::Incoming(_) => {
                debug!("connection handle {:?} established", handle.connection_id);
                let _ = mem::replace(self, ConnectionHandle::Established(handle));
            }
            ConnectionHandle::Established(_) => {}
        }
    }
}

impl Connection {
    pub(crate) fn establish(&mut self, state: EstablishedState) -> Result<()> {
        if cfg!(test) {
            let conn = state.connection.lock();
            debug_assert!(conn.is_established() || conn.is_in_early_data(),
                          "connection must be established or ready for data")
        }
        match self {
            Connection::Incoming(s) => {
                'drain: loop {
                    match s.udp_rx.try_recv() {
                        Ok(mut dgram) => {
                            let mut conn = state.connection.lock();
                            conn.recv(dgram.pkt.as_mut_slice(), dgram.recv_info)
                                .explain_err(
                                    ErrorType::HandshakeError, |_| "receiving dgram failed")?;
                            debug!("connection {:?} dgram received while establishing", s.connection_id)
                        }
                        Err(e) => {
                            match e {
                                TryRecvError::Empty => {
                                    // stop accepting packets
                                    s.udp_rx.close();
                                }
                                TryRecvError::Disconnected => {
                                    // remote already closed channel
                                }
                            }
                            break 'drain;
                        }
                    }
                }
                debug_assert!(s.udp_rx.is_empty(),
                              "udp rx channel must be empty when establishing the connection");
                debug!("connection {:?} established", state.connection_id);
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
                    debug!("connection {:?} stopped tx task", s.connection_id);
                }
            }
        }
    }
}

pub(crate) struct ConnectionTx {
    pub(crate) socket: Arc<UdpSocket>,
    pub(crate) socket_details: SocketDetails,

    pub(crate) connection: Arc<Mutex<QuicheConnection>>,
    pub(crate) connection_id: ConnectionId<'static>,

    pub(crate) tx_notify: Arc<Notify>,
    pub(crate) tx_stats: TxStats,
}

impl ConnectionTx {
    pub(crate) async fn start_tx(mut self) -> Result<()> {
        let id = self.connection_id;
        let mut out = [0u8;MAX_IPV6_BUF_SIZE];

        let mut finished_sending = false;
        let mut continue_write = false;
        debug!("connection {:?} tx write", id);
        'write: loop {
            // update stats from connection
            let max_send_burst = {
                let conn = self.connection.lock();
                self.tx_stats.max_send_burst(conn.stats(), conn.send_quantum())
            };
            let mut total_write = 0;
            let mut dst_info = None;

            // fill tx buffer with connection data
            trace!("connection {:?} total_write={}, max_send_burst={}", id, total_write, max_send_burst);
            'fill: while total_write < max_send_burst {
                let send = {
                    let mut conn = self.connection.lock();
                    conn.send(&mut out[total_write..max_send_burst])
                };

                let (size, send_info) = match send {
                    Ok((size, info)) => {
                        debug!("connection {:?} sent to={:?}, length={}", id, info.to, size);
                        (size, info)
                    },
                    Err(e) => {
                        if e == quiche::Error::Done {
                            trace!("connection {:?} send finished", id);
                            finished_sending = true;
                            break 'fill;
                        }
                        error!("connection {:?} send error: {:?}", id, e);
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

                if size < self.tx_stats.max_datagram_size {
                    continue_write = true;
                    break 'fill
                }
            }

            if total_write == 0 || dst_info.is_none() {
                trace!("connection {:?} nothing to send", id);
                self.tx_notify.notified().await;
                continue 'write;
            }
            let dst_info = dst_info.unwrap();

            // send to network
            if let Err(e) = send_to(
                &self.socket,
                &out[..total_write],
                &dst_info,
                self.tx_stats.max_datagram_size,
                self.socket_details.pacing_enabled,
                self.socket_details.gso_enabled,
            ).await {
                if e.kind() == io::ErrorKind::WouldBlock {
                    error!("connection {:?} network socket would block", id);
                    continue
                }
                break 'write Err(Error::explain(
                    ErrorType::WriteError,
                    format!("connection {:?} network send failed with {:?}", id, e)));
            }
            trace!("connection {:?} network sent to={} bytes={}", id, dst_info.to, total_write);

            if continue_write {
                continue 'write;
            }

            if finished_sending {
                trace!("connection {:?} finished sending", id);
                self.tx_notify.notified().await;
                continue 'write;
            }
        }
    }
}

pub struct TxStats {
    loss_rate: f64,
    max_send_burst: usize,
    max_datagram_size: usize
}

impl TxStats {
    pub(crate) fn new(max_send_udp_payload_size: usize) -> Self {
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
    ) -> Poll<Result<usize, io::Error>> {
        todo!()
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        // this is called on l4::Stream::drop()
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), io::Error>> {
        todo!()
    }
}

#[allow(unused_variables)] // TODO: remove
impl AsyncRead for Connection {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
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
