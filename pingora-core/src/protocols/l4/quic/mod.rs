use log::{debug, error, trace};
use parking_lot::Mutex;
use pingora_error::{Error, ErrorType, OrErr, Result};
use quiche::Connection as QuicheConnection;
use quiche::ConnectionId;
use quiche::{h3, Config};
use ring::hmac::Key;
use ring::rand::SystemRandom;

use std::fmt::{Debug, Formatter};
use std::net::SocketAddr;

use std::os::fd::{AsRawFd, RawFd};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::{io, mem};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::UdpSocket;
use tokio::sync::mpsc::error::TryRecvError;
use tokio::sync::Notify;

pub(crate) mod connector;
pub(crate) mod id_token;
pub(crate) mod listener;
mod sendto;

use crate::listeners::ALPN;
use crate::protocols::l4::quic::sendto::{detect_gso, send_to, set_txtime_sockopt};
use crate::protocols::tls::{SslDigest, TlsRef};
use crate::protocols::{ConnectionState, Ssl};

use crate::protocols::l4::quic::connector::{OutgoingEstablishedState, OutgoingHandshakeState};
use crate::protocols::l4::quic::listener::{IncomingEstablishedState, IncomingHandshakeState};

// UDP header 8 bytes, IPv4 Header 20 bytes
//pub const MAX_IPV4_BUF_SIZE: usize = 65507;
/// UDP header 8 bytes, IPv6 Header 40 bytes
pub const MAX_IPV6_BUF_SIZE: usize = 65487;

// 1500(Ethernet MTU) - 20(IPv4 header) - 8(UDP header) = 1472.
//pub const MAX_IPV4_UDP_PACKET_SIZE: usize = 1472;
/// 1500(Ethernet MTU) - 40(IPv6 header) - 8(UDP header) = 1452
pub const MAX_IPV6_UDP_PACKET_SIZE: usize = 1452;

//pub const MAX_IPV4_QUIC_DATAGRAM_SIZE: usize = 1370;
// TODO: validate size (possibly 1200 is the standard)
pub const MAX_IPV6_QUIC_DATAGRAM_SIZE: usize = 1350;

/// max. amount of [`UdpRecv`] messages on the `tokio::sync::mpsc::channel`
const HANDSHAKE_PACKET_BUFFER_SIZE: usize = 64;
/// initial size for the connection drop deque
const CONNECTION_DROP_DEQUE_INITIAL_SIZE: usize = 1024;

/// Represents a Quic [`Connection`] in either `Incoming` or `Outgoing` direction.
///
/// A [`Connection`] of variant `Incoming*` corresponds to a [`IncomingConnectionHandle`].
/// They are created having e.g. the variants [`Connection::IncomingHandshake`] / [`IncomingConnectionHandle::Handshake`]
/// and are transitioned to the [`Connection::IncomingEstablished`] / [`IncomingConnectionHandle::Established`]
/// variants once the TLS handshake was successful.
///
/// `Outgoing` connections do not have corresponding handles as they are bound to a distinguished
/// socket/quad-tuple and having a distinguished ConnectionRx task.
pub enum Connection {
    /// new incoming connection while in the handshake phase
    IncomingHandshake(IncomingHandshakeState),
    /// established incoming connection after successful handshake ([`quiche::Connection::is_established`])
    IncomingEstablished(IncomingEstablishedState),

    /// new outgoing connection while in the handshake phase
    OutgoingHandshake(OutgoingHandshakeState),
    /// established outgoing connection after successful handshake ([`quiche::Connection::is_established`])
    OutgoingEstablished(OutgoingEstablishedState),
}

#[derive(Clone)]
pub(crate) struct SocketDetails {
    pub(crate) io: Arc<UdpSocket>,
    pub(crate) local_addr: SocketAddr,
    pub(crate) peer_addr: Option<SocketAddr>,
    gso_enabled: bool,
    pacing_enabled: bool,
}

/// cryptographic for generation and validation of connection ids
pub(crate) struct Crypto {
    pub(crate) rng: SystemRandom,
    key: Key,
}

impl Crypto {
    pub(crate) fn new() -> Result<Self> {
        let rng = SystemRandom::new();
        let key = Key::generate(ring::hmac::HMAC_SHA256, &rng)
            .explain_err(ErrorType::InternalError, |e| {
                format!("failed to generate crypto key: {}", e)
            })?;

        Ok(Self { rng, key })
    }
}

