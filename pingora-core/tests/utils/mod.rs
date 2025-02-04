// Copyright 2025 Cloudflare, Inc.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use once_cell::sync::Lazy;
use std::{thread, time};

use clap::Parser;
use pingora_core::listeners::Listeners;
use pingora_core::server::configuration::Opt;
use pingora_core::server::Server;
use pingora_core::services::listening::Service;

use async_trait::async_trait;
use bytes::{BufMut, BytesMut};
use http::{Response, StatusCode};
use pingora_timeout::timeout;
use std::time::Duration;

use pingora_core::apps::http_app::ServeHttp;
use pingora_core::protocols::http::ServerSession;
use pingora_core::protocols::l4::quic::{QuicHttp3Configs, MAX_IPV6_BUF_SIZE};

#[derive(Clone)]
pub struct EchoApp;

#[async_trait]
impl ServeHttp for EchoApp {
    async fn response(&self, http_stream: &mut ServerSession) -> Response<Vec<u8>> {
        // read timeout of 2s
        let read_timeout = 2000;
        let body_future = async {
            let mut body = BytesMut::with_capacity(MAX_IPV6_BUF_SIZE);
            while let Ok(b) = http_stream.read_request_body().await {
                match b {
                    None => break, // finished reading request
                    Some(b) => body.put(b),
                }
            }
            if body.is_empty() {
                body.put("no body!".as_bytes());
            }
            body.freeze()
        };

        let body = match timeout(Duration::from_millis(read_timeout), body_future).await {
            Ok(res) => res,
            Err(_) => {
                panic!("Timed out after {:?}ms", read_timeout);
            }
        };

        Response::builder()
            .status(StatusCode::OK)
            .header(http::header::CONTENT_TYPE, "text/html")
            .header(http::header::CONTENT_LENGTH, body.len())
            .body(body.to_vec())
            .unwrap()
    }
}

pub struct MyServer {
    // Maybe useful in the future
    #[allow(dead_code)]
    pub handle: thread::JoinHandle<()>,
}

fn entry_point(opt: Option<Opt>) {
    env_logger::builder()
        .format_timestamp(Some(env_logger::TimestampPrecision::Nanos))
        .init();

    let cert_path = format!("{}/tests/keys/server.crt", env!("CARGO_MANIFEST_DIR"));
    let key_path = format!("{}/tests/keys/key.pem", env!("CARGO_MANIFEST_DIR"));

    let mut my_server = Server::new(opt).unwrap();
    my_server.bootstrap();

    let mut listeners = Listeners::tcp("0.0.0.0:6145");
    #[cfg(unix)]
    listeners.add_uds("/tmp/echo.sock", None);

    let mut tls_settings =
        pingora_core::listeners::tls::TlsSettings::intermediate(&cert_path, &key_path).unwrap();
    tls_settings.enable_h2();
    listeners.add_tls_with_settings("0.0.0.0:6146", None, tls_settings);

    let configs = QuicHttp3Configs::from_cert_key_paths(&cert_path, &key_path).unwrap();
    listeners.add_quic("0.0.0.0:6147", configs);

    let mut echo_service_http =
        Service::with_listeners("Echo Service HTTP".to_string(), listeners, EchoApp);
    echo_service_http.threads = Some(1);

    my_server.add_service(echo_service_http);
    my_server.run_forever();
}

impl MyServer {
    pub fn start() -> Self {
        let opts: Vec<String> = vec![
            "pingora".into(),
            "-c".into(),
            "tests/pingora_conf.yaml".into(),
        ];
        let server_handle = thread::spawn(|| {
            entry_point(Some(Opt::parse_from(opts)));
        });
        // wait until the server is up
        thread::sleep(time::Duration::from_secs(2));
        MyServer {
            handle: server_handle,
        }
    }
}

pub static TEST_SERVER: Lazy<MyServer> = Lazy::new(MyServer::start);

pub fn init() {
    let _ = *TEST_SERVER;
}
