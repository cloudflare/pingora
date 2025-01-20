use crate::protocols::l4::quic::{detect_gso_pacing, SocketDetails};
use crate::protocols::l4::quic::{Connection, OutgoingState};
use pingora_error::{ErrorType, OrErr, Result};
use std::sync::Arc;
use tokio::net::UdpSocket;

impl Connection {
    pub fn initiate_outgoing(io: UdpSocket) -> Result<Self> {
        let addr = io.local_addr().explain_err(ErrorType::SocketError, |e| {
            format!("failed to get local address from socket: {}", e)
        })?;

        let (gso_enabled, pacing_enabled) = detect_gso_pacing(&io);
        Ok(Self::Outgoing(OutgoingState {
            socket_details: SocketDetails {
                io: Arc::new(io),
                addr,
                gso_enabled,
                pacing_enabled,
            },
        }))
    }
}
