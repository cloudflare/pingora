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

use async_trait::async_trait;
use clap::Parser;
use log::info;
use std::sync::Mutex;

use pingora_core::server::configuration::Opt;
use pingora_core::server::Server;
use pingora_core::upstreams::peer::HttpPeer;
use pingora_core::Result;
use pingora_proxy::{ProxyHttp, Session};

// global counter
static REQ_COUNTER: Mutex<usize> = Mutex::new(0);

pub struct MyProxy {
    // counter for the service
    beta_counter: Mutex<usize>, // AtomicUsize works too
}

pub struct MyCtx {
    beta_user: bool,
}

fn check_beta_user(req: &pingora_http::RequestHeader) -> bool {
    // some simple logic to check if user is beta
    req.headers.get("beta-flag").is_some()
}

#[async_trait]
impl ProxyHttp for MyProxy {
    type CTX = MyCtx;
    fn new_ctx(&self) -> Self::CTX {
        MyCtx { beta_user: false }
    }

    async fn request_filter(&self, session: &mut Session, ctx: &mut Self::CTX) -> Result<bool> {
        ctx.beta_user = check_beta_user(session.req_header());
        Ok(false)
    }

    async fn upstream_peer(
        &self,
        _session: &mut Session,
        ctx: &mut Self::CTX,
    ) -> Result<Box<HttpPeer>> {
        let mut req_counter = REQ_COUNTER.lock().unwrap();
        *req_counter += 1;

        let addr = if ctx.beta_user {
            let mut beta_count = self.beta_counter.lock().unwrap();
            *beta_count += 1;
            info!("I'm a beta user #{beta_count}");
            ("1.0.0.1", 443)
        } else {
            info!("I'm an user #{req_counter}");
            ("1.1.1.1", 443)
        };

        let peer = Box::new(HttpPeer::new(addr, true, "one.one.one.one".to_string()));
        Ok(peer)
    }
}

// RUST_LOG=INFO cargo run --example ctx
// curl 127.0.0.1:6190 -H "Host: one.one.one.one"
// curl 127.0.0.1:6190 -H "Host: one.one.one.one" -H "beta-flag: 1"
fn main() {
    env_logger::init();

    // read command line arguments
    let opt = Opt::parse();
    let mut my_server = Server::new(Some(opt)).unwrap();
    my_server.bootstrap();

    let mut my_proxy = pingora_proxy::http_proxy_service(
        &my_server.configuration,
        MyProxy {
            beta_counter: Mutex::new(0),
        },
    );
    my_proxy.add_tcp("0.0.0.0:6190");

    my_server.add_service(my_proxy);
    my_server.run_forever();
}
