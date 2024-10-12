// Copyright 2024 Cloudflare, Inc.
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

#[global_allocator]
static GLOBAL: jemallocator::Jemalloc = jemallocator::Jemalloc;

use pingora::listeners::tls::TlsSettings;
use pingora::protocols::TcpKeepalive;
use pingora::server::configuration::Opt;
use pingora::server::{Server, ShutdownWatch};
use pingora::services::background::{background_service, BackgroundService};
use pingora::services::{listening::Service as ListeningService, Service};

use async_trait::async_trait;
use clap::Parser;
use tokio::time::interval;

use std::time::Duration;

mod app;
mod service;

pub struct ExampleBackgroundService;
#[async_trait]
impl BackgroundService for ExampleBackgroundService {
    async fn start(&self, mut shutdown: ShutdownWatch) {
        let mut period = interval(Duration::from_secs(1));
        loop {
            tokio::select! {
                _ = shutdown.changed() => {
                    // shutdown
                    break;
                }
                _ = period.tick() => {
                    // do some work
                    // ...
                }
            }
        }
    }
}
#[cfg(feature = "openssl_derived")]
mod boringssl_openssl {
    use super::*;
    use pingora::tls::pkey::{PKey, Private};
    use pingora::tls::x509::X509;

    pub(super) struct DynamicCert {
        cert: X509,
        key: PKey<Private>,
    }

    impl DynamicCert {
        pub(super) fn new(cert: &str, key: &str) -> Box<Self> {
            let cert_bytes = std::fs::read(cert).unwrap();
            let cert = X509::from_pem(&cert_bytes).unwrap();

            let key_bytes = std::fs::read(key).unwrap();
            let key = PKey::private_key_from_pem(&key_bytes).unwrap();
            Box::new(DynamicCert { cert, key })
        }
    }

    #[async_trait]
    impl pingora::listeners::TlsAccept for DynamicCert {
        async fn certificate_callback(&self, ssl: &mut pingora::tls::ssl::SslRef) {
            use pingora::tls::ext;
            ext::ssl_use_certificate(ssl, &self.cert).unwrap();
            ext::ssl_use_private_key(ssl, &self.key).unwrap();
        }
    }
}

const USAGE: &str = r#"
Usage
port 6142: TCP echo server
nc 127.0.0.1 6142

port 6143: TLS echo server
openssl s_client -connect 127.0.0.1:6143

port 6145: Http echo server
curl http://127.0.0.1:6145 -v -d 'hello'

port 6148: Https echo server
curl https://127.0.0.1:6148 -vk -d 'hello'

port 6141: TCP proxy
curl http://127.0.0.1:6141 -v -H 'host: 1.1.1.1'

port 6144: TLS proxy
curl https://127.0.0.1:6144 -vk -H 'host: one.one.one.one' -o /dev/null

port 6150: metrics endpoint
curl http://127.0.0.1:6150
"#;

pub fn main() {
    env_logger::init();

    print!("{USAGE}");

    let opt = Some(Opt::parse());
    let mut my_server = Server::new(opt).unwrap();
    my_server.bootstrap();

    let cert_path = format!("{}/tests/keys/server.crt", env!("CARGO_MANIFEST_DIR"));
    let key_path = format!("{}/tests/keys/key.pem", env!("CARGO_MANIFEST_DIR"));

    let mut echo_service = service::echo::echo_service();
    echo_service.add_tcp("127.0.0.1:6142");
    echo_service
        .add_tls("0.0.0.0:6143", &cert_path, &key_path)
        .unwrap();

    let mut echo_service_http = service::echo::echo_service_http();

    let mut options = pingora::listeners::TcpSocketOptions::default();
    options.tcp_fastopen = Some(10);
    options.tcp_keepalive = Some(TcpKeepalive {
        idle: Duration::from_secs(60),
        interval: Duration::from_secs(5),
        count: 5,
    });

    echo_service_http.add_tcp_with_settings("0.0.0.0:6145", options);
    echo_service_http.add_uds("/tmp/echo.sock", None);

    let mut tls_settings;

    // NOTE: dynamic certificate callback is only supported with BoringSSL/OpenSSL
    #[cfg(feature = "openssl_derived")]
    {
        use std::ops::DerefMut;

        let dynamic_cert = boringssl_openssl::DynamicCert::new(&cert_path, &key_path);
        tls_settings = TlsSettings::with_callbacks(dynamic_cert).unwrap();
        // by default intermediate supports both TLS 1.2 and 1.3. We force to tls 1.2 just for the demo

        tls_settings
            .deref_mut()
            .deref_mut()
            .set_max_proto_version(Some(pingora::tls::ssl::SslVersion::TLS1_2))
            .unwrap();
    }
    #[cfg(feature = "rustls")]
    {
        tls_settings = TlsSettings::intermediate(&cert_path, &key_path).unwrap();
    }
    #[cfg(not(feature = "any_tls"))]
    {
        tls_settings = TlsSettings;
    }

    tls_settings.enable_h2();
    echo_service_http.add_tls_with_settings("0.0.0.0:6148", None, tls_settings);

    let proxy_service = service::proxy::proxy_service(
        "0.0.0.0:6141", // listen
        "1.1.1.1:80",   // proxy to
    );

    let proxy_service_ssl = service::proxy::proxy_service_tls(
        "0.0.0.0:6144",    // listen
        "1.1.1.1:443",     // proxy to
        "one.one.one.one", // SNI
        &cert_path,
        &key_path,
    );

    let mut prometheus_service_http = ListeningService::prometheus_http_service();
    prometheus_service_http.add_tcp("127.0.0.1:6150");

    let background_service = background_service("example", ExampleBackgroundService {});

    let services: Vec<Box<dyn Service>> = vec![
        Box::new(echo_service),
        Box::new(echo_service_http),
        Box::new(proxy_service),
        Box::new(proxy_service_ssl),
        Box::new(prometheus_service_http),
        Box::new(background_service),
    ];
    my_server.add_services(services);
    my_server.run_forever();
}
