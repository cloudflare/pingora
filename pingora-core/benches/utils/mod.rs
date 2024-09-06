#![allow(dead_code)]

use once_cell::sync::Lazy;
use rand::distributions::{Alphanumeric, DistString};
use reqwest::Version;
use std::io::ErrorKind::Unsupported;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream};
use std::time::Duration;
use tokio::task::JoinSet;

pub static CERT_PATH: Lazy<String> = Lazy::new(|| {
    format!(
        "{}/../pingora-proxy/tests/utils/conf/keys/server_rustls.crt",
        env!("CARGO_MANIFEST_DIR")
    )
});

pub static KEY_PATH: Lazy<String> = Lazy::new(|| {
    format!(
        "{}/../pingora-proxy/tests/utils/conf/keys/key.pem",
        env!("CARGO_MANIFEST_DIR")
    )
});
pub static TLS_HTTP11_PORT: u16 = 6100;
pub static TLS_HTTP2_PORT: u16 = 7200;

pub fn http_version_parser(version: &str) -> Result<Version, std::io::Error> {
    match version {
        "HTTP/1.1" => Ok(Version::HTTP_11),
        "HTTP/2.0" => Ok(Version::HTTP_2),
        _ => Err(std::io::Error::from(Unsupported)),
    }
}

pub async fn wait_for_tcp_connect(version: Version, parallelism: u16) {
    let port = http_version_to_port(version);

    let mut wait_set = JoinSet::new();
    for i in 0..parallelism {
        wait_set.spawn(async move {
            let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), port + i);
            while let Err(_err) = TcpStream::connect(addr) {
                let _ = tokio::time::sleep(Duration::from_millis(100)).await;
            }
        });
    }
    while !wait_set.is_empty() {
        wait_set.join_next().await;
    }
}

pub fn http_version_to_port(version: Version) -> u16 {
    match version {
        Version::HTTP_11 => TLS_HTTP11_PORT,
        Version::HTTP_2 => TLS_HTTP2_PORT,
        _ => {
            panic!("HTTP version not supported.")
        }
    }
}

pub fn generate_random_ascii_data(count: i32, len: usize) -> Vec<String> {
    let mut random_data = vec![];
    for _i in 0..count {
        let random_string = Alphanumeric.sample_string(&mut rand::thread_rng(), len);
        random_data.push(random_string)
    }
    random_data
}
