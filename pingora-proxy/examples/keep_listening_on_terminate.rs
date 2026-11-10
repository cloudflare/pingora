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

//! On SIGTERM the proxy stops accepting so the load balancer moves traffic away, while the
//! readiness and metrics endpoints stay reachable for the whole grace period.
//!
//! ```bash
//! cargo run --example keep_listening_on_terminate --features keep_listening_on_terminate
//! curl -i 127.0.0.1:6193   # readiness: 200 ready
//! pkill -TERM -f keep_listening_on_terminate
//! curl -i 127.0.0.1:6191   # proxy: connection refused
//! curl -i 127.0.0.1:6193   # readiness: 503 draining
//! curl 127.0.0.1:6192      # metrics: still served
//! ```

use async_trait::async_trait;
use http::{Response, StatusCode};
use pingora_core::apps::http_app::{HttpServer, ServeHttp};
use pingora_core::protocols::http::ServerSession;
use pingora_core::server::configuration::Opt;
use pingora_core::server::{Server, ShutdownWatch};
use pingora_core::services::background::{background_service, BackgroundService};
use pingora_core::services::listening::Service;
use pingora_core::upstreams::peer::HttpPeer;
use pingora_core::Result;
use pingora_proxy::{ProxyHttp, Session};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

struct Proxy;

#[async_trait]
impl ProxyHttp for Proxy {
    type CTX = ();
    fn new_ctx(&self) -> Self::CTX {}

    async fn upstream_peer(&self, _session: &mut Session, _ctx: &mut ()) -> Result<Box<HttpPeer>> {
        Ok(Box::new(HttpPeer::new(
            ("1.1.1.1", 80),
            false,
            String::new(),
        )?))
    }
}

struct Readiness {
    draining: Arc<AtomicBool>,
}

#[async_trait]
impl ServeHttp for Readiness {
    async fn response(&self, _session: &mut ServerSession) -> Response<Vec<u8>> {
        let (status, body) = if self.draining.load(Ordering::Relaxed) {
            (StatusCode::SERVICE_UNAVAILABLE, "draining\n")
        } else {
            (StatusCode::OK, "ready\n")
        };
        Response::builder()
            .status(status)
            .header(http::header::CONTENT_LENGTH, body.len())
            .body(body.into())
            .unwrap()
    }
}

/// Marks the server as draining once shutdown starts. A kept listener's app is never cleaned up
/// on terminate, so draining has to be observed here.
struct DrainWatch {
    draining: Arc<AtomicBool>,
}

#[async_trait]
impl BackgroundService for DrainWatch {
    async fn start(&self, mut shutdown: ShutdownWatch) {
        shutdown.wait_for(|&shutting_down| shutting_down).await.ok();
        self.draining.store(true, Ordering::Relaxed);
    }
}

fn main() {
    env_logger::init();

    let mut server = Server::new(Some(Opt::parse_args())).unwrap();
    server.bootstrap();

    let mut proxy = pingora_proxy::http_proxy_service(&server.configuration, Proxy);
    proxy.add_tcp("127.0.0.1:6191");
    server.add_service(proxy);

    let mut metrics = pingora_prometheus::prometheus_http_service();
    metrics.add_tcp("127.0.0.1:6192");
    metrics.keep_listening_on_terminate(&server);
    server.add_service(metrics);

    let draining = Arc::new(AtomicBool::new(false));
    server.add_service(background_service(
        "drain watch",
        DrainWatch {
            draining: draining.clone(),
        },
    ));
    let mut readiness = Service::new(
        "readiness".to_string(),
        HttpServer::new_app(Readiness { draining }),
    );
    readiness.add_tcp("127.0.0.1:6193");
    readiness.keep_listening_on_terminate(&server);
    server.add_service(readiness);

    server.run_forever();
}
