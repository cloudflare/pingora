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
use bytes::Bytes;
use clap::Parser;
use serde::{Deserialize, Serialize};
use std::net::ToSocketAddrs;

use pingora_core::server::configuration::Opt;
use pingora_core::server::Server;
use pingora_core::upstreams::peer::HttpPeer;
use pingora_core::Result;
use pingora_http::ResponseHeader;
use pingora_proxy::{ProxyHttp, Session};

const HOST: &str = "ip.jsontest.com";

#[derive(Serialize, Deserialize)]
pub struct Resp {
    ip: String,
}

pub struct Json2Yaml {
    addr: std::net::SocketAddr,
}

pub struct MyCtx {
    buffer: Vec<u8>,
}

#[async_trait]
impl ProxyHttp for Json2Yaml {
    type CTX = MyCtx;
    fn new_ctx(&self) -> Self::CTX {
        MyCtx { buffer: vec![] }
    }

    async fn upstream_peer(
        &self,
        _session: &mut Session,
        _ctx: &mut Self::CTX,
    ) -> Result<Box<HttpPeer>> {
        let peer = Box::new(HttpPeer::new(self.addr, false, HOST.to_owned()));
        Ok(peer)
    }

    async fn upstream_request_filter(
        &self,
        _session: &mut Session,
        upstream_request: &mut pingora_http::RequestHeader,
        _ctx: &mut Self::CTX,
    ) -> Result<()> {
        upstream_request
            .insert_header("Host", HOST.to_owned())
            .unwrap();
        Ok(())
    }

    async fn response_filter(
        &self,
        _session: &mut Session,
        upstream_response: &mut ResponseHeader,
        _ctx: &mut Self::CTX,
    ) -> Result<()>
    where
        Self::CTX: Send + Sync,
    {
        // Remove content-length because the size of the new body is unknown
        upstream_response.remove_header("Content-Length");
        upstream_response
            .insert_header("Transfer-Encoding", "Chunked")
            .unwrap();
        Ok(())
    }

    fn response_body_filter(
        &self,
        _session: &mut Session,
        body: &mut Option<Bytes>,
        end_of_stream: bool,
        ctx: &mut Self::CTX,
    ) -> Result<Option<std::time::Duration>>
    where
        Self::CTX: Send + Sync,
    {
        // buffer the data
        if let Some(b) = body {
            ctx.buffer.extend(&b[..]);
            // drop the body
            b.clear();
        }
        if end_of_stream {
            // This is the last chunk, we can process the data now
            let json_body: Resp = serde_json::de::from_slice(&ctx.buffer).unwrap();
            let yaml_body = serde_yaml::to_string(&json_body).unwrap();
            *body = Some(Bytes::copy_from_slice(yaml_body.as_bytes()));
        }

        Ok(None)
    }
}

// RUST_LOG=INFO cargo run --example modify_response
// curl 127.0.0.1:6191
fn main() {
    env_logger::init();

    let opt = Opt::parse();
    let mut my_server = Server::new(Some(opt)).unwrap();
    my_server.bootstrap();

    let mut my_proxy = pingora_proxy::http_proxy_service(
        &my_server.configuration,
        Json2Yaml {
            // hardcode the IP of ip.jsontest.com for now
            addr: ("142.251.2.121", 80)
                .to_socket_addrs()
                .unwrap()
                .next()
                .unwrap(),
        },
    );

    my_proxy.add_tcp("127.0.0.1:6191");

    my_server.add_service(my_proxy);
    my_server.run_forever();
}
