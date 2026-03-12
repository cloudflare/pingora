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
use clap::Parser;
use log::info;
use pingora_core::listeners::ConnectionFilter;
use pingora_core::prelude::Opt;
use pingora_core::server::Server;
use pingora_core::upstreams::peer::HttpPeer;
use pingora_core::Result;
use pingora_proxy::{ProxyHttp, Session};
use std::sync::Arc;

/// This example demonstrates how to implement a connection filter
pub struct MyProxy;

#[async_trait]
impl ProxyHttp for MyProxy {
    type CTX = ();

    fn new_ctx(&self) -> Self::CTX {}

    async fn upstream_peer(
        &self,
        _session: &mut Session,
        _ctx: &mut Self::CTX,
    ) -> Result<Box<HttpPeer>> {
        // Forward to httpbin.org for testing
        let peer = HttpPeer::new(("httpbin.org", 80), false, "httpbin.org".into());
        Ok(Box::new(peer))
    }
}

/// Connection filter that blocks ALL connections (for testing)
#[derive(Debug, Clone)]
struct BlockAllFilter;

#[async_trait]
impl ConnectionFilter for BlockAllFilter {
    async fn should_accept(&self, addr: &std::net::SocketAddr) -> bool {
        info!("BLOCKING connection from {} (BlockAllFilter active)", addr);
        false
    }
}

// RUST_LOG=INFO cargo run --example connection_filter

fn main() {
    env_logger::init();

    // read command line arguments
    let opt = Opt::parse();
    let mut my_server = Server::new(Some(opt)).unwrap();
    my_server.bootstrap();

    let mut my_proxy = pingora_proxy::http_proxy_service(&my_server.configuration, MyProxy);

    // Create a filter that blocks ALL connections
    let filter = Arc::new(BlockAllFilter);

    info!("Setting BlockAllFilter on proxy service");
    my_proxy.set_connection_filter(filter.clone());

    info!("Adding TCP endpoints AFTER setting filter");
    my_proxy.add_tcp("0.0.0.0:6195");
    my_proxy.add_tcp("0.0.0.0:6196");

    info!("====================================");
    info!("Server starting with BlockAllFilter");
    info!("This filter blocks ALL connections!");
    info!("====================================");
    info!("");
    info!("Test with:");
    info!("  curl http://localhost:6195/get");
    info!("  curl http://localhost:6196/get");
    info!("");
    info!("ALL requests should be blocked!");
    info!("You should see 'BLOCKING connection' in the logs");
    info!("and curl should fail with 'Connection refused' or hang");
    info!("");

    my_server.add_service(my_proxy);
    my_server.run_forever();
}
