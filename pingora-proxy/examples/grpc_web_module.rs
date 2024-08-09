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

use pingora_core::server::Server;
use pingora_core::upstreams::peer::HttpPeer;
use pingora_core::Result;
use pingora_core::{
    modules::http::{
        grpc_web::{GrpcWeb, GrpcWebBridge},
        HttpModules,
    },
    prelude::Opt,
};
use pingora_proxy::{ProxyHttp, Session};

/// This example shows how to use the gRPC-web bridge module

pub struct GrpcWebBridgeProxy;

#[async_trait]
impl ProxyHttp for GrpcWebBridgeProxy {
    type CTX = ();
    fn new_ctx(&self) -> Self::CTX {}

    fn init_downstream_modules(&self, modules: &mut HttpModules) {
        // Add the gRPC web module
        modules.add_module(Box::new(GrpcWeb))
    }

    async fn early_request_filter(
        &self,
        session: &mut Session,
        _ctx: &mut Self::CTX,
    ) -> Result<()> {
        let grpc = session
            .downstream_modules_ctx
            .get_mut::<GrpcWebBridge>()
            .expect("GrpcWebBridge module added");

        // initialize gRPC module for this request
        grpc.init();
        Ok(())
    }

    async fn upstream_peer(
        &self,
        _session: &mut Session,
        _ctx: &mut Self::CTX,
    ) -> Result<Box<HttpPeer>> {
        // this needs to be your gRPC server
        let grpc_peer = Box::new(HttpPeer::new(
            ("1.1.1.1", 443),
            true,
            "one.one.one.one".to_string(),
        ));
        Ok(grpc_peer)
    }
}

// RUST_LOG=INFO cargo run --example grpc_web_module

fn main() {
    env_logger::init();

    // read command line arguments
    let opt = Opt::parse();
    let mut my_server = Server::new(Some(opt)).unwrap();
    my_server.bootstrap();

    let mut my_proxy =
        pingora_proxy::http_proxy_service(&my_server.configuration, GrpcWebBridgeProxy);
    my_proxy.add_tcp("0.0.0.0:6194");

    my_server.add_service(my_proxy);
    my_server.run_forever();
}
