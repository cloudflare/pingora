use std::net::SocketAddr;
use std::sync::Arc;
use log::{debug, error, trace, warn};
use parking_lot::Mutex;
use tokio::net::UdpSocket;
use tokio::sync::Notify;
use pingora_error::{Error, ErrorType, OrErr};
use crate::protocols::ConnectionState;
use crate::protocols::l4::quic::{Connection, ConnectionTx, EstablishedHandle, EstablishedState, HandshakeResponse, IncomingState, TxBurst, MAX_IPV6_QUIC_DATAGRAM_SIZE, MAX_IPV6_UDP_PACKET_SIZE};
use crate::protocols::l4::quic::id_token::{mint_token, validate_token};
use crate::protocols::l4::quic::sendto::{detect_gso, set_txtime_sockopt};
use crate::protocols::l4::stream::Stream as L4Stream;

pub(crate) async fn handshake(mut stream: L4Stream) -> pingora_error::Result<L4Stream> {
    let Some(connection) = stream.quic_connection_state() else {
        debug_assert!(false, "quic::handshake called on stream of another type");
        return Err(Error::explain(ErrorType::InternalError, "stream is not a quic stream"))
    };

    let e_state = match connection {
        Connection::Incoming(s) => {
            if let Some((e_state, e_handle)) = handshake_inner(s).await? {
                s.response_tx.send(HandshakeResponse::Established(e_handle)).await
                    .explain_err(ErrorType::WriteError,
                                 |e| format!("Sending HandshakeResponse failed with {}", e))?;
                Some(e_state)
            } else {
                debug!("handshake either rejected or ignored for connection {:?}", s.id);
                None
            }
        }
        Connection::Established(_) => {
            debug_assert!(false, "quic::handshake on already established connection");
            return Err(Error::explain(ErrorType::HandshakeError, "handshake state not of type incoming"))
        }
    };

    if let Some(e_state) = e_state {
        connection.establish(e_state);
        Ok(stream)
    } else {
        Err(Error::explain(ErrorType::HandshakeError, "handshake rejected or ignored"))
    }
}

