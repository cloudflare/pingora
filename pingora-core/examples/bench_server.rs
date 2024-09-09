use clap::Parser;
use hyper::Version;
use pingora_core::listeners::{Listeners, TlsSettings};
use pingora_core::prelude::{Opt, Server};
use pingora_core::services::listening::Service;
use std::env::current_dir;
use std::thread;
use std::time::Duration;

#[allow(dead_code, unused_imports)]
#[path = "../tests/utils/mod.rs"]
mod test_utils;
use test_utils::EchoApp;

#[path = "../benches/utils/mod.rs"]
mod bench_utils;
use bench_utils::{http_version_parser, CERT_PATH, KEY_PATH, TLS_HTTP11_PORT, TLS_HTTP2_PORT};

pub struct BenchServer {
    // Maybe useful in the future
    #[allow(dead_code)]
    pub handle: thread::JoinHandle<()>,
}

#[derive(Parser, Debug)]
struct Args {
    #[clap(long, parse(try_from_str = http_version_parser))]
    http_version: Version,

    #[clap(long, default_value = "1")]
    parallel_acceptors: u16,
}

fn entry_point(opt: Option<Opt>, args: Args) {
    env_logger::init();

    let mut test_server = Server::new(opt).unwrap();
    test_server.bootstrap();

    let mut listeners = Listeners::new();

    match args.http_version {
        Version::HTTP_11 => {
            for i in 0..args.parallel_acceptors {
                let tls_settings =
                    TlsSettings::intermediate(CERT_PATH.as_str(), KEY_PATH.as_str()).unwrap();
                listeners.add_tls_with_settings(
                    format! {"0.0.0.0:{}", TLS_HTTP11_PORT + i}.as_str(),
                    None,
                    tls_settings,
                );
            }
        }
        Version::HTTP_2 => {
            for i in 0..args.parallel_acceptors {
                let mut tls_settings =
                    TlsSettings::intermediate(CERT_PATH.as_str(), KEY_PATH.as_str()).unwrap();
                tls_settings.enable_h2();

                listeners.add_tls_with_settings(
                    format! {"0.0.0.0:{}", TLS_HTTP2_PORT + i}.as_str(),
                    None,
                    tls_settings,
                );
            }
        }
        _ => {
            panic!("HTTP version not supported.")
        }
    };

    let echo_service_http =
        Service::with_listeners("Echo Service HTTP".to_string(), listeners, EchoApp);

    test_server.add_service(echo_service_http);
    test_server.run_forever();
}

impl BenchServer {
    pub fn start() -> Self {
        println!("WorkingDir: {:?}", current_dir().unwrap());

        let opts = Opt::parse_from::<Vec<String>, String>(vec![
            "pingora".into(),
            "-c".into(),
            "benches/utils/pingora_conf.yaml".into(),
        ]);
        println!("{:?}", opts);

        let args = Args::parse();
        println!("{:?}", args);

        let server_handle = thread::spawn(|| {
            entry_point(Some(opts), args);
        });
        // wait until the server is up
        thread::sleep(Duration::from_secs(2));
        BenchServer {
            handle: server_handle,
        }
    }
}

fn main() {
    let _server = BenchServer::start();
    thread::sleep(Duration::from_secs(10));
}
