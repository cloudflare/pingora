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

//! Quic Client TLS Handshake

use crate::listeners::ALPN;
use crate::protocols::l4::quic::connector::{ConnectionRx, EstablishedState, HandshakeState};
use crate::protocols::l4::quic::id_token::generate_outgoing_cid;
use crate::protocols::l4::quic::{
    handle_connection_errors, Connection, ConnectionTx, QuicHttp3Configs, TxStats,
};
use crate::protocols::IO;
use crate::upstreams::peer::Peer;
use log::{info, trace};
use parking_lot::Mutex;
use pingora_error::ErrorType::HandshakeError;
use pingora_error::{Error, ErrorType, OrErr};
use std::sync::Arc;
use tokio::sync::Notify;

pub(crate) async fn handshake<T, P>(
    mut stream: T,
    peer: &P,
    alpn_override: Option<ALPN>,
    tls_ctx: &QuicHttp3Configs,
) -> pingora_error::Result<T>
where
    T: IO,
    P: Peer + Send + Sync,
{
    let Some(connection) = stream.quic_connection_state() else {
        debug_assert!(false, "quic::handshake called on stream of another type");
        return Err(Error::explain(
            ErrorType::InternalError,
            "stream is not a quic stream",
        ));
    };

    let e_state = match connection {
        Connection::IncomingHandshake(_) | Connection::IncomingEstablished(_) => {
            debug_assert!(false, "client handshake on server connection");
            return Err(Error::explain(
                ErrorType::InternalError,
                "client handshake on server connection",
            ));
        }
        Connection::OutgoingEstablished(_) => {
            debug_assert!(false, "handshake on already established connection");
            return Err(Error::explain(
                ErrorType::InternalError,
                "handshake state already established",
            ));
        }
        Connection::OutgoingHandshake(o) => {
            handshake_inner(o, peer, alpn_override, tls_ctx).await?
        }
    };

    connection.establish_outgoing(e_state)?;
    Ok(stream)
}

pub(crate) async fn handshake_inner<P>(
    state: &mut HandshakeState,
    peer: &P,
    _alpn_override: Option<ALPN>, // potentially HTTP09 could be supported
    quic_tls_ctx: &QuicHttp3Configs,
) -> pingora_error::Result<EstablishedState>
where
    P: Peer + Send + Sync,
{
    let HandshakeState {
        crypto,
        socket_details,
    } = state;

    let mut peer_quic_http3_config = None;
    if let Some(peer_options) = peer.get_peer_options() {
        peer_quic_http3_config.clone_from(&peer_options.quic_http3_configs)
    };

    // use peer config in case present or fallback to tls_ctx from TransportConnector
    let configs = if let Some(peer_quic_http3_config) = peer_quic_http3_config {
        peer_quic_http3_config
    } else {
        quic_tls_ctx.clone()
    };

    let conn_id = generate_outgoing_cid(&crypto.rng);

    let local_addr = socket_details.local_addr;
    let Some(peer_addr) = socket_details.peer_addr else {
        return Err(Error::explain(
            HandshakeError,
            "peer address for outgoing connection not present",
        ));
    };

    let conn = {
        let mut config = configs.quic().lock();
        // Create a QUIC connection and initiate handshake.
        quiche::connect(
            Some(peer.sni()),
            &conn_id,
            local_addr,
            peer_addr,
            &mut config,
        )
        .explain_err(HandshakeError, |e| {
            format!("failed to generate initial handshake packet {:?}", e)
        })?
    };
    info!(
        "connection {:?} outgoing from {:} to {:}",
        conn_id, local_addr, peer_addr
    );

    let connection = Arc::new(Mutex::new(conn));
    let tx_notify = Arc::new(Notify::new());
    let rx_notify = Arc::new(Notify::new());

    // starting connection IO
    let tx = ConnectionTx {
        socket_details: socket_details.clone(),
        connection_id: conn_id.clone(),
        connection: connection.clone(),
        tx_notify: tx_notify.clone(),
        tx_stats: TxStats::new(),
    };
    let rx = ConnectionRx {
        socket_details: socket_details.clone(),
        connection_id: conn_id.clone(),
        connection: connection.clone(),
        rx_notify: rx_notify.clone(),
        tx_notify: tx_notify.clone(),
    };

    let rx_handle = tokio::task::spawn(rx.start());
    // starting the ConnectionTx task sent the initial handshake packet
    let tx_handle = tokio::task::spawn(tx.start());

    loop {
        // wait for the response
        rx_notify.notified().await;
        {
            let conn = connection.lock();

            trace!("connection {:?} established={}, early_data={}, closed={}, draining={}, readable={}, timed_out={}, resumed={}",
                conn_id, conn.is_established(), conn.is_in_early_data(), conn.is_closed(),
                conn.is_draining(), conn.is_readable(), conn.is_timed_out(), conn.is_resumed());
            trace!(
                "connection {:?} peer_error={:?}, local_error={:?}",
                conn_id,
                conn.peer_error(),
                conn.local_error()
            );

            handle_connection_errors(&conn_id, conn.peer_error(), conn.local_error())?;
            if conn.is_established() {
                // send response packets
                tx_notify.notify_waiters();
                break;
            }
        }
        // send connection data on ConnectionTx task to continue handshake
        tx_notify.notify_waiters();
    }

    let e_state = EstablishedState {
        connection_id: conn_id.clone(),
        connection: connection.clone(),

        http3_config: configs.http3().clone(),

        socket: socket_details.io.clone(),
        rx_notify: rx_notify.clone(),
        tx_notify: tx_notify.clone(),

        rx_handle,
        tx_handle,
    };

    Ok(e_state)
}