async fn handshake_inner(state: &mut IncomingState) -> pingora_error::Result<Option<(EstablishedState, EstablishedHandle)>> {
    let IncomingState {
        id,
        config,

        socket,
        udp_rx,
        dgram,

        response_tx,

        ignore,
        reject
    } = state;

    if *ignore {
        if let Err(_) = response_tx.send(HandshakeResponse::Ignored).await {
            trace!("failed sending endpoint response for incoming connection id={:?}.", id)
        };
        return Ok(None);
    } else if *reject {
        if let Err(_) = response_tx.send(HandshakeResponse::Rejected).await {
            trace!("failed sending endpoint response for incoming connection id={:?}.", id)
        };
        return Ok(None);
        // TODO: send to peer, return err if send fails
    }

    let initial_dcid = dgram.header.dcid.clone();

    // TODO: use correct buf sizes for IPv4 & IPv6
    // for now use IPv6 values as they are smaller, should work as well on IPv4
    let mut out = [0u8; MAX_IPV6_UDP_PACKET_SIZE];

    if !quiche::version_is_supported(dgram.header.version) {
        warn!("QUIC packet version received is not supported. Negotiating version...");
        let size = quiche::negotiate_version(&dgram.header.scid, &dgram.header.dcid, &mut out)
            .map_err(|e| Error::explain(
                ErrorType::HandshakeError,
                format!("Creating version negotiation packet failed. Error: {:?}", e)))?;

        // send data to network
        send_dgram(&socket, &out[..size], dgram.recv_info.from).await
            .map_err(|e| Error::explain(
                ErrorType::WriteError,
                format!("Sending version negotiation packet failed. Error: {:?}", e)))?;

        // validate response
        if let Some(resp_dgram) = udp_rx.recv().await {
            if quiche::version_is_supported(resp_dgram.header.version) {
                *dgram = resp_dgram
            } else {
                return Err(Error::explain(
                    ErrorType::HandshakeError,
                    "Version negotiation failed responded version is not supported.".to_string()));
            };
        } else {
            return Err(Error::explain(
                ErrorType::HandshakeError,
                "Version negotiation did not receive a response".to_string()));
        }
    };

    // token is always present in "Initial" packets
    let token = dgram.header.token.as_ref().unwrap();
    // do stateless retry if the client didn't send a token
    if token.is_empty() {
        debug!("stateless retry as Quic header token is empty");

        let hdr = &dgram.header;
        let new_token = mint_token(&hdr, &dgram.recv_info.from);
        let size = quiche::retry(
            &hdr.scid,
            &hdr.dcid,
            &id,
            &new_token,
            hdr.version,
            &mut out,
        ).map_err(|e| Error::explain(
            ErrorType::HandshakeError,
            format!("Creating retry packet failed. Error: {:?}", e)))?;

        send_dgram(&socket, &out[..size], dgram.recv_info.from).await
            .map_err(|e| Error::explain(
                ErrorType::WriteError,
                format!("Sending retry packet failed. Error: {:?}", e)))?;

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
    if id.len() != hdr.dcid.len() {
        return Err(Error::explain(
            ErrorType::HandshakeError,
            "Quic header has invalid destination connection id.".to_string()));
    }

    // Reuse the source connection ID we sent in the Retry packet,
    // instead of changing it again.
    trace!("new Quic connection odcid={:?} dcid={:?} scid={:?} ", initial_dcid, hdr.dcid, hdr.scid);

    let mut conn;
    {
        let mut config = config.lock();
        conn = quiche::accept(&hdr.dcid, Some(&initial_dcid), dgram.recv_info.to, dgram.recv_info.from, &mut config)
            .map_err(|e| Error::explain(
                ErrorType::HandshakeError,
                format!("Connection instantiation failed. Error: {:?}", e)))?;
    }

    // receive quic data into connection
    let buf = dgram.pkt.as_mut_slice();
    conn.recv(buf, dgram.recv_info)
        .map_err(|e| Error::explain(
            ErrorType::HandshakeError,
            format!("Recieving initial data failed. Error: {:?}", e)))?;

    trace!("starting handshake for connection {:?}", id);
    // RSA handshake requires more than one packet
    while !conn.is_established() {
        trace!("creating handshake packet");
        let (size, info) = conn.send(out.as_mut_slice())
            .map_err(|e| Error::explain(
                ErrorType::WriteError,
                format!("creating handshake packet failed with {:?}", e)))?;

        trace!("sending handshake packet");
        send_dgram(&socket, &out[..size], info.to).await
            .map_err(|e| Error::explain(
                ErrorType::WriteError,
                format!("sending handshake packet failed with {:?}", e)))?;

        // FIXME: this should be looped till empty
        trace!("waiting for handshake response");
        if let Some(mut dgram) = udp_rx.recv().await {
            trace!("received handshake response");
            let buf = dgram.pkt.as_mut_slice();
            conn.recv(buf, dgram.recv_info)
                .map_err(|e| Error::explain(
                    ErrorType::HandshakeError,
                    format!("receiving handshake response failed with {:?}", e)))?;
        } else {
            return Err(Error::explain(
                ErrorType::HandshakeError,
                "finishing handshake failed, did not receive a response"));
        }

        trace!("connection established={}, early_data={}, closed={}, draining={}, readable={}, timed_out={}, resumed={}",
                conn.is_established(), conn.is_in_early_data(), conn.is_closed(),
                conn.is_draining(), conn.is_readable(), conn.is_timed_out(), conn.is_resumed());

        trace!("connection peer_error={:?}, local_error={:?}", conn.peer_error(), conn.local_error());
        match conn.peer_error() {
            None => {}
            Some(e) => {
                error!("{}", String::from_utf8_lossy(e.reason.as_slice()).to_string())
            }
        }
        match conn.local_error() {
            None => {}
            Some(e) => {
                error!("{}", String::from_utf8_lossy(e.reason.as_slice()).to_string())
            }
        }
    }
    trace!("handshake successful for connection {:?}", id);

    let max_send_udp_payload_size = conn.max_send_udp_payload_size();
    // TODO: move to listener/socket creation
    let gso_enabled = detect_gso(&*socket, MAX_IPV6_QUIC_DATAGRAM_SIZE);
    let pacing_enabled = match set_txtime_sockopt(&*socket) {
        Ok(_) => {
            debug!("successfully set SO_TXTIME socket option");
            true
        },
        Err(e) => {
            debug!("setsockopt failed {:?}", e);
            false
        },
    };

    let tx_notify = Arc::new(Notify::new());
    let rx_notify = Arc::new(Notify::new());
    let connection_id = conn.trace_id().to_string();
    let connection = Arc::new(Mutex::new(conn));

    let tx = ConnectionTx {
        socket: socket.clone(),
        connection_id,
        connection: connection.clone(),
        tx_notify: tx_notify.clone(),
        tx_stats: TxBurst::new(max_send_udp_payload_size),
        gso_enabled,
        pacing_enabled,
    };

    let state = EstablishedState {
        socket: socket.clone(),
        tx_handle: tokio::spawn(tx.start_tx()),

        connection: connection.clone(),
        rx_notify: rx_notify.clone(),
        tx_notify: tx_notify.clone(),
    };
    let handle = EstablishedHandle {
        connection,
        rx_notify,
        tx_notify
    };

    Ok(Some((state, handle)))
}


// connection io tx directly via socket
async fn send_dgram(io: &Arc<UdpSocket>, buf: &[u8], to: SocketAddr) -> pingora_error::Result<usize> {
    match io.send_to(buf, &to).await {
        Ok(sent) => {
            debug_assert_eq!(sent, buf.len(), "amount of network sent data does not correspond to packet size");
            trace!("sent dgram to={:?} length={:?} ", to, buf.len());
            Ok(sent)
        }
        Err(e) => {
            error!("Failed sending packet via UDP. Error: {:?}", e);
            Err(Error::explain(
                ErrorType::WriteError, format!("Failed sending packet via UDP. Error: {:?}", e)))
        }
    }
}