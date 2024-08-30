use once_cell::sync::Lazy;
use rand::distributions::{Alphanumeric, DistString};
use reqwest::Version;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream};
use std::time::Duration;

pub static CERT_PATH: Lazy<String> = Lazy::new(|| {
    format!(
        "{}/../pingora-proxy/tests/utils/conf/keys/server_rustls.crt",
        env!("CARGO_MANIFEST_DIR")
    )
});
#[allow(dead_code)]
pub static KEY_PATH: Lazy<String> = Lazy::new(|| {
    format!(
        "{}/../pingora-proxy/tests/utils/conf/keys/key.pem",
        env!("CARGO_MANIFEST_DIR")
    )
});
pub static TLS_HTTP11_PORT: u16 = 6204;
pub static TLS_HTTP2_PORT: u16 = 6205;

pub fn generate_random_ascii_data(count: i32, len: usize) -> Vec<String> {
    let mut random_data = vec![];
    for _i in 0..count {
        let random_string = Alphanumeric.sample_string(&mut rand::thread_rng(), len);
        random_data.push(random_string)
    }
    random_data
}

pub fn version_to_port(version: Version) -> u16 {
    match version {
        Version::HTTP_11 => TLS_HTTP11_PORT,
        Version::HTTP_2 => TLS_HTTP2_PORT,
        _ => {
            panic!("HTTP version not supported.")
        }
    }
}

pub async fn wait_for_tcp_connect(version: Version) {
    let port = version_to_port(version);
    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), port);
    while let Err(_err) = TcpStream::connect(addr) {
        let _ = tokio::time::sleep(Duration::from_millis(100)).await;
    }
}
