use ahash::{HashMap, HashMapExt};
use iai_callgrind::{
    binary_benchmark, binary_benchmark_group, main, BinaryBenchmarkConfig, Command,
    FlamegraphConfig, Stdio, Tool, ValgrindTool,
};
use iai_callgrind::{Pipe, Stdin};
use log::{debug, info, LevelFilter};
use regex::Regex;
use reqwest::blocking::Client;
use reqwest::{Certificate, Error, Response, StatusCode, Version};
use std::env;
use std::fs::File;
use std::io::{Read, Stdout};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use tokio::task::JoinSet;

mod utils;

use utils::{
    generate_random_ascii_data, http_version_to_port, wait_for_tcp_connect, CERT_PATH,
    TLS_HTTP11_PORT, TLS_HTTP2_PORT,
};

main!(binary_benchmark_groups = tls_acceptor);

binary_benchmark_group!(
    name = tls_acceptor;
    config = BinaryBenchmarkConfig::default()
        .flamegraph(FlamegraphConfig::default())
        .tool(Tool::new(ValgrindTool::DHAT))
        .raw_callgrind_args([""
            // NOTE: toggle values can be extracted from .out files
            // see '^fn=' values, need to be suffixed with '*' or '()'
            // grep -E '^fn=' *.out | cut -d '=' -f2- | sort -u
            //"--toggle-collect=pingora_core::services::listening::Service<A>::run_endpoint*"
            // NOTE: for usage with callgrind::start_instrumentation() & stop_instrumentation()
            //"--instr-atstart=no"
    ]);
    benchmarks = bench_server_sequential, bench_server_parallel
);

static SEQUENTIAL_REQUEST_COUNT: i32 = 64;
static SEQUENTIAL_REQUEST_SIZE: usize = 64;
#[binary_benchmark]
#[bench::http_11_handshake_always(args = (Version::HTTP_11),
    setup = send_requests(1, false, Version::HTTP_11, SEQUENTIAL_REQUEST_COUNT, SEQUENTIAL_REQUEST_SIZE))]
#[bench::http_11_handshake_once(args = (Version::HTTP_11),
    setup = send_requests(1, true, Version::HTTP_11, SEQUENTIAL_REQUEST_COUNT, SEQUENTIAL_REQUEST_SIZE))]
#[bench::http_2_handshake_always(args = (Version::HTTP_2),
    setup = send_requests(1, false, Version::HTTP_2, SEQUENTIAL_REQUEST_COUNT, SEQUENTIAL_REQUEST_SIZE))]
#[bench::http_2_handshake_once(args = (Version::HTTP_2),
    setup = send_requests(1, true, Version::HTTP_2, SEQUENTIAL_REQUEST_COUNT, SEQUENTIAL_REQUEST_SIZE))]
fn bench_server_sequential(http_version: Version) -> Command {
    let path = format!(
        "{}/../target/release/examples/bench_server",
        env!("CARGO_MANIFEST_DIR")
    );
    Command::new(path)
        // TODO: currently a workaround to keep the setup function running parallel with benchmark execution
        .stdin(Stdin::Setup(Pipe::Stderr))
        .args([format!("--http-version={:?}", http_version)])
        .build()
}

static PARALLEL_ACCEPTORS: u16 = 16;
static PARALLEL_REQUEST_COUNT: i32 = SEQUENTIAL_REQUEST_COUNT / PARALLEL_ACCEPTORS as i32;
static PARALLEL_REQUEST_SIZE: usize = 64;
#[binary_benchmark]
#[bench::http_11_handshake_always(args = (PARALLEL_ACCEPTORS, Version::HTTP_11),
    setup = send_requests(PARALLEL_ACCEPTORS, false, Version::HTTP_11, PARALLEL_REQUEST_COUNT, PARALLEL_REQUEST_SIZE))]
#[bench::http_11_handshake_once(args = (PARALLEL_ACCEPTORS, Version::HTTP_11),
    setup = send_requests(PARALLEL_ACCEPTORS, true, Version::HTTP_11, PARALLEL_REQUEST_COUNT, PARALLEL_REQUEST_SIZE))]
#[bench::http_2_handshake_always(args = (PARALLEL_ACCEPTORS, Version::HTTP_2),
    setup = send_requests(PARALLEL_ACCEPTORS, false, Version::HTTP_2, PARALLEL_REQUEST_COUNT, PARALLEL_REQUEST_SIZE))]
