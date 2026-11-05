// Copyright 2026 Cloudflare, Inc.
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

use async_trait::async_trait;
use futures::FutureExt;
use log::info;
use pingora_core::services::background::background_service;
use std::{sync::Arc, time::Duration};

use pingora_core::server::configuration::Opt;
use pingora_core::server::Server;
use pingora_core::upstreams::peer::HttpPeer;
use pingora_core::Result;
use pingora_load_balancing::{
    discovery, health_check, selection::RoundRobin, Backend, Backends, LoadBalancer,
};
use pingora_proxy::{ProxyHttp, Session};

pub struct LB(Arc<LoadBalancer<RoundRobin>>);

#[derive(Clone)]
struct BackendName(&'static str);

#[async_trait]
impl ProxyHttp for LB {
    type CTX = ();
    fn new_ctx(&self) -> Self::CTX {}

    async fn upstream_peer(&self, _session: &mut Session, _ctx: &mut ()) -> Result<Box<HttpPeer>> {
        let upstream = self
            .0
            .select(b"", 256) // hash doesn't matter
            .unwrap();

        // Extensions survive discovery and selection, so we can use backend metadata here.
        let name = upstream.ext.get::<BackendName>().unwrap();
        info!("upstream peer is: {} ({:?})", name.0, upstream);

        let peer = Box::new(HttpPeer::new(upstream, true, "one.one.one.one".to_string()));
        Ok(peer)
    }

    async fn upstream_request_filter(
        &self,
        _session: &mut Session,
        upstream_request: &mut pingora_http::RequestHeader,
        _ctx: &mut Self::CTX,
    ) -> Result<()> {
        upstream_request
            .insert_header("Host", "one.one.one.one")
            .unwrap();
        Ok(())
    }
}

fn upstreams() -> LoadBalancer<RoundRobin> {
    // Construct backends explicitly to attach typed metadata before static discovery.
    // Keep equal weights; 127.0.0.1:343 is just a bad server.
    let backends = [
        ("1.1.1.1:443", "primary"),
        ("1.0.0.1:443", "secondary"),
        ("127.0.0.1:343", "bad server"),
    ]
    .into_iter()
    .map(|(addr, name)| {
        let mut backend = Backend::new_with_weight(addr, 1).unwrap();
        backend.ext.insert(BackendName(name));
        backend
    })
    .collect();
    let upstreams = LoadBalancer::from_backends(Backends::new(discovery::Static::new(backends)));

    // Unlike try_from_iter, from_backends needs an initial discovery update.
    // Static discovery completes immediately and does not need an async runtime.
    upstreams
        .update()
        .now_or_never()
        .expect("static discovery should not block")
        .unwrap();

    upstreams
}

// RUST_LOG=INFO cargo run -p pingora-proxy --example load_balancer --features openssl
fn main() {
    env_logger::init();

    // read command line arguments
    let opt = Opt::parse_args();
    let mut my_server = Server::new(Some(opt)).unwrap();
    my_server.bootstrap();

    let mut upstreams = upstreams();
    // Background checks stop selecting the bad server once it is marked unhealthy.
    let hc = health_check::TcpHealthCheck::new();
    upstreams.set_health_check(hc);
    upstreams.health_check_frequency = Some(Duration::from_secs(1));

    let background = background_service("health check", upstreams);

    let upstreams = background.task();

    let mut lb = pingora_proxy::http_proxy_service(&my_server.configuration, LB(upstreams));
    lb.add_tcp("0.0.0.0:6188");

    let cert_path = format!("{}/tests/keys/server.crt", env!("CARGO_MANIFEST_DIR"));
    let key_path = format!("{}/tests/keys/key.pem", env!("CARGO_MANIFEST_DIR"));

    let mut tls_settings =
        pingora_core::listeners::tls::TlsSettings::intermediate(&cert_path, &key_path).unwrap();
    tls_settings.enable_h2();
    lb.add_tls_with_settings("0.0.0.0:6189", None, tls_settings);

    my_server.add_service(lb);
    my_server.add_service(background);
    my_server.run_forever();
}

#[cfg(test)]
mod tests {
    use super::*;

    struct ExampleHealthCheck;

    #[async_trait]
    impl health_check::HealthCheck for ExampleHealthCheck {
        async fn check(&self, backend: &Backend) -> Result<()> {
            if backend.ext.get::<BackendName>().unwrap().0 == "bad server" {
                Err(pingora_core::Error::new(
                    pingora_core::ErrorType::ConnectRefused,
                ))
            } else {
                Ok(())
            }
        }

        fn health_threshold(&self, _success: bool) -> usize {
            1
        }
    }

    #[tokio::test]
    async fn extensions_survive_initialization_selection_and_health_checks() {
        let mut upstreams = upstreams();
        let mut selected = std::collections::BTreeMap::new();
        for _ in 0..3 {
            let backend = upstreams.select(b"", 256).unwrap();
            assert_eq!(backend.weight, 1);
            selected.insert(
                backend.addr.to_string(),
                backend.ext.get::<BackendName>().unwrap().0,
            );
        }
        assert_eq!(
            selected,
            std::collections::BTreeMap::from([
                ("1.1.1.1:443".to_string(), "primary"),
                ("1.0.0.1:443".to_string(), "secondary"),
                ("127.0.0.1:343".to_string(), "bad server"),
            ])
        );

        // Exercise health filtering without depending on public network endpoints.
        upstreams.set_health_check(Box::new(ExampleHealthCheck));
        let background = background_service("health check", upstreams);
        let shared = background.task();
        shared.update().await.unwrap();
        shared.backends().run_health_check(false).await;
        let bad = Backend::new("127.0.0.1:343").unwrap();
        assert!(!shared.backends().ready(&bad));
        for _ in 0..6 {
            let backend = shared.select(b"", 256).unwrap();
            assert_ne!(backend.addr, bad.addr);
            assert_ne!(backend.ext.get::<BackendName>().unwrap().0, "bad server");
        }
    }
}
