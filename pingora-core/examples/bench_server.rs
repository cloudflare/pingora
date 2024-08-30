use clap::Parser;
use pingora_core::listeners::Listeners;
use pingora_core::prelude::{Opt, Server};
use pingora_core::services::listening::Service;
use std::env::current_dir;
use std::thread;
use std::time::Duration;

#[allow(dead_code, unused_imports)]
#[path = "../tests/utils/mod.rs"]
mod test_utils;
use crate::test_utils::EchoApp;

#[allow(dead_code, unused_imports)]
#[path = "../benches/utils/mod.rs"]
mod bench_utils;
use bench_utils::{CERT_PATH, KEY_PATH, TLS_HTTP11_PORT, TLS_HTTP2_PORT};

pub struct BenchServer {
    // Maybe useful in the future
    #[allow(dead_code)]
    pub handle: thread::JoinHandle<()>,
}

fn entry_point(opt: Option<Opt>) {
    env_logger::init();

    let mut test_server = Server::new(opt).unwrap();
    test_server.bootstrap();

    let mut listeners = Listeners::new();

    let tls_settings_h1 =
        pingora_core::listeners::TlsSettings::intermediate(CERT_PATH.as_str(), KEY_PATH.as_str())
            .unwrap();
    let mut tls_settings_h2 =
        pingora_core::listeners::TlsSettings::intermediate(CERT_PATH.as_str(), KEY_PATH.as_str())
            .unwrap();
    tls_settings_h2.enable_h2();

    listeners.add_tls_with_settings(
        format! {"0.0.0.0:{}", TLS_HTTP11_PORT}.as_str(),
        None,
        tls_settings_h1,
    );
    listeners.add_tls_with_settings(
        format! {"0.0.0.0:{}", TLS_HTTP2_PORT}.as_str(),
        None,
        tls_settings_h2,
    );

    let echo_service_http =
        Service::with_listeners("Echo Service HTTP".to_string(), listeners, EchoApp);

    test_server.add_service(echo_service_http);
    test_server.run_forever();
}

impl BenchServer {
    pub fn start() -> Self {
        println!("{:?}", current_dir().unwrap());
        let opts: Vec<String> = vec![
            "pingora".into(),
            "-c".into(),
            "benches/utils/pingora_conf.yaml".into(),
        ];
        println!("{:?}", opts);

        let server_handle = thread::spawn(|| {
            entry_point(Some(Opt::parse_from(opts)));
        });
        // wait until the server is up
        thread::sleep(Duration::from_secs(2));
        BenchServer {
            handle: server_handle,
        }
    }
}

fn main() {
    println!("bench_server: starting.");
    let _server = BenchServer::start();
    thread::sleep(Duration::from_secs(10));
    println!("bench_server: finished.");
}
