use bytes::Bytes;
use clap::Parser;
use http::StatusCode;
use log::debug;
use pingora_core::connectors::http::v1::Connector as ConnectorV11;
use pingora_core::connectors::http::v2::Connector as ConnectorV2;
use pingora_core::connectors::ConnectorOptions;
use pingora_core::prelude::HttpPeer;
use pingora_core::protocols::http::v1::client::HttpSession as HttpSessionV11;
use pingora_core::protocols::http::v2::client::Http2Session;
use pingora_http::RequestHeader;
use reqwest::Version;
use std::io::ErrorKind::Unsupported;

#[allow(dead_code, unused_imports)]
#[path = "../benches/utils/mod.rs"]
mod bench_utils;
use bench_utils::{
    generate_random_ascii_data, wait_for_tcp_connect, CERT_PATH, KEY_PATH, TLS_HTTP2_PORT,
};
use pingora_core::protocols::http::client::HttpSession;

const DEFAULT_POOL_SIZE: usize = 2;
const HTTP_HOST: &str = "openrusty.org";
const SERVER_IP: &str = "127.0.0.1";

fn get_tls_connector_options() -> ConnectorOptions {
    let mut options = ConnectorOptions::new(DEFAULT_POOL_SIZE);
    options.ca_file = Some(CERT_PATH.clone());
    options.cert_key_file = Some((CERT_PATH.clone(), KEY_PATH.clone()));
    options
}

async fn connector_http11_session() -> (ConnectorV11, HttpPeer) {
    let connector = ConnectorV11::new(Some(get_tls_connector_options()));
    let peer = get_http_peer(TLS_HTTP2_PORT as i32);

    (connector, peer)
}

async fn connector_http2() -> (ConnectorV2, HttpPeer) {
    let connector = ConnectorV2::new(Some(get_tls_connector_options()));
    let mut peer = get_http_peer(TLS_HTTP2_PORT as i32);
    peer.options.set_http_version(2, 2);
    peer.options.max_h2_streams = 1;

    (connector, peer)
}

async fn session_new_http2(connector: &ConnectorV2, peer: HttpPeer) -> Http2Session {
    let http2_session = connector.new_http_session(&peer).await.unwrap();
    match http2_session {
        HttpSession::H1(_) => panic!("expect h2"),
        HttpSession::H2(h2_stream) => h2_stream,
    }
}

async fn session_new_http11(connector: &ConnectorV11, peer: HttpPeer) -> HttpSessionV11 {
    let (http_session, reused) = connector.get_http_session(&peer).await.unwrap();
    assert!(!reused);
    http_session
}

async fn session_reuse_http2(connector: &ConnectorV2, peer: HttpPeer) -> Http2Session {
    connector.reused_http_session(&peer).await.unwrap().unwrap()
}

async fn session_reuse_http11(connector: &ConnectorV11, peer: HttpPeer) -> HttpSessionV11 {
    connector.reused_http_session(&peer).await.unwrap()
}

fn get_http_peer(port: i32) -> HttpPeer {
    HttpPeer::new(format!("{}:{}", SERVER_IP, port), true, HTTP_HOST.into())
}

async fn post_http11(
    client_reuse: bool,
    connector: ConnectorV11,
    peer: HttpPeer,
    data: Vec<String>,
) {
    let mut first = true;
    for d in data {
        let mut http_session = if client_reuse {
            if first {
                debug!("Creating a new HTTP stream for the first request.");
                session_new_http11(&connector, peer.clone()).await
            } else {
                debug!("Re-using existing HTTP stream for request.");
                session_reuse_http11(&connector, peer.clone()).await
            }
        } else {
            debug!("Using new HTTP stream for request.");
            session_new_http11(&connector, peer.clone()).await
        };

        let mut req = Box::new(RequestHeader::build("POST", b"/", Some(d.len())).unwrap());
        req.append_header("Host", HTTP_HOST).unwrap();
        req.append_header("Content-Length", d.len()).unwrap();
        req.append_header("Content-Type", "text/plain").unwrap();
        debug!("request_headers: {:?}", req.headers);

        http_session.write_request_header(req).await.unwrap();
        http_session.write_body(d.as_bytes()).await.unwrap();

        let res_headers = *http_session.read_resp_header_parts().await.unwrap();
        debug!("response_headers: {:?}", res_headers);

        let res_body = http_session.read_body_bytes().await.unwrap().unwrap();
        debug!("res_body: {:?}", res_body);
        assert_eq!(res_body, Bytes::from(d.clone()));

        assert_eq!(res_headers.version, http::version::Version::HTTP_11);
        assert_eq!(res_headers.status, StatusCode::OK);

        if client_reuse {
            connector
                .release_http_session(http_session, &peer, None)
                .await;
            first = false;
        }
    }
}

