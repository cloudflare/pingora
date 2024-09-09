use axum::routing::post;
use axum::Router;
use axum_server::tls_rustls::RustlsConfig;
use axum_server::Handle;
use iai_callgrind::{
    binary_benchmark, binary_benchmark_group, main, BinaryBenchmarkConfig, Command,
    FlamegraphConfig, Tool, ValgrindTool,
};
use iai_callgrind::{Pipe, Stdin};
use reqwest::Version;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;
use tokio::task::JoinSet;

mod utils;

use utils::{CERT_PATH, KEY_PATH, TLS_HTTP11_PORT, TLS_HTTP2_PORT};

main!(binary_benchmark_groups = tls_connector);

binary_benchmark_group!(
    name = tls_connector;
    config = BinaryBenchmarkConfig::default()
        .flamegraph(FlamegraphConfig::default())
        .tool(Tool::new(ValgrindTool::DHAT))
        .raw_callgrind_args([""
            // NOTE: toggle values can be extracted from .out files
            // see '^fn=' values, need to be suffixed with '*' or '()'
            // grep -E '^fn=' *.out | cut -d '=' -f2- | sort -u
            //, "--toggle-collect=bench_client::post_http*"
            // NOTE: for usage with callgrind::start_instrumentation() & stop_instrumentation()
            //"--instr-atstart=no"
    ]);
    benchmarks = bench_client
);

static SEQUENTIAL_REQUEST_COUNT: i32 = 64;
static SEQUENTIAL_REQUEST_SIZE: usize = 64;
static PARALLEL_CONNECTORS: u16 = 16;
static PARALLEL_REQUEST_COUNT: i32 = SEQUENTIAL_REQUEST_COUNT / PARALLEL_CONNECTORS as i32;
static PARALLEL_REQUEST_SIZE: usize = 64;
#[binary_benchmark]
#[bench::seq_http_11_handshake_always(setup = start_servers(Version::HTTP_11, 1),
    args = [1, false, Version::HTTP_11, SEQUENTIAL_REQUEST_COUNT, SEQUENTIAL_REQUEST_SIZE])]
#[bench::seq_http_11_handshake_once(setup = start_servers(Version::HTTP_11, 1),
    args = [1, true, Version::HTTP_11, SEQUENTIAL_REQUEST_COUNT, SEQUENTIAL_REQUEST_SIZE])]
#[bench::seq_http_2_handshake_always(setup = start_servers(Version::HTTP_2, 1),
    args = [1, false, Version::HTTP_2, SEQUENTIAL_REQUEST_COUNT, SEQUENTIAL_REQUEST_SIZE])]
#[bench::seq_http_2_handshake_once(setup = start_servers(Version::HTTP_2, 1),
    args = [1, true, Version::HTTP_2, SEQUENTIAL_REQUEST_COUNT, SEQUENTIAL_REQUEST_SIZE])]
#[bench::par_http_11_handshake_always(setup = start_servers(Version::HTTP_11, PARALLEL_CONNECTORS),
    args = [PARALLEL_CONNECTORS, false, Version::HTTP_11, PARALLEL_REQUEST_COUNT, PARALLEL_REQUEST_SIZE])]
#[bench::par_http_11_handshake_once(setup = start_servers(Version::HTTP_11, PARALLEL_CONNECTORS),
    args = [PARALLEL_CONNECTORS, true, Version::HTTP_11, PARALLEL_REQUEST_COUNT, PARALLEL_REQUEST_SIZE])]
#[bench::par_http_2_handshake_always(setup = start_servers(Version::HTTP_2, PARALLEL_CONNECTORS),
    args = [PARALLEL_CONNECTORS, false, Version::HTTP_2, PARALLEL_REQUEST_COUNT, PARALLEL_REQUEST_SIZE])]
#[bench::par_http_2_handshake_once(setup = start_servers(Version::HTTP_2, PARALLEL_CONNECTORS),
    args = [PARALLEL_CONNECTORS, true, Version::HTTP_2, PARALLEL_REQUEST_COUNT, PARALLEL_REQUEST_SIZE])]
fn bench_client(
    parallel_connectors: u16,
    stream_reuse: bool,
    http_version: Version,
    request_count: i32,
    request_size: usize,
) -> Command {
    let path = format!(
        "{}/../target/release/examples/bench_client",
        env!("CARGO_MANIFEST_DIR")
    );
    Command::new(path)
        // TODO: currently a workaround to keep the setup function running parallel with benchmark execution
        .stdin(Stdin::Setup(Pipe::Stderr))
        .args([
            format!("--parallel-connectors={}", parallel_connectors),
            format!("--stream-reuse={}", stream_reuse),
            format!("--http-version={:?}", http_version),
            format!("--request-count={}", request_count),
            format!("--request-size={}", request_size),
        ])
        .build()
}

fn start_servers(http_version: Version, parallel_connectors: u16) {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let mut server_set = JoinSet::new();

            for i in 0..parallel_connectors {
                server_set.spawn(start_server(http_version, i));
            }

            while !server_set.is_empty() {
                server_set.join_next().await;
            }
        });
}

async fn start_server(http_version: Version, port_offset: u16) {
    let addr = match http_version {
        Version::HTTP_11 => SocketAddr::from(([127, 0, 0, 1], TLS_HTTP11_PORT + port_offset)),
        Version::HTTP_2 => SocketAddr::from(([127, 0, 0, 1], TLS_HTTP2_PORT + port_offset)),
        _ => {
            panic!("HTTP version not supported.")
        }
    };

    let app = Router::new().route("/", post(|body: String| async { body }));
    let handle = Handle::new();

    // configure certificate and private key used by https
    let config = RustlsConfig::from_pem_file(PathBuf::from(&*CERT_PATH), PathBuf::from(&*KEY_PATH))
        .await
        .unwrap();

    let (http_server, _) = tokio::join!(
        axum_server::bind_rustls(addr, config.clone())
            .handle(handle.clone())
            .serve(app.into_make_service()),
        graceful_shutdown(handle),
    );
    http_server.unwrap();
}

async fn graceful_shutdown(handle: Handle) {
    tokio::time::sleep(Duration::from_secs(10)).await;
    handle.graceful_shutdown(None);
}
