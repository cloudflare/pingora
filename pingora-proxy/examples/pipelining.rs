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

use pingora_core::server::configuration::Opt;
use pingora_core::server::Server;
use pingora_core::upstreams::peer::HttpPeer;
use pingora_core::Result;
use pingora_proxy::{ProxyHttp, Session};

pub struct PipelinedGateway;

#[async_trait]
impl ProxyHttp for PipelinedGateway {
    type CTX = ();
    fn new_ctx(&self) -> Self::CTX {}

    async fn early_request_filter(
        &self,
        session: &mut Session,
        _ctx: &mut Self::CTX,
    ) -> Result<()> {
        // Opt in once per session; persists across keep-alive reuses.
        session.set_pipelining_enabled(true);
        Ok(())
    }

    async fn upstream_peer(
        &self,
        _session: &mut Session,
        _ctx: &mut Self::CTX,
    ) -> Result<Box<HttpPeer>> {
        let peer = HttpPeer::new(("httpbin.org", 80), false, "httpbin.org".into());
        Ok(Box::new(peer))
    }
}

// RUST_LOG=INFO cargo run --example pipelining
//
// Two pipelined GETs on one connection (expect two `HTTP/1.1 200` lines):
//   printf 'GET /get HTTP/1.1\r\nHost: httpbin.org\r\n\r\nGET /get HTTP/1.1\r\nHost: httpbin.org\r\n\r\n' \
//     | ncat --no-shutdown localhost 6191 \
//     | grep -oE 'HTTP/1.1 [0-9]{3}'

fn main() {
    env_logger::init();

    let opt = Opt::parse_args();
    let mut my_server = Server::new(Some(opt)).unwrap();
    my_server.bootstrap();

    let mut my_proxy =
        pingora_proxy::http_proxy_service(&my_server.configuration, PipelinedGateway);
    my_proxy.add_tcp("0.0.0.0:6191");
    my_server.add_service(my_proxy);

    my_server.run_forever();
}