#[bench::http_2_handshake_once(args = (PARALLEL_ACCEPTORS, Version::HTTP_2),
    setup = send_requests(PARALLEL_ACCEPTORS, true, Version::HTTP_2, PARALLEL_REQUEST_COUNT, PARALLEL_REQUEST_SIZE))]
fn bench_server_parallel(parallel_acceptors: u16, http_version: Version) -> Command {
    let path = format!(
        "{}/../target/release/examples/bench_server",
        env!("CARGO_MANIFEST_DIR")
    );
    Command::new(path)
        // TODO: currently a workaround to keep the setup function running parallel with benchmark execution
        .stdin(Stdin::Setup(Pipe::Stderr))
        .args([
            format!("--http-version={:?}", http_version),
            format!("--parallel-acceptors={}", parallel_acceptors),
        ])
        .build()
}

fn send_requests(
    parallel_acceptors: u16,
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
            wait_for_tcp_connect(http_version, parallel_acceptors).await;
            println!("TCP connect successful.");

            println!("Sending benchmark requests...");
            tls_post_data(
                parallel_acceptors,
                client_reuse,
                http_version,
                generate_random_ascii_data(request_count, request_size),
            )
            .await;
            println!("Benchmark requests successfully sent.");
            println!("Waiting for server(s) to gracefully shutdown...");
        })
}

async fn tls_post_data(
    parallel_acceptors: u16,
    client_reuse: bool,
    http_version: Version,
    data: Vec<String>,
) {
    if http_version != Version::HTTP_2 && http_version != Version::HTTP_11 {
        panic!("HTTP version not supported");
    }
    let base_port = http_version_to_port(http_version);

    let mut clients: HashMap<String, Client> = HashMap::new();
    if client_reuse {
        for i in 0..parallel_acceptors {
            if http_version == Version::HTTP_11 {
                clients.insert(i.to_string(), client_http11(base_port + i));
            }
            if http_version == Version::HTTP_2 {
                clients.insert(i.to_string(), client_http2(base_port + i));
            };
        }
    };

    let mut req_set = JoinSet::new();
    for i in 0..parallel_acceptors {
        // spawn request for all elements within data
        for d in data.iter() {
            if client_reuse {
                // reuse same connection & avoid new handshake
                let reuse_client = clients.get(&i.to_string()).unwrap();
                req_set.spawn(post_data(
                    reuse_client.to_owned(),
                    http_version,
                    base_port + i,
                    d.to_string(),
                ));
            } else {
                // always create a new client to ensure handshake is performed
                if http_version == Version::HTTP_11 {
                    req_set.spawn(post_data(
                        client_http11(base_port + i),
                        http_version,
                        base_port + i,
                        d.to_string(),
                    ));
                }
                if http_version == Version::HTTP_2 {
                    req_set.spawn(post_data(
                        client_http2(base_port + i),
                        http_version,
                        base_port + i,
                        d.to_string(),
                    ));
                };
            }
        }
    }
    // wait for all responses
    while let Some(res) = req_set.join_next().await {
        res.unwrap();
    }
}

async fn post_data(client: Client, version: Version, port: u16, data: String) {
    // using blocking client to reuse same connection in case of client_reuse=true
    // async client does not ensure to use the same connection, will start a new one
    // in case the existing is still blocked
    let resp = client
        .post(format! {"https://openrusty.org:{}", port})
        .body(data)
        .send()
        .unwrap_or_else(|err| {
            println!("HTTP client error: {err}");
            panic!("error: {err}");
        });

    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.version(), version);

    // read full response, important for consistent tests
    let _resp_body = resp.text().unwrap();
    // println!("resp_body: {}", resp_body)
}

fn client_http11(port: u16) -> Client {
    let socket = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), port);
    Client::builder()
        .resolve_to_addrs("openrusty.org", &[socket])
        .add_root_certificate(read_cert())
        .build()
        .unwrap()
}

fn client_http2(port: u16) -> Client {
    let socket = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), port);
    Client::builder()
        .resolve_to_addrs("openrusty.org", &[socket])
        .add_root_certificate(read_cert())
        // avoid error messages during first set of connections (os error 32, broken pipe)
        .http2_prior_knowledge()
        .build()
        .unwrap()
}

fn read_cert() -> Certificate {
    let mut buf = Vec::new();
    File::open(CERT_PATH.to_string())
        .unwrap()
        .read_to_end(&mut buf)
        .unwrap();
    Certificate::from_pem(&buf).unwrap()
}
