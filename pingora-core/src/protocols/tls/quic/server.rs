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

//! Quic Server TLS Handshake

use crate::protocols::l4::quic::id_token::{mint_token, validate_token};
use crate::protocols::l4::quic::listener::{
    EstablishedHandle, EstablishedState, HandshakeResponse, HandshakeState,
};
use crate::protocols::l4::quic::{
    handle_connection_errors, Connection, ConnectionTx, TxStats, MAX_IPV6_QUIC_DATAGRAM_SIZE,
};
use crate::protocols::l4::stream::Stream as L4Stream;
use crate::protocols::ConnectionState;
use log::{debug, error, trace, warn};
use parking_lot::Mutex;
use pingora_error::{Error, ErrorType, OrErr};
use quiche::ConnectionId;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::UdpSocket;
use tokio::sync::mpsc::error::TryRecvError;
use tokio::sync::Notify;

pub(crate) async fn handshake(mut stream: L4Stream) -> pingora_error::Result<L4Stream> {
    let Some(connection) = stream.quic_connection_state() else {
        debug_assert!(false, "quic::handshake called on stream of another type");
        return Err(Error::explain(
            ErrorType::InternalError,
            "stream is not a quic stream",
        ));
    };

    let e_state = match connection {
        Connection::IncomingEstablished(_) => {
            debug_assert!(false, "quic::handshake on already established connection");
            return Err(Error::explain(
                ErrorType::InternalError,
                "handshake state already established",
            ));
        }
        Connection::IncomingHandshake(i) => {
            if let Some(e_state) = handshake_inner(i).await? {
                // send HANDSHAKE_DONE Quic frame on established connection
                e_state.tx_notify.notify_waiters();
                Some(e_state)
            } else {
                debug!(
                    "handshake either rejected or ignored for connection {:?}",
                    i.connection_id
                );
                None
            }
        }
        Connection::OutgoingHandshake(_) | Connection::OutgoingEstablished(_) => {
            debug_assert!(false, "server handshake on client connection");
            return Err(Error::explain(
                ErrorType::InternalError,
                "server handshake on client connection",
            ));
        }
    };

    if let Some(e_state) = e_state {
        connection.establish_incoming(e_state)?;
        Ok(stream)
    } else {
        Err(Error::explain(
            ErrorType::HandshakeError,
            "handshake rejected or ignored",
        ))
    }
}