/// connections transmit task sends data from the [`quiche::Connection`] to the UDP socket
/// the actor is notified through the `tx_notify` and flushes all connection data to the network
pub struct ConnectionTx {
    pub(crate) socket_details: SocketDetails,

    pub(crate) connection_id: ConnectionId<'static>,
    pub(crate) connection: Arc<Mutex<QuicheConnection>>,

    pub(crate) tx_notify: Arc<Notify>,
    pub(crate) tx_stats: TxStats,
}

/// During establishing a [`ConnectionTx`] task is started being responsible to write data from
/// the [`quiche::Connection`] to the `[UdpSocket`].
/// The connections `Rx` path is part of the [`Listener::accept`] which distributes the datagrams
/// to the according connections.
impl ConnectionTx {
    pub(crate) async fn start(mut self) -> Result<()> {
        let id = self.connection_id;
        let mut out = [0u8; MAX_IPV6_BUF_SIZE];

        let mut finished_sending = false;
        debug!("connection {:?} tx write", id);
        'write: loop {
            let mut continue_write = false;

            // update tx stats & get current details
            let (max_dgram_size, max_send_burst) = self.tx_stats.max_send_burst(&self.connection);

            let mut total_write = 0;
            let mut dst_info = None;

            // fill tx buffer with connection data
            trace!(
                "connection {:?} total_write={}, max_send_burst={}",
                id,
                total_write,
                max_send_burst
            );
            'fill: while total_write < max_send_burst {
                let send = {
                    let mut conn = self.connection.lock();
                    conn.send(&mut out[total_write..max_send_burst])
                };

                let (size, send_info) = match send {
                    Ok((size, info)) => {
                        debug!("connection {:?} sent to={:?}, length={}", id, info.to, size);
                        (size, info)
                    }
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
                            format!(
                                "Connection {:?} send data to network failed with {:?}",
                                id, e
                            ),
                        ));
                    }
                };

                total_write += size;
                // Use the first packet time to send, not the last.
                let _ = dst_info.get_or_insert(send_info);

                if size < self.tx_stats.max_datagram_size {
                    continue_write = true;
                    break 'fill;
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
                &self.socket_details.io,
                &out[..total_write],
                &dst_info,
                max_dgram_size,
                self.socket_details.pacing_enabled,
                self.socket_details.gso_enabled,
            )
            .await
            {
                if e.kind() == io::ErrorKind::WouldBlock {
                    error!("connection {:?} network socket would block", id);
                    continue;
                }
                break 'write Err(Error::explain(
                    ErrorType::WriteError,
                    format!("connection {:?} network send failed with {:?}", id, e),
                ));
            }
            trace!(
                "connection {:?} network sent to={} bytes={}",
                id,
                dst_info.to,
                total_write
            );

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

/// used within [`ConnectionTx`] to keep track of the maximum send burst
pub struct TxStats {
    loss_rate: f64,
    max_send_burst: usize,
    max_datagram_size: usize,
}

impl TxStats {
    pub(crate) fn new() -> Self {
        Self {
            loss_rate: 0.0,
            max_send_burst: MAX_IPV6_BUF_SIZE,
            max_datagram_size: 0,
        }
    }

    pub fn max_send_burst(&mut self, connection: &Mutex<quiche::Connection>) -> (usize, usize) {
        let stats;
        let send_quantum;
        {
            let conn = connection.lock();
            let max_udp_send_payload_size = conn.max_send_udp_payload_size();
            if self.max_datagram_size != max_udp_send_payload_size {
                self.max_datagram_size = max_udp_send_payload_size
            }
            stats = conn.stats();
            send_quantum = conn.send_quantum();
        }

        // Reduce max_send_burst by 25% if loss is increasing more than 0.1%.
        let loss_rate = stats.lost as f64 / stats.sent as f64;

        if loss_rate > self.loss_rate + 0.001 {
            self.max_send_burst = self.max_send_burst / 4 * 3;
            // Minimum bound of 10xMSS.
            self.max_send_burst = self.max_send_burst.max(self.max_datagram_size * 10);
            self.loss_rate = loss_rate;
        }

        let max_send_burst =
            send_quantum.min(self.max_send_burst) / self.max_datagram_size * self.max_datagram_size;

        (self.max_datagram_size, max_send_burst)
    }
}

