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

//! Example: early request body buffering for routing and body mutation.
//!
//! Demonstrates two patterns enabled by `request_body_buffer_limit()`:
//!
//! 1. **Peek**: read the buffered body with `get_buffered_body()` to make
//!    routing decisions without modifying the body.
//!
//! 2. **Mutate**: read the buffered body, transform it, and replace it with
//!    `set_buffered_body()` so the upstream receives the modified version.
//!
//! Uses httpbin.org as the upstream — its `/post` endpoint echoes back the
//! request body, so you can verify mutations in the response.

use async_trait::async_trait;
use bytes::Bytes;
use log::info;

use pingora_core::server::configuration::Opt;
use pingora_core::server::Server;
use pingora_core::upstreams::peer::HttpPeer;
use pingora_core::Result;
use pingora_proxy::{ProxyHttp, Session};

pub struct MyProxy;

pub struct MyCtx {
    route_beta: bool,
}

#[async_trait]
impl ProxyHttp for MyProxy {
    type CTX = MyCtx;
    fn new_ctx(&self) -> Self::CTX {
        MyCtx { route_beta: false }
    }

    /// Opt in to body buffering for POST requests up to 4KB.
    fn request_body_buffer_limit(&self, session: &Session, _ctx: &Self::CTX) -> Option<usize> {
        if session.req_header().method == http::Method::POST {
            Some(4096)
        } else {
            None
        }
    }

    async fn request_filter(&self, session: &mut Session, ctx: &mut Self::CTX) -> Result<bool> {
        // By the time request_filter runs, the body is already buffered.
        if let Some(body) = session.get_buffered_body() {
            let text = std::str::from_utf8(body).unwrap_or("");

            // Peek: inspect body content for routing decisions.
            ctx.route_beta = text.contains("beta");
            info!("peek: route_beta={}", ctx.route_beta);

            // Mutate: wrap the original body in an envelope before forwarding.
            let wrapped = format!(r#"{{"envelope":true,"original":{text}}}"#);
            info!("mutate: {wrapped}");
            session.set_buffered_body(Some(Bytes::from(wrapped.clone())));

            // Update Content-Length to match the new body size.
            session
                .req_header_mut()
                .insert_header(http::header::CONTENT_LENGTH, wrapped.len().to_string())?;
        }
        Ok(false)
    }

    async fn upstream_peer(
        &self,
        _session: &mut Session,
        _ctx: &mut Self::CTX,
    ) -> Result<Box<HttpPeer>> {
        // httpbin.org echoes the request body back in its JSON response,
        // so we can verify the mutation in the curl output.
        let peer = Box::new(HttpPeer::new(
            ("httpbin.org", 443),
            true,
            "httpbin.org".to_string(),
        ));
        Ok(peer)
    }
}

// RUST_LOG=INFO cargo run --features openssl --example body_routing
//
// Peek + mutate — body is inspected for routing then wrapped in an envelope:
//   curl -X POST 127.0.0.1:6193/post -H "Host: httpbin.org" -H "Content-Type: application/json" -d '{"route": "beta"}'
//   curl -X POST 127.0.0.1:6193/post -H "Host: httpbin.org" -H "Content-Type: application/json" -d '{"route": "default"}'
//
// No buffering — GET requests pass through unchanged:
//   curl 127.0.0.1:6193/get -H "Host: httpbin.org"
fn main() {
    env_logger::init();

    let opt = Opt::parse_args();
    let mut my_server = Server::new(Some(opt)).unwrap();
    my_server.bootstrap();

    let mut my_proxy = pingora_proxy::http_proxy_service(&my_server.configuration, MyProxy);
    my_proxy.add_tcp("0.0.0.0:6193");

    my_server.add_service(my_proxy);
    my_server.run_forever();
}