async fn handshake_inner(
    state: &mut HandshakeState,
) -> pingora_error::Result<Option<EstablishedState>> {
    let HandshakeState {
        connection_id: conn_id,
        configs,
        drop_connection,

        socket_details,
        udp_rx,
        dgram,

        response,

        ignore,
    } = state;

    if *ignore {
        {
            let mut resp = response.lock();
            *resp = Some(HandshakeResponse::Ignored)
        }
        return Ok(None);
    }

    let socket = &socket_details.io;
    let initial_dcid = dgram.header.dcid.clone();
    let mut out = [0u8; MAX_IPV6_QUIC_DATAGRAM_SIZE];

    if !quiche::version_is_supported(dgram.header.version) {
        warn!("Quic packet version received is not supported. Negotiating version...");
        let size = quiche::negotiate_version(&dgram.header.scid, &dgram.header.dcid, &mut out)
            .explain_err(ErrorType::HandshakeError, |_| {
                "creating version negotiation packet failed"
            })?;

        // send data to network
        send_dgram(conn_id, socket, &out[..size], dgram.recv_info.from)
            .await
            .explain_err(ErrorType::WriteError, |_| {
                "sending version negotiation packet failed"
            })?;

        // validate response
        if let Some(resp_dgram) = udp_rx.recv().await {
            if quiche::version_is_supported(resp_dgram.header.version) {
                *dgram = resp_dgram
            } else {
                return Err(Error::explain(
                    ErrorType::HandshakeError,
                    "version negotiation failed as responded version is not supported",
                ));
            };
        } else {
            return Err(Error::explain(
                ErrorType::HandshakeError,
                "version negotiation did not receive a response",
            ));
        }
    };

    // token is always present in "Initial" packets
    let token = dgram.header.token.as_ref().unwrap();
    // do stateless retry if the client didn't send a token
    if token.is_empty() {
        trace!(
            "connection {:?} stateless retry as Quic header token is empty",
            conn_id
        );

        let hdr = &dgram.header;
        let new_token = mint_token(hdr, &dgram.recv_info.from);
        let size = quiche::retry(
            &hdr.scid,
            &hdr.dcid,
            conn_id,
            &new_token,
            hdr.version,
            &mut out,
        )
        .explain_err(ErrorType::HandshakeError, |_| {
            "creating retry packet failed"
        })?;

        send_dgram(conn_id, socket, &out[..size], dgram.recv_info.from)
            .await
            .explain_err(ErrorType::WriteError, |_| "sending retry packet failed")?;

        // validate response
        if let Some(resp_dgram) = udp_rx.recv().await {
            // token is always present in "Initial" packets
            let resp_token = resp_dgram.header.token.as_ref().unwrap();
            if resp_token.is_empty() {
                return Err(Error::explain(
                    ErrorType::HandshakeError,
                    "Stateless retry failed. Still no token available after stateless retry."
                        .to_string(),
                ));
            } else {
                *dgram = resp_dgram;
            };
        } else {
            return Err(Error::explain(
                ErrorType::HandshakeError,
                "Stateless retry did not receive a response.".to_string(),
            ));
        }
    }

    let hdr = &dgram.header;
    let token = hdr.token.as_ref().unwrap();
    let odcid = validate_token(&dgram.recv_info.from, token);

    // The token was not valid, meaning the retry failed, so drop the connection.
    if odcid.is_none() {
        return Err(Error::explain(
            ErrorType::HandshakeError,
            "Quic header has invalid address validation token.".to_string(),
        ));
    }

    // The destination id was not valid, so drop the connection.
    if conn_id.len() != hdr.dcid.len() {
        return Err(Error::explain(
            ErrorType::HandshakeError,
            "Quic header has invalid destination connection id.".to_string(),
        ));
    }

    // Reuse the source connection ID we sent in the Retry packet,
    // instead of changing it again.
    debug!(
        "new connection {:?} odcid={:?} scid={:?} ",
        hdr.dcid, initial_dcid, hdr.scid
    );

    let mut conn;
    {
        let mut config = configs.quic().lock();
        conn = quiche::accept(
            &hdr.dcid,
            Some(&initial_dcid),
            dgram.recv_info.to,
            dgram.recv_info.from,
            &mut config,
        )
        .explain_err(ErrorType::HandshakeError, |_| {
            "connection instantiation failed"
        })?;
    }

    // receive quic data into connection
    let buf = dgram.pkt.as_mut_slice();
    conn.recv(buf, dgram.recv_info)
        .explain_err(ErrorType::HandshakeError, |_| {
            "receiving initial data failed"
        })?;

    debug!("connection {:?} starting handshake", conn_id);
    // RSA handshake requires more than one packet
    while !conn.is_established() {
        trace!("connection {:?} creating handshake packet", conn_id);
        'tx: loop {
            let (size, info) = match conn.send(out.as_mut_slice()) {
                Ok((size, info)) => (size, info),
                Err(quiche::Error::Done) => break 'tx,
                Err(e) => {
                    return Err(e).explain_err(ErrorType::WriteError, |_| {
                        "creating handshake packet failed"
                    })
                }
            };

            trace!("connection {:?} sending handshake packet", conn_id);
            send_dgram(conn_id, socket, &out[..size], info.to)
                .await
                .explain_err(ErrorType::WriteError, |_| "sending handshake packet failed")?;
        }

        trace!("connection {:?} waiting for handshake response", conn_id);
        'rx: loop {
            if let Some(mut dgram) = udp_rx.recv().await {
                trace!("connection {:?} received handshake response", conn_id);
                conn.recv(dgram.pkt.as_mut_slice(), dgram.recv_info)
                    .explain_err(ErrorType::HandshakeError, |_| {
                        "receiving handshake response failed"
                    })?;
            } else {
                return Err(Error::explain(
                    ErrorType::HandshakeError,
                    "finishing handshake failed, did not receive a response",
                ));
            }
            if udp_rx.is_empty() {
                break 'rx;
            }
        }

        trace!("connection {:?} established={}, early_data={}, closed={}, draining={}, readable={}, timed_out={}, resumed={}",
                conn_id, conn.is_established(), conn.is_in_early_data(), conn.is_closed(),
                conn.is_draining(), conn.is_readable(), conn.is_timed_out(), conn.is_resumed());

        trace!(
            "connection {:?} peer_error={:?}, local_error={:?}",
            conn_id,
            conn.peer_error(),
            conn.local_error()
        );

        handle_connection_errors(conn_id, conn.peer_error(), conn.local_error())?;
    }

    let connection = Arc::new(Mutex::new(conn));
    let tx_notify = Arc::new(Notify::new());
    let rx_notify = Arc::new(Notify::new());

    debug!("connection {:?} handshake successful", conn_id);

    let handle = EstablishedHandle {
        connection_id: conn_id.clone(),
        connection: connection.clone(),
        rx_notify: rx_notify.clone(),
        tx_notify: tx_notify.clone(),
    };

    {
        // hold the lock while draining the channel
        // avoid pkt receiving issues during establishing
        let mut resp = response.lock();
        'drain: loop {
            if !udp_rx.is_empty() {
                debug!(
                    "connection {:?} established udp_rx {}",
                    conn_id,
                    udp_rx.len()
                );
            }
            match udp_rx.try_recv() {
                Ok(mut dgram) => {
                    let mut conn = connection.lock();
                    conn.recv(dgram.pkt.as_mut_slice(), dgram.recv_info)
                        .explain_err(ErrorType::HandshakeError, |_| "receiving dgram failed")?;
                    debug!("connection {:?} dgram received while establishing", conn_id)
                }
                Err(e) => {
                    match e {
                        TryRecvError::Empty => break 'drain,
                        TryRecvError::Disconnected => {
                            // remote already closed channel
                            // not an issue, HandshakeResponse already processed
                            error!("connection {:?} channel disconnected", conn_id)
                        }
                    }
                }
            }
        }

        assert_eq!(
            udp_rx.len(),
            0,
            "udp rx channel must be empty when establishing the connection"
        );
        *resp = Some(HandshakeResponse::Established(handle));
    }
    // release the lock, next packet will be received on the connection
    // the Listener::accept() needs to hold the lock while writing to the udp_tx channel

    let tx = ConnectionTx {
        socket_details: socket_details.clone(),
        connection_id: conn_id.clone(),
        connection: connection.clone(),

        tx_notify: tx_notify.clone(),
        tx_stats: TxStats::new(),
    };

    let e_state = EstablishedState {
        connection_id: conn_id.clone(),
        connection: connection.clone(),

        http3_config: configs.http3().clone(),

        rx_notify: rx_notify.clone(),
        tx_notify: tx_notify.clone(),

        tx_handle: tokio::spawn(tx.start()), // sends HANDSHAKE_DONE Quic frame on established connection
        drop_connection: drop_connection.clone(),
        socket: socket.clone(),
    };

    Ok(Some(e_state))
}

// connection io tx directly via socket
async fn send_dgram(
    id: &ConnectionId<'_>,
    io: &Arc<UdpSocket>,
    buf: &[u8],
    to: SocketAddr,
) -> pingora_error::Result<usize> {
    match io.send_to(buf, &to).await {
        Ok(sent) => {
            debug_assert_eq!(
                sent,
                buf.len(),
                "amount of network sent data does not correspond to packet size"
            );
            trace!(
                "connection {:?} sent dgram to={:?} length={:?} ",
                id,
                to,
                buf.len()
            );
            Ok(sent)
        }
        Err(e) => {
            error!("Failed sending packet via UDP. Error: {:?}", e);
            Err(Error::explain(
                ErrorType::WriteError,
                format!("Failed sending packet via UDP. Error: {:?}", e),
            ))
        }
    }
}