/// contains configs for Quic [`quiche::Config`] and Http3 [`quiche::h3::Config`]
///
/// the configs can be supplied during the [`crate::listeners::Listeners`] creation
#[derive(Clone)]
pub struct QuicHttp3Configs {
    quic: Arc<Mutex<Config>>,
    http3: Arc<h3::Config>,
}

impl QuicHttp3Configs {
    pub fn new_quic_connector(trust_origin_ca_pem: Option<&str>) -> Result<Config> {
        let mut quic = Config::new(quiche::PROTOCOL_VERSION)
            .explain_err(ErrorType::InternalError, |_| {
                "Failed to create quiche config."
            })?;

        if let Some(trust_origin_ca_pem) = trust_origin_ca_pem {
            quic.load_verify_locations_from_file(trust_origin_ca_pem)
                .explain_err(ErrorType::FileReadError, |_| {
                    "Could not load trust CA from pem file."
                })?;
        };

        quic.set_application_protos(h3::APPLICATION_PROTOCOL)
            .explain_err(ErrorType::InternalError, |_| {
                "Failed to set application protocols."
            })?;

        quic.grease(false); // default true

        quic.set_max_idle_timeout(60 * 1000); // default ulimited
        quic.set_max_recv_udp_payload_size(MAX_IPV6_QUIC_DATAGRAM_SIZE); // recv default is 65527
        quic.set_max_send_udp_payload_size(MAX_IPV6_QUIC_DATAGRAM_SIZE); // send default is 1200
        quic.set_initial_max_data(10_000_000); // 10 Mb
        quic.set_initial_max_stream_data_bidi_local(1_000_000); // 1 Mb
        quic.set_initial_max_stream_data_bidi_remote(1_000_000); // 1 Mb
        quic.set_initial_max_stream_data_uni(1_000_000); // 1 Mb
        quic.set_initial_max_streams_bidi(100);
        quic.set_initial_max_streams_uni(100);

        quic.set_disable_active_migration(true); // default is false

        // quic.set_active_connection_id_limit(2); // default 2
        // quic.set_max_connection_window(conn_args.max_window); // default 24 Mb
        // quic.set_max_stream_window(conn_args.max_stream_window); // default 16 Mb

        Ok(quic)
    }
    pub fn new_quic_listener(cert_chain_pem_file: &str, priv_key_pem_file: &str) -> Result<Config> {
        let mut quic = Config::new(quiche::PROTOCOL_VERSION)
            .explain_err(ErrorType::InternalError, |_| {
                "Failed to create quiche config."
            })?;

        quic.load_cert_chain_from_pem_file(cert_chain_pem_file)
            .explain_err(ErrorType::FileReadError, |_| {
                "Could not load certificate chain from pem file."
            })?;

        quic.load_priv_key_from_pem_file(priv_key_pem_file)
            .explain_err(ErrorType::FileReadError, |_| {
                "Could not load private key from pem file."
            })?;

        // quic.load_verify_locations_from_file() for CA's
        // quic.verify_peer(); default server = false; client = true
        // quic.discover_pmtu(false); // default false
        quic.grease(false); // default true
                            // quic.log_keys() && config.set_keylog(); // logging SSL secrets
                            // quic.set_ticket_key() // session ticket signer key material

        //config.enable_early_data(); // can lead to ZeroRTT headers during handshake

        quic.set_application_protos(h3::APPLICATION_PROTOCOL)
            .explain_err(ErrorType::InternalError, |_| {
                "Failed to set application protocols."
            })?;

        // quic.set_application_protos_wire_format();
        // quic.set_max_amplification_factor(3); // anti-amplification limit factor; default 3

        quic.set_max_idle_timeout(60 * 1000); // default ulimited
        quic.set_max_recv_udp_payload_size(MAX_IPV6_QUIC_DATAGRAM_SIZE); // recv default is 65527
        quic.set_max_send_udp_payload_size(MAX_IPV6_QUIC_DATAGRAM_SIZE); // send default is 1200
        quic.set_initial_max_data(10_000_000); // 10 Mb
        quic.set_initial_max_stream_data_bidi_local(1_000_000); // 1 Mb
        quic.set_initial_max_stream_data_bidi_remote(1_000_000); // 1 Mb
        quic.set_initial_max_stream_data_uni(1_000_000); // 1 Mb
        quic.set_initial_max_streams_bidi(100);
        quic.set_initial_max_streams_uni(100);

        // quic.set_ack_delay_exponent(3); // default 3
        // quic.set_max_ack_delay(25); // default 25
        // quic.set_active_connection_id_limit(2); // default 2
        // quic.set_disable_active_migration(false); // default false

        // quic.set_active_connection_id_limit(2); // default 2
        // quic.set_disable_active_migration(false); // default false
        // quic.set_cc_algorithm_name("cubic"); // default cubic
        // quic.set_initial_congestion_window_packets(10); // default 10
        // quic.set_cc_algorithm(CongestionControlAlgorithm::CUBIC); // default CongestionControlAlgorithm::CUBIC

        // quic.enable_hystart(true); // default true
        // quic.enable_pacing(true); // default true
        // quic.set_max_pacing_rate(); // default ulimited

        //config.enable_dgram(false); // default false

        // quic.set_path_challenge_recv_max_queue_len(3); // default 3
        // quic.set_max_connection_window(MAX_CONNECTION_WINDOW); // default 24 Mb
        // quic.set_max_stream_window(MAX_STREAM_WINDOW); // default 16 Mb
        // quic.set_stateless_reset_token(None) // default None
        // quic.set_disable_dcid_reuse(false) // default false

        Ok(quic)
    }

