use crate::protocols::l4::quic::MAX_IPV6_QUIC_DATAGRAM_SIZE;
use parking_lot::Mutex;
use pingora_error::{ErrorType, OrErr, Result};
use quiche::Config;
use std::sync::Arc;

pub struct Settings {
    config: Arc<Mutex<Config>>,
}

impl Settings {
    pub(crate) fn try_default() -> Result<Self> {
        // TODO: use pingora config values where possible
        // enable user provided default config

        let mut config = Config::new(quiche::PROTOCOL_VERSION)
            .explain_err(ErrorType::InternalError, |_| {
                "Failed to create quiche config."
            })?;

        config
            .load_cert_chain_from_pem_file("/home/hargut/Sources/github.com/pingora/pingora-proxy/tests/utils/conf/keys/server_rustls.crt")
            .explain_err(ErrorType::FileReadError, |_| "Could not load certificate chain from pem file.")?;

        config
            .load_priv_key_from_pem_file("/home/hargut/Sources/github.com/pingora/pingora-proxy/tests/utils/conf/keys/key.pem")
            .explain_err(ErrorType::FileReadError, |_| "Could not load private key from pem file.")?;

        // config.load_verify_locations_from_file() for CA's
        // config.verify_peer(); default server = false; client = true
        // config.discover_pmtu(false); // default false
        config.grease(false); // default true
                              // config.log_keys() && config.set_keylog(); // logging SSL secrets
                              // config.set_ticket_key() // session ticket signer key material

        //config.enable_early_data(); // can lead to ZeroRTT headers during handshake

        config
            .set_application_protos(quiche::h3::APPLICATION_PROTOCOL)
            .explain_err(ErrorType::InternalError, |_| {
                "Failed to set application protocols."
            })?;

        // config.set_application_protos_wire_format();
        // config.set_max_amplification_factor(3); // anti-amplification limit factor; default 3

        config.set_max_idle_timeout(60 * 1000); // default ulimited
        config.set_max_recv_udp_payload_size(MAX_IPV6_QUIC_DATAGRAM_SIZE); // recv default is 65527
        config.set_max_send_udp_payload_size(MAX_IPV6_QUIC_DATAGRAM_SIZE); // send default is 1200
        config.set_initial_max_data(10_000_000); // 10 Mb
        config.set_initial_max_stream_data_bidi_local(1_000_000); // 1 Mb
        config.set_initial_max_stream_data_bidi_remote(1_000_000); // 1 Mb
        config.set_initial_max_stream_data_uni(1_000_000); // 1 Mb
        config.set_initial_max_streams_bidi(100);
        config.set_initial_max_streams_uni(100);

        // config.set_ack_delay_exponent(3); // default 3
        // config.set_max_ack_delay(25); // default 25
        // config.set_active_connection_id_limit(2); // default 2
        // config.set_disable_active_migration(false); // default false

        // config.set_active_connection_id_limit(2); // default 2
        // config.set_disable_active_migration(false); // default false
        // config.set_cc_algorithm_name("cubic"); // default cubic
        // config.set_initial_congestion_window_packets(10); // default 10
        // config.set_cc_algorithm(CongestionControlAlgorithm::CUBIC); // default CongestionControlAlgorithm::CUBIC

        // config.enable_hystart(true); // default true
        // config.enable_pacing(true); // default true
        // config.set_max_pacing_rate(); // default ulimited

        //config.enable_dgram(false); // default false

        // config.set_path_challenge_recv_max_queue_len(3); // default 3
        // config.set_max_connection_window(MAX_CONNECTION_WINDOW); // default 24 Mb
        // config.set_max_stream_window(MAX_STREAM_WINDOW); // default 16 Mb
        // config.set_stateless_reset_token(None) // default None
        // config.set_disable_dcid_reuse(false) // default false

        Ok(Self {
            config: Arc::new(Mutex::new(config)),
        })
    }

    pub(crate) fn get_config(&self) -> Arc<Mutex<Config>> {
        self.config.clone()
    }
}

impl From<Config> for Settings {
    fn from(config: Config) -> Self {
        Self {
            config: Arc::new(Mutex::new(config)),
        }
    }
}
