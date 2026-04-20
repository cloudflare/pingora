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
use log::info;
use pingora_core::server::configuration::Opt;
use pingora_core::server::Server;
use pingora_core::services::background::background_service;
use pingora_core::upstreams::peer::HttpPeer;
use pingora_core::Result;
use pingora_load_balancing::{
    health_check,
    selection::{LeastConnLease, LeastConnections},
    LoadBalancer,
};
use pingora_proxy::{ProxyHttp, Session};
use std::sync::Arc;
use std::time::Duration;

// CTX object (with a lease) will be dropped at the end of the request
struct Ctx {
    lease: Option<LeastConnLease>,
}

struct LB(Arc<LoadBalancer<LeastConnections>>);

#[async_trait]
impl ProxyHttp for LB {
    type CTX = Ctx;

    fn new_ctx(&self) -> Self::CTX {
        Ctx { lease: None }
    }

    async fn upstream_peer(
        &self,
        _session: &mut Session,
        ctx: &mut Self::CTX,
    ) -> Result<Box<HttpPeer>> {
        let (upstream, lease) = self.0.select_with_lease(b"", 256).unwrap();

        // we need to store this lease until the end of the request
        ctx.lease = Some(lease);

        info!("upstream peer is: {:?}", upstream);

        Ok(Box::new(HttpPeer::new(
            upstream,
            true,
            "one.one.one.one".to_string(),
        )))
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

// RUST_LOG=INFO cargo run --example least_connections
fn main() {
    env_logger::init();

    let opt = Opt::parse_args();
    let mut my_server = Server::new(Some(opt)).unwrap();
    my_server.bootstrap();

    let mut upstreams = LoadBalancer::try_from_iter(["1.1.1.1:443", "1.0.0.1:443"]).unwrap();

    let hc = health_check::TcpHealthCheck::new();
    upstreams.set_health_check(hc);
    upstreams.health_check_frequency = Some(Duration::from_secs(1));

    let background = background_service("least connections health check", upstreams);
    let upstreams = background.task();

    let mut lb = pingora_proxy::http_proxy_service(&my_server.configuration, LB(upstreams));
    lb.add_tcp("0.0.0.0:6190");

    let cert_path = format!("{}/tests/keys/server.crt", env!("CARGO_MANIFEST_DIR"));
    let key_path = format!("{}/tests/keys/key.pem", env!("CARGO_MANIFEST_DIR"));
    let mut tls_settings =
        pingora_core::listeners::tls::TlsSettings::intermediate(&cert_path, &key_path).unwrap();
    tls_settings.enable_h2();
    lb.add_tls_with_settings("0.0.0.0:6191", None, tls_settings);

    my_server.add_service(lb);
    my_server.add_service(background);
    my_server.run_forever();
}
