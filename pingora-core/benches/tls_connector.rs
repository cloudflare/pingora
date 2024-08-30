use axum::routing::post;
use axum::Router;
use axum_server::tls_rustls::RustlsConfig;
use axum_server::Handle;
use iai_callgrind::{
    binary_benchmark, binary_benchmark_group, main, BinaryBenchmarkConfig, Command,
    FlamegraphConfig,
};
use iai_callgrind::{Pipe, Stdin};
use once_cell::sync::Lazy;
use reqwest::Version;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

static CERT_PATH: Lazy<String> = Lazy::new(|| {
    format!(
        "{}/../pingora-proxy/tests/utils/conf/keys/server_rustls.crt",
        env!("CARGO_MANIFEST_DIR")
    )
});
static KEY_PATH: Lazy<String> = Lazy::new(|| {
    format!(
        "{}/../pingora-proxy/tests/utils/conf/keys/key.pem",
        env!("CARGO_MANIFEST_DIR")
    )
});

static TLS_HTTP11_PORT: u16 = 6204;
static TLS_HTTP2_PORT: u16 = 6205;

async fn graceful_shutdown(handle: Handle) {
    tokio::time::sleep(Duration::from_secs(10)).await;

    println!("Sending graceful shutdown signal.");
    handle.graceful_shutdown(None);
}

fn run_benchmark_server() {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let addr_http11 = SocketAddr::from(([127, 0, 0, 1], TLS_HTTP11_PORT));
            let addr_http2 = SocketAddr::from(([127, 0, 0, 1], TLS_HTTP2_PORT));

            let app = Router::new().route("/", post(|body: String| async { body }));

            let handle_http11 = Handle::new();
            let handle_http2 = Handle::new();

            // configure certificate and private key used by https
            let config =
                RustlsConfig::from_pem_file(PathBuf::from(&*CERT_PATH), PathBuf::from(&*KEY_PATH))
                    .await
                    .unwrap();

            let (http11_server, _, http2_server, _) = tokio::join!(
                axum_server::bind_rustls(addr_http11, config.clone())
                    .handle(handle_http11.clone())
                    .serve(app.clone().into_make_service()),
                graceful_shutdown(handle_http11),
                axum_server::bind_rustls(addr_http2, config)
                    .handle(handle_http2.clone())
                    .serve(app.into_make_service()),
                graceful_shutdown(handle_http2)
            );
            http11_server.unwrap();
            http2_server.unwrap();
        });
}

static REQUEST_COUNT: i32 = 128;
static REQUEST_SIZE: usize = 64;
#[binary_benchmark]
#[bench::http_11_handshake_always(setup = run_benchmark_server(), args = [false, Version::HTTP_11, REQUEST_COUNT, REQUEST_SIZE])]
#[bench::http_11_handshake_once(setup = run_benchmark_server(), args = [true, Version::HTTP_11, REQUEST_COUNT, REQUEST_SIZE])]
#[bench::http_2_handshake_always(setup = run_benchmark_server(), args = [false, Version::HTTP_2, REQUEST_COUNT, REQUEST_SIZE])]
#[bench::http_2_handshake_once(setup = run_benchmark_server(), args = [true, Version::HTTP_2, REQUEST_COUNT, REQUEST_SIZE])]
fn bench_client(
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
            format!("--stream-reuse={}", stream_reuse),
            format!("--http-version={:?}", http_version),
            format!("--request-count={}", request_count),
            format!("--request-size={}", request_size),
        ])
        .build()
}

binary_benchmark_group!(
    name = tls_connector;
    config = BinaryBenchmarkConfig::default()
        .flamegraph(FlamegraphConfig::default())
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

main!(binary_benchmark_groups = tls_connector);
