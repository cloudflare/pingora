use std::net::SocketAddr;
use std::sync::Arc;
use log::{debug, error, trace, warn};
use parking_lot::Mutex;
use quiche::ConnectionId;
use tokio::net::UdpSocket;
use tokio::sync::mpsc::error::TryRecvError;
use tokio::sync::Notify;
use pingora_error::{Error, ErrorType, OrErr};
use crate::protocols::ConnectionState;
use crate::protocols::l4::quic::{Connection, ConnectionTx, EstablishedHandle, EstablishedState, HandshakeResponse, IncomingState, TxBurst, MAX_IPV6_UDP_PACKET_SIZE};
use crate::protocols::l4::quic::id_token::{mint_token, validate_token};
use crate::protocols::l4::stream::Stream as L4Stream;

pub(crate) async fn handshake(mut stream: L4Stream) -> pingora_error::Result<L4Stream> {
    let Some(connection) = stream.quic_connection_state() else {
        debug_assert!(false, "quic::handshake called on stream of another type");
        return Err(Error::explain(ErrorType::InternalError, "stream is not a quic stream"))
    };

    let e_state = match connection {
        Connection::Incoming(i) => {
            if let Some(e_state) = handshake_inner(i).await? {
                // send HANDSHAKE_DONE Quic frame on established connection
                e_state.tx_notify.notify_waiters();
                e_state.tx_flushed.notified().await;
                Some(e_state)
            } else {
                debug!("handshake either rejected or ignored for connection {:?}", i.connection_id);
                None
            }
        }
        Connection::Established(_) => {
            debug_assert!(false, "quic::handshake on already established connection");
            return Err(Error::explain(ErrorType::HandshakeError, "handshake state not of type incoming"))
        }
    };

    if let Some(e_state) = e_state {
        connection.establish(e_state)?;
        Ok(stream)
    } else {
        Err(Error::explain(ErrorType::HandshakeError, "handshake rejected or ignored"))
    }
}