async fn post_http2(client_reuse: bool, connector: ConnectorV2, peer: HttpPeer, data: Vec<String>) {
    let mut first = true;
    for d in data {
        let mut http_session = if client_reuse {
            if first {
                debug!("Creating a new HTTP stream for the first request.");
                session_new_http2(&connector, peer.clone()).await
            } else {
                debug!("Re-using existing HTTP stream for request.");
                session_reuse_http2(&connector, peer.clone()).await
            }
        } else {
            debug!("Using new HTTP stream for request.");
            session_new_http2(&connector, peer.clone()).await
        };

        let mut req = Box::new(RequestHeader::build("POST", b"/", Some(d.len())).unwrap());
        req.append_header("Host", HTTP_HOST).unwrap();
        req.append_header("Content-Length", d.len()).unwrap();
        req.append_header("Content-Type", "text/plain").unwrap();
        debug!("res_headers: {:?}", req.headers);

        http_session.write_request_header(req, false).unwrap();
        http_session
            .write_request_body(Bytes::from(d.clone()), true)
            .unwrap();
        http_session.finish_request_body().unwrap();

        http_session.read_response_header().await.unwrap();
        let res_body = http_session.read_response_body().await.unwrap().unwrap();
        debug!("res_body: {:?}", res_body);
        assert_eq!(res_body, Bytes::from(d));

        let res_headers = http_session.response_header().unwrap();
        debug!("res_header: {:?}", res_headers);
        assert_eq!(res_headers.version, http::version::Version::HTTP_2);
        assert_eq!(res_headers.status, StatusCode::OK);

        if client_reuse {
            connector.release_http_session(http_session, &peer, None);
            first = false;
        }
    }
}

async fn connector_tls_post_data(client_reuse: bool, version: Version, data: Vec<String>) {
    match version {
        Version::HTTP_11 => {
            let (connector, peer) = connector_http11_session().await;
            post_http11(client_reuse, connector, peer, data).await;
        }
        Version::HTTP_2 => {
            let (connector, peer) = connector_http2().await;
            post_http2(client_reuse, connector, peer, data).await;
        }
        _ => {
            panic!("HTTP version not supported.")
        }
    };
}

fn http_version_parser(version: &str) -> Result<Version, std::io::Error> {
    match version {
        "HTTP/1.1" => Ok(Version::HTTP_11),
        "HTTP/2.0" => Ok(Version::HTTP_2),
        _ => Err(std::io::Error::from(Unsupported)),
    }
}

#[derive(Parser, Debug)]
struct Args {
    #[clap(long, parse(try_from_str = http_version_parser))]
    http_version: Version,

    #[clap(long, action = clap::ArgAction::Set)]
    stream_reuse: bool,

    #[clap(long)]
    request_count: i32,

    #[clap(long)]
    request_size: usize,
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let args = Args::parse();
    println!("{:?}", args);

    println!("Waiting for TCP connect...");
    wait_for_tcp_connect(args.http_version).await;
    println!("TCP connect successful.");

    println!("Starting to send benchmark requests.");
    connector_tls_post_data(
        args.stream_reuse,
        args.http_version,
        generate_random_ascii_data(args.request_count, args.request_size),
    )
    .await;
    println!("Successfully sent benchmark requests.");
}
