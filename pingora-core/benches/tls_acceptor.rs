use iai_callgrind::{
    binary_benchmark, binary_benchmark_group, main, BinaryBenchmarkConfig, Command,
    FlamegraphConfig,
};
use iai_callgrind::{Pipe, Stdin};
use once_cell::sync::Lazy;
use reqwest::{Certificate, Client, StatusCode, Version};
use std::fs::File;
use std::io::Read;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use tokio::task::JoinSet;

mod utils;

use utils::{
    generate_random_ascii_data, version_to_port, wait_for_tcp_connect, CERT_PATH, TLS_HTTP11_PORT,
    TLS_HTTP2_PORT,
};

fn read_cert() -> Certificate {
    let mut buf = Vec::new();
    File::open(CERT_PATH.to_string())
        .unwrap()
        .read_to_end(&mut buf)
        .unwrap();
    Certificate::from_pem(&buf).unwrap()
}

fn client_http11() -> Client {
    let socket = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), TLS_HTTP11_PORT);
    Client::builder()
        .resolve_to_addrs("openrusty.org", &[socket])
        .add_root_certificate(read_cert())
        .build()
        .unwrap()
}
fn client_http2() -> Client {
    let socket = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), TLS_HTTP2_PORT);
    Client::builder()
        .resolve_to_addrs("openrusty.org", &[socket])
        .add_root_certificate(read_cert())
        // avoid error messages during first set of connections (os error 32, broken pipe)
        .http2_prior_knowledge()
        .build()
        .unwrap()
}

pub static CLIENT_HTTP11: Lazy<Client> = Lazy::new(client_http11);
pub static CLIENT_HTTP2: Lazy<Client> = Lazy::new(client_http2);

/// using with client: None instantiates a new client and performs a full handshake
/// providing Some(client) will re-use the provided client/session
async fn post_data(client_reuse: bool, version: Version, port: u16, data: String) {
    let client = if client_reuse {
        // NOTE: do not perform TLS handshake for each request
        match version {
            Version::HTTP_11 => &*CLIENT_HTTP11,
            Version::HTTP_2 => &*CLIENT_HTTP2,
            _ => {
                panic!("HTTP version not supported.")
            }
        }
    } else {
        // NOTE: perform TLS handshake for each request
        match version {
            Version::HTTP_11 => &client_http11(),
            Version::HTTP_2 => &client_http2(),
            _ => {
                panic!("HTTP version not supported.")
            }
        }
    };

    let resp = client
        .post(format! {"https://openrusty.org:{}", port})
        .body(data)
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.version(), version);

    // read full response, important for consistent tests
    let _resp_body = resp.text().await.unwrap();
    // println!("resp_body: {}", resp_body)
}

async fn tls_post_data(client_reuse: bool, version: Version, data: Vec<String>) {
    let port = version_to_port(version);

    let mut req_set = JoinSet::new();
    // spawn request for all elements within data
    data.iter().for_each(|d| {
        req_set.spawn(post_data(client_reuse, version, port, d.to_string()));
    });

    // wait for all responses
    while let Some(res) = req_set.join_next().await {
        let _ = res.unwrap();
    }
}

fn run_benchmark_requests(
    client_reuse: bool,
    http_version: Version,
    request_count: i32,
    request_size: usize,
) {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            println!("Waiting for TCP connect...");
            wait_for_tcp_connect(http_version).await;
            println!("TCP connect successful.");

            println!("Starting to send benchmark requests.");
            tls_post_data(
                client_reuse,
                http_version,
                generate_random_ascii_data(request_count, request_size),
            )
            .await;
            println!("Successfully sent benchmark requests.");
        })
}

static REQUEST_COUNT: i32 = 128;
static REQUEST_SIZE: usize = 64;
#[binary_benchmark]
#[bench::http_11_handshake_always(setup = run_benchmark_requests(false, Version::HTTP_11, REQUEST_COUNT, REQUEST_SIZE))]
#[bench::http_11_handshake_once(setup = run_benchmark_requests(true, Version::HTTP_11, REQUEST_COUNT, REQUEST_SIZE))]
#[bench::http_2_handshake_always(setup = run_benchmark_requests(false, Version::HTTP_2, REQUEST_COUNT, REQUEST_SIZE))]
#[bench::http_2_handshake_once(setup = run_benchmark_requests(true, Version::HTTP_2, REQUEST_COUNT, REQUEST_SIZE))]
fn bench_server() -> Command {
    let path = format!(
        "{}/../target/release/examples/bench_server",
        env!("CARGO_MANIFEST_DIR")
    );
    Command::new(path)
        // TODO: currently a workaround to keep the setup function running parallel with benchmark execution
        .stdin(Stdin::Setup(Pipe::Stderr))
        .build()
}

binary_benchmark_group!(
    name = tls_acceptor;
    config = BinaryBenchmarkConfig::default()
        .flamegraph(FlamegraphConfig::default())
        .raw_callgrind_args([""
            // NOTE: toggle values can be extracted from .out files
            // see '^fn=' values, need to be suffixed with '*' or '()'
            // grep -E '^fn=' *.out | cut -d '=' -f2- | sort -u
            //"--toggle-collect=pingora_core::services::listening::Service<A>::run_endpoint*"
            // NOTE: for usage with callgrind::start_instrumentation() & stop_instrumentation()
            //"--instr-atstart=no"
    ]);
    benchmarks = bench_server
);

main!(binary_benchmark_groups = tls_acceptor);