async fn handshake_inner(state: &mut IncomingState) -> pingora_error::Result<Option<EstablishedState>> {
    let IncomingState {
        connection_id: conn_id,
        config,
        drop_connection,

        socket,
        socket_details,
        udp_rx,
        dgram,

        response,

        ignore,
        reject
    } = state;

    if *ignore {
        {
            let mut resp = response.lock();
            *resp = Some(HandshakeResponse::Ignored)
        }
        return Ok(None);
    } else if *reject {
        {
            let mut resp = response.lock();
            *resp = Some(HandshakeResponse::Rejected)
        }
        return Ok(None);
        // TODO: send to peer, return err if send fails
    }

    let initial_dcid = dgram.header.dcid.clone();

    // TODO: use correct buf sizes for IPv4 & IPv6
    // for now use IPv6 values as they are smaller, should work as well on IPv4
    let mut out = [0u8; MAX_IPV6_UDP_PACKET_SIZE];

    if !quiche::version_is_supported(dgram.header.version) {
        warn!("Quic packet version received is not supported. Negotiating version...");
        let size = quiche::negotiate_version(&dgram.header.scid, &dgram.header.dcid, &mut out)
            .explain_err(
                ErrorType::HandshakeError, |_| "creating version negotiation packet failed")?;

        // send data to network
        send_dgram(conn_id, &socket, &out[..size], dgram.recv_info.from).await
            .explain_err(
                ErrorType::WriteError, |_| "sending version negotiation packet failed")?;

        // validate response
        if let Some(resp_dgram) = udp_rx.recv().await {
            if quiche::version_is_supported(resp_dgram.header.version) {
                *dgram = resp_dgram
            } else {
                return Err(Error::explain(
                    ErrorType::HandshakeError,
                    "version negotiation failed as responded version is not supported"));
            };
        } else {
            return Err(Error::explain(
                ErrorType::HandshakeError,"version negotiation did not receive a response"));
        }
    };

    // token is always present in "Initial" packets
    let token = dgram.header.token.as_ref().unwrap();
    // do stateless retry if the client didn't send a token
    if token.is_empty() {
        trace!("connection {:?} stateless retry as Quic header token is empty", conn_id);

        let hdr = &dgram.header;
        let new_token = mint_token(&hdr, &dgram.recv_info.from);
        let size = quiche::retry(
            &hdr.scid,
            &hdr.dcid,
            &conn_id,
            &new_token,
            hdr.version,
            &mut out,
        ).explain_err(ErrorType::HandshakeError, |_| "creating retry packet failed")?;

        send_dgram(&conn_id, &socket, &out[..size], dgram.recv_info.from).await
            .explain_err(ErrorType::WriteError, |_| "sending retry packet failed")?;

        // validate response
        if let Some(resp_dgram) = udp_rx.recv().await {
            // token is always present in "Initial" packets
            let resp_token = resp_dgram.header.token.as_ref().unwrap();
            if resp_token.is_empty() {
                return Err(Error::explain(
                    ErrorType::HandshakeError,
                    "Stateless retry failed. Still no token available after stateless retry.".to_string()));
            } else {
                *dgram = resp_dgram;
            };
        } else {
            return Err(Error::explain(
                ErrorType::HandshakeError,
                "Stateless retry did not receive a response.".to_string()));
        }
    }

    let hdr = &dgram.header;
    let token = hdr.token.as_ref().unwrap();
    let odcid = validate_token(&dgram.recv_info.from, token);

    // The token was not valid, meaning the retry failed, so drop the connection.
    if odcid.is_none() {
        return Err(Error::explain(
            ErrorType::HandshakeError,
            "Quic header has invalid address validation token.".to_string()));
    }

    // The destination id was not valid, so drop the connection.
    if conn_id.len() != hdr.dcid.len() {
        return Err(Error::explain(
            ErrorType::HandshakeError,
            "Quic header has invalid destination connection id.".to_string()));
    }

    // Reuse the source connection ID we sent in the Retry packet,
    // instead of changing it again.
    debug!("new connection {:?} odcid={:?} scid={:?} ", hdr.dcid, initial_dcid, hdr.scid);

    let mut conn;
    {
        let mut config = config.lock();
        conn = quiche::accept(&hdr.dcid, Some(&initial_dcid), dgram.recv_info.to, dgram.recv_info.from, &mut config)
            .explain_err(ErrorType::HandshakeError, |_| "connection instantiation failed")?;
    }

    // receive quic data into connection
    let buf = dgram.pkt.as_mut_slice();
    conn.recv(buf, dgram.recv_info)
        .explain_err(ErrorType::HandshakeError, |_| "receiving initial data failed")?;

    debug!("connection {:?} starting handshake", conn_id);
    // RSA handshake requires more than one packet
    while !conn.is_established() {
        trace!("connection {:?} creating handshake packet", conn_id);
        'tx: loop {
            let (size, info) = match conn.send(out.as_mut_slice()) {
                Ok((size, info)) => (size, info),
                Err(quiche::Error::Done) => break 'tx,
                Err(e) => return Err(e).explain_err(
                    ErrorType::WriteError, |_| "creating handshake packet failed"),
            };

            trace!("connection {:?} sending handshake packet", conn_id);
            send_dgram(&conn_id, &socket, &out[..size], info.to).await
                .explain_err(ErrorType::WriteError, |_| "sending handshake packet failed")?;
        }

        trace!("connection {:?} waiting for handshake response", conn_id);
        'rx: loop {
            if let Some(mut dgram) = udp_rx.recv().await {
                trace!("connection {:?} received handshake response", conn_id);
                conn.recv(dgram.pkt.as_mut_slice(), dgram.recv_info)
                    .explain_err(
                        ErrorType::HandshakeError, |_| "receiving handshake response failed")?;
            } else {
                return Err(Error::explain(
                    ErrorType::HandshakeError,
                    "finishing handshake failed, did not receive a response"));
            }
            if udp_rx.is_empty() {
                break 'rx;
            }
        }

        trace!("connection {:?} established={}, early_data={}, closed={}, draining={}, readable={}, timed_out={}, resumed={}",
                conn_id, conn.is_established(), conn.is_in_early_data(), conn.is_closed(),
                conn.is_draining(), conn.is_readable(), conn.is_timed_out(), conn.is_resumed());

        trace!("connection {:?} peer_error={:?}, local_error={:?}", conn_id, conn.peer_error(), conn.local_error());
        if let Some(e) = conn.peer_error() {
            error!("connection {:?} peer error reason: {}", conn_id, String::from_utf8_lossy(e.reason.as_slice()).to_string());
        }
        if let Some(e) = conn.local_error() {
            error!("connection {:?} local error reason: {}", conn_id, String::from_utf8_lossy(e.reason.as_slice()).to_string());
        }
    }

    let max_send_udp_payload_size = conn.max_send_udp_payload_size();
    let connection_id = conn_id;
    let connection = Arc::new(Mutex::new(conn));
    let tx_notify = Arc::new(Notify::new());
    let rx_notify = Arc::new(Notify::new());
    let tx_flushed = Arc::new(Notify::new());

    debug!("connection {:?} handshake successful, udp_rx {}", connection_id, udp_rx.len());
    let handle = EstablishedHandle {
        connection_id: connection_id.clone(),
        connection: connection.clone(),
        rx_notify: rx_notify.clone(),
        tx_notify: tx_notify.clone()
    };

    {
        let mut resp = response.lock();
        *resp = Some(HandshakeResponse::Established(handle));
    }

    let tx = ConnectionTx {
        socket: socket.clone(),
        socket_details: socket_details.clone(),
        connection_id: connection_id.clone(),
        connection: connection.clone(),
        tx_notify: tx_notify.clone(),
        tx_flushed: tx_flushed.clone(),
        tx_stats: TxBurst::new(max_send_udp_payload_size),
    };

    let state = EstablishedState {
        socket: socket.clone(),
        tx_handle: tokio::spawn(tx.start_tx()),

        connection_id: connection_id.clone(),
        connection: connection.clone(),
        drop_connection: drop_connection.clone(),

        rx_notify: rx_notify.clone(),
        tx_notify: tx_notify.clone(),
        tx_flushed: tx_flushed.clone(),
    };

    Ok(Some(state))
}


// connection io tx directly via socket
async fn send_dgram(id: &ConnectionId<'_>, io: &Arc<UdpSocket>, buf: &[u8], to: SocketAddr) -> pingora_error::Result<usize> {
    match io.send_to(buf, &to).await {
        Ok(sent) => {
            debug_assert_eq!(sent, buf.len(), "amount of network sent data does not correspond to packet size");
            trace!("connection {:?} sent dgram to={:?} length={:?} ", id, to, buf.len());
            Ok(sent)
        }
        Err(e) => {
            error!("Failed sending packet via UDP. Error: {:?}", e);
            Err(Error::explain(
                ErrorType::WriteError, format!("Failed sending packet via UDP. Error: {:?}", e)))
        }
    }
}