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

//! Quic integration

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

use pingora_error::ErrorType::ConnectionClosed;
use std::os::fd::{AsRawFd, RawFd};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::{io, mem};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::UdpSocket;
use tokio::sync::Notify;

pub(crate) mod connector;
pub(crate) mod id_token;
pub(crate) mod listener;
mod sendto;

use crate::listeners::ALPN;
use crate::protocols::l4::quic::sendto::{detect_gso, send_to, set_txtime_sockopt};
use crate::protocols::tls::{SslDigest, TlsRef};
use crate::protocols::{ConnectionState, Ssl};
use pingora_boringssl::ssl::SslContextBuilder;

// UDP header 8 bytes, IPv4 Header 20 bytes
//pub const MAX_IPV4_BUF_SIZE: usize = 65507;
/// UDP header 8 bytes, IPv6 Header 40 bytes
pub const MAX_IPV6_BUF_SIZE: usize = 65487;

// 1500(Ethernet MTU) - 20(IPv4 header) - 8(UDP header) = 1472.
//pub const MAX_IPV4_UDP_PACKET_SIZE: usize = 1472;
/// 1500(Ethernet MTU) - 40(IPv6 header) - 8(UDP header) = 1452
pub const MAX_IPV6_UDP_PACKET_SIZE: usize = 1452;

//pub const MAX_IPV4_QUIC_DATAGRAM_SIZE: usize = 1370;
// TODO: validate size (is 1200 the standard?)
pub const MAX_IPV6_QUIC_DATAGRAM_SIZE: usize = 1350;

/// initial size for the connection drop deque
const CONNECTION_DROP_DEQUE_INITIAL_SIZE: usize = 1024;

/// Represents a Quic [`Connection`] in either `Incoming` or `Outgoing` direction.
///
/// A [`Connection`] of variant `Incoming*` corresponds to a `IncomingConnectionHandle`.
///
/// They are created having e.g. the variants [`Connection::IncomingHandshake`] / [`listener::IncomingConnectionHandle::Handshake`].
/// Once the TLS handshake was successful they are transitioned to the
/// [`Connection::IncomingEstablished`] / [`listener::IncomingConnectionHandle::Established`] variants.
///
/// `Outgoing` connections **do not have** corresponding handles as they are bound to
/// a distinguished socket/4-tuple and having a distinguished [`connector::ConnectionRx`] task.
pub enum Connection {
    /// new incoming connection while in the handshake phase
    IncomingHandshake(listener::HandshakeState),
    /// established incoming connection after successful handshake ([`quiche::Connection::is_established`])
    IncomingEstablished(listener::EstablishedState),

    /// new outgoing connection while in the handshake phase
    OutgoingHandshake(connector::HandshakeState),
    /// established outgoing connection after successful handshake ([`quiche::Connection::is_established`])
    OutgoingEstablished(connector::EstablishedState),
}

/// the [`UdpSocket`] and according details
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

/// Connection transmit (`tx`) task sends data from the [`quiche::Connection`] to the UDP socket
/// the task is notified through the `tx_notify` and flushes all connection data to the network
pub(crate) struct ConnectionTx {
    pub(crate) socket_details: SocketDetails,

    pub(crate) connection_id: ConnectionId<'static>,
    pub(crate) connection: Arc<Mutex<QuicheConnection>>,

    pub(crate) tx_notify: Arc<Notify>,
    pub(crate) tx_stats: TxStats,
}

/// During establishing a [`ConnectionTx`] task is started being responsible to write data from
/// the [`quiche::Connection`] to the [`UdpSocket`].
///
/// The connections receive (`rx`) path is part of the [`listener::Listener::accept`] which distributes the datagrams
/// to the according connections.
///
/// For outgoing [`Connection`]s a [`connector::ConnectionRx`] task is responsible to receive the data into the [`quiche::Connection`]
impl ConnectionTx {
    /// start the `tx` task, consumes the struct
    ///
    /// is stopped within the `Drop` implementation of the corresponding [`Connection`]
    pub(crate) async fn start(mut self) -> Result<()> {
        let id = self.connection_id;
        let mut out = [0u8; MAX_IPV6_BUF_SIZE];

        let mut finished_sending = None;
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
                    if conn.is_draining() {
                        // Once is_draining() returns true, it is no longer necessary
                        // to call send() and all calls will return Done.
                        break 'write Ok(());
                    }
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
                            // register notify before socket send to avoid misses under high load
                            finished_sending = Some(self.tx_notify.notified());
                            break 'fill;
                        }
                        error!("connection {:?} send error: {:?}", id, e);
                        // TODO: close connection needed
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

            if let Some(tx_notified) = finished_sending {
                trace!("connection {:?} finished sending", id);
                tx_notified.await;
                finished_sending = None;
                continue 'write;
            }
        }
    }
}

