use log::error;
use pingora_error::ErrorType::HandshakeError;
use pingora_error::OrErr;
use quiche::ConnectionId;

pub mod client;
pub mod server;

fn handle_connection_errors(
    conn_id: ConnectionId<'_>,
    local_error: Option<&quiche::ConnectionError>,
    peer_error: Option<&quiche::ConnectionError>,
) -> pingora_error::Result<()> {
    if let Some(e) = local_error {
        error!(
            "connection {:?} local error reason: {}",
            conn_id,
            String::from_utf8_lossy(e.reason.as_slice()).to_string()
        );
        return Err(e).explain_err(HandshakeError, |_| "local error during handshake");
    }

    if let Some(e) = peer_error {
        error!(
            "connection {:?} peer error reason: {}",
            conn_id,
            String::from_utf8_lossy(e.reason.as_slice()).to_string()
        );
        return Err(e).explain_err(HandshakeError, |_| "peer error during handshake");
    }

    Ok(())
}