    fn new_http3() -> Result<h3::Config> {
        h3::Config::new().explain_err(ErrorType::InternalError, |_| {
            "failed to create new h3::Config"
        })
    }

    pub fn from_ca_file_path(trust_origin_ca_pem: Option<&str>) -> Result<Self> {
        Ok(Self {
            quic: Arc::new(Mutex::new(Self::new_quic_connector(trust_origin_ca_pem)?)),
            http3: Arc::new(Self::new_http3()?),
        })
    }

    pub fn from_cert_key_paths(cert_chain_pem_file: &str, priv_key_pem_file: &str) -> Result<Self> {
        Ok(Self {
            quic: Arc::new(Mutex::new(Self::new_quic_listener(
                cert_chain_pem_file,
                priv_key_pem_file,
            )?)),
            http3: Arc::new(Self::new_http3()?),
        })
    }

    pub fn new(quic: Config, http3: h3::Config) -> Self {
        Self {
            quic: Arc::new(Mutex::new(quic)),
            http3: Arc::new(http3),
        }
    }

    pub fn try_from(quic: Config) -> Result<Self> {
        let http3 = h3::Config::new().explain_err(ErrorType::InternalError, |_| {
            "failed to create new h3::Config"
        })?;

        Ok(Self {
            quic: Arc::new(Mutex::new(quic)),
            http3: Arc::new(http3),
        })
    }

    pub fn quic(&self) -> &Arc<Mutex<Config>> {
        &self.quic
    }
    pub fn http3(&self) -> &Arc<h3::Config> {
        &self.http3
    }
}

impl Debug for QuicHttp3Configs {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let mut dbg = f.debug_struct("Configs");
        dbg.finish()
    }
}

fn detect_gso_pacing(io: &UdpSocket) -> (bool, bool) {
    let gso_enabled = detect_gso(&io, MAX_IPV6_QUIC_DATAGRAM_SIZE);
    let pacing_enabled = match set_txtime_sockopt(&io) {
        Ok(_) => {
            debug!("successfully set SO_TXTIME socket option");
            true
        }
        Err(e) => {
            debug!("setsockopt failed {:?}", e);
            false
        }
    };
    (gso_enabled, pacing_enabled)
}