/// used within [`ConnectionTx`] to keep track of the maximum send burst
pub(crate) struct TxStats {
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

        QuicHttp3Configs::set_quic_defaults(quic)
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

        QuicHttp3Configs::set_quic_defaults(quic)
    }

    fn set_quic_defaults(mut quic: Config) -> Result<Config> {
        quic.set_application_protos(h3::APPLICATION_PROTOCOL)
            .explain_err(ErrorType::InternalError, |_| {
                "Failed to set application protocols."
            })?;
        quic.grease(false); // default true

        // TODO: usable for mTLS?
        // quic.verify_peer(); default server = false; client = true

        quic.set_max_idle_timeout(5 * 1000); // default ulimited
        quic.set_max_recv_udp_payload_size(MAX_IPV6_BUF_SIZE); // recv default is 65527
        quic.set_max_send_udp_payload_size(MAX_IPV6_QUIC_DATAGRAM_SIZE); // send default is 1200
        quic.set_initial_max_data(10_000_000); // 10 Mb
        quic.set_initial_max_stream_data_bidi_local(1_000_000); // 1 Mb
        quic.set_initial_max_stream_data_bidi_remote(1_000_000); // 1 Mb
        quic.set_initial_max_stream_data_uni(1_000_000); // 1 Mb

        // TODO: config through peer.options.max_h3_streams
        quic.set_initial_max_streams_bidi(100);
        quic.set_initial_max_streams_uni(100);

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

    pub(crate) fn with_boring_ssl_ctx_builder(ctx_builder: SslContextBuilder) -> Result<Self> {
        let quic = Config::with_boring_ssl_ctx_builder(quiche::PROTOCOL_VERSION, ctx_builder)
            .explain_err(ErrorType::InternalError, |_| {
                "Failed to create quiche config."
            })?;

        let quic = QuicHttp3Configs::set_quic_defaults(quic)?;

        let http3 = QuicHttp3Configs::new_http3()?;
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
    let gso_enabled = detect_gso(io, MAX_IPV6_QUIC_DATAGRAM_SIZE);
    let pacing_enabled = match set_txtime_sockopt(io) {
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
    pub(crate) fn establish_incoming(&mut self, state: listener::EstablishedState) -> Result<()> {
        if cfg!(test) {
            let conn = state.connection.lock();
            debug_assert!(
                conn.is_established() || conn.is_in_early_data(),
                "connection must be established or ready for data"
            )
        }
        match self {
            Connection::IncomingHandshake(_) => {
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

    pub(crate) fn establish_outgoing(&mut self, state: connector::EstablishedState) -> Result<()> {
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
        if let Connection::IncomingEstablished(s) = self {
            if !s.tx_handle.is_finished() {
                s.tx_handle.abort();
                debug!("connection {:?} stopped tx task", s.connection_id);
            }
        }
        if let Connection::OutgoingEstablished(s) = self {
            if !s.rx_handle.is_finished() {
                s.rx_handle.abort();
                debug!("connection {:?} stopped rx task", s.connection_id);
            }
            if !s.tx_handle.is_finished() {
                s.tx_handle.abort();
                debug!("connection {:?} stopped tx task", s.connection_id);
            }
        }
    }
}

impl Ssl for Connection {
    /// Return the TLS info if the connection is over TLS
    fn get_ssl(&self) -> Option<&TlsRef> {
        None
    }

    /// Return the [`crate::protocols::tls::SslDigest`] for logging
    fn get_ssl_digest(&self) -> Option<Arc<SslDigest>> {
        match self {
            Connection::IncomingEstablished(s) => {
                let mut conn = s.connection.lock();
                let conn = &mut *conn;
                Some(Arc::from(SslDigest::from_quic_ssl(conn.as_mut())))
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

// TODO: remove, ideally requirement for AsyncRead & AsyncWrite on Stream
// possibly switch to AsyncIO & IO and/or AsyncStream & Stream
#[allow(unused_variables)]
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

pub(crate) fn handle_connection_errors(
    conn_id: &ConnectionId<'_>,
    local_error: Option<&quiche::ConnectionError>,
    peer_error: Option<&quiche::ConnectionError>,
) -> Result<()> {
    if let Some(e) = local_error {
        let error_msg = format!(
            "connection {:?} local error {}",
            conn_id,
            String::from_utf8_lossy(e.reason.as_slice())
        );
        debug!("{}", error_msg);
        return Err(e).explain_err(ConnectionClosed, |_| error_msg);
    }

    if let Some(e) = peer_error {
        let error_msg = format!(
            "connection {:?} peer error {}",
            conn_id,
            String::from_utf8_lossy(e.reason.as_slice())
        );
        debug!("{}", error_msg);
        return Err(e).explain_err(ConnectionClosed, |_| error_msg);
    }

    Ok(())
}