impl Connection {
    pub(crate) fn establish_incoming(&mut self, state: IncomingEstablishedState) -> Result<()> {
        if cfg!(test) {
            let conn = state.connection.lock();
            debug_assert!(
                conn.is_established() || conn.is_in_early_data(),
                "connection must be established or ready for data"
            )
        }
        match self {
            Connection::IncomingHandshake(s) => {
                'drain: loop {
                    match s.udp_rx.try_recv() {
                        Ok(mut dgram) => {
                            let mut conn = state.connection.lock();
                            conn.recv(dgram.pkt.as_mut_slice(), dgram.recv_info)
                                .explain_err(ErrorType::HandshakeError, |_| {
                                    "receiving dgram failed"
                                })?;
                            debug!(
                                "connection {:?} dgram received while establishing",
                                s.connection_id
                            )
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
                debug_assert!(
                    s.udp_rx.is_empty(),
                    "udp rx channel must be empty when establishing the connection"
                );
                debug!("connection {:?} established", state.connection_id);
                let _ = mem::replace(self, Connection::IncomingEstablished(state));
                Ok(())
            }
            _ => Err(Error::explain(
                ErrorType::InternalError,
                "establishing connection only possible on incoming handshake connection",
            )),
        }
    }

    pub(crate) fn establish_outgoing(&mut self, state: OutgoingEstablishedState) -> Result<()> {
        if cfg!(test) {
            let conn = state.connection.lock();
            debug_assert!(
                conn.is_established() || conn.is_in_early_data(),
                "connection must be established or ready for data"
            )
        }
        match self {
            Connection::OutgoingHandshake(_) => {
                debug!("connection {:?} established", state.connection_id);
                let _ = mem::replace(self, Connection::OutgoingEstablished(state));
                Ok(())
            }
            _ => Err(Error::explain(
                ErrorType::InternalError,
                "establishing connection only possible on outgoing handshake connection",
            )),
        }
    }

    pub(crate) fn local_addr(&self) -> io::Result<SocketAddr> {
        match self {
            Connection::IncomingHandshake(s) => s.socket_details.io.local_addr(),
            Connection::IncomingEstablished(s) => s.socket.local_addr(),
            Connection::OutgoingHandshake(s) => s.socket_details.io.local_addr(),
            Connection::OutgoingEstablished(s) => s.socket.local_addr(),
        }
    }
}

impl Debug for Connection {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QuicConnection").finish()
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

impl Drop for Connection {
    fn drop(&mut self) {
        match self {
            Connection::IncomingEstablished(s) => {
                if !s.tx_handle.is_finished() {
                    s.tx_handle.abort();
                    debug!("connection {:?} stopped tx task", s.connection_id);
                }
            }
            // FIXME: handle outgoing (stopping rx loop)
            _ => {}
        }
    }
}

impl Ssl for Connection {
    /// Return the TLS info if the connection is over TLS
    fn get_ssl(&self) -> Option<&TlsRef> {
        None
    }

    /// Return the [`tls::SslDigest`] for logging
    fn get_ssl_digest(&self) -> Option<Arc<SslDigest>> {
        match self {
            Connection::IncomingEstablished(s) => {
                let mut conn = s.connection.lock();
                let conn = &mut *conn;
                Some(Arc::from(SslDigest::from_ssl(conn.as_mut())))
            }
            _ => None,
        }
    }

    /// Return selected ALPN if any
    fn selected_alpn_proto(&self) -> Option<ALPN> {
        match self {
            Connection::IncomingEstablished(s) => {
                let conn = s.connection.lock();
                ALPN::from_wire_selected(conn.application_proto())
            }
            Connection::OutgoingEstablished(s) => {
                let conn = s.connection.lock();
                ALPN::from_wire_selected(conn.application_proto())
            }
            _ => None,
        }
    }
}

impl AsRawFd for Connection {
    fn as_raw_fd(&self) -> RawFd {
        match self {
            Connection::IncomingHandshake(s) => s.socket_details.io.as_raw_fd(),
            Connection::IncomingEstablished(s) => s.socket.as_raw_fd(),
            Connection::OutgoingHandshake(s) => s.socket_details.io.as_raw_fd(),
            Connection::OutgoingEstablished(s) => s.socket.as_raw_fd(),
        }
    }
}

#[allow(unused_variables)] // TODO: remove
impl AsyncWrite for Connection {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        todo!()
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        // this is called on l4::Stream::drop()
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        todo!()
    }
}

// TODO: consider usage for Quic/Connection/Datagrams
// is there be any source for data in this area (e.g. L4/UDP -> Quic/Dgram, Media Over Quic, ...)
#[allow(unused_variables)]
impl AsyncRead for Connection {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        todo!()
    }
}
