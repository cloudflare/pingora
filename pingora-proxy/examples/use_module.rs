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

use pingora_core::modules::http::HttpModules;
use pingora_core::server::configuration::Opt;
use pingora_core::server::Server;
use pingora_core::upstreams::peer::HttpPeer;
use pingora_core::Result;
use pingora_http::RequestHeader;
use pingora_proxy::{ProxyHttp, Session};

/// This example shows how to build and import 3rd party modules

/// A simple ACL to check "Authorization: basic $credential" header
mod my_acl {
    use super::*;
    use pingora_core::modules::http::{HttpModule, HttpModuleBuilder, Module};
    use pingora_error::{Error, ErrorType::HTTPStatus};
    use std::any::Any;

    // This is the struct for per request module context
    struct MyAclCtx {
        credential_header: String,
    }

    // Implement how the module would consume and/or modify request and/or response
    #[async_trait]
    impl HttpModule for MyAclCtx {
        async fn request_header_filter(&mut self, req: &mut RequestHeader) -> Result<()> {
            let Some(auth) = req.headers.get(http::header::AUTHORIZATION) else {
                return Error::e_explain(HTTPStatus(403), "Auth failed, no auth header");
            };

            if auth.as_bytes() != self.credential_header.as_bytes() {
                Error::e_explain(HTTPStatus(403), "Auth failed, credential mismatch")
            } else {
                Ok(())
            }
        }

        // boilerplate code for all modules
        fn as_any(&self) -> &dyn Any {
            self
        }
        fn as_any_mut(&mut self) -> &mut dyn Any {
            self
        }
    }

    // This is the singleton object which will be attached to the server
    pub struct MyAcl {
        pub credential: String,
    }
    impl HttpModuleBuilder for MyAcl {
        // This function defines how to create each Ctx. This function is called when a new request
        // arrives
        fn init(&self) -> Module {
            Box::new(MyAclCtx {
                // Make it easier to compare header
                // We could also store this value in MyAcl and use Arc to share it with every Ctx.
                credential_header: format!("basic {}", self.credential),
            })
        }
    }
}

pub struct MyProxy;

#[async_trait]
impl ProxyHttp for MyProxy {
    type CTX = ();
    fn new_ctx(&self) -> Self::CTX {}

    // This function is only called once when the server starts
    fn init_downstream_modules(&self, modules: &mut HttpModules) {
        // Add the module to MyProxy
        modules.add_module(Box::new(my_acl::MyAcl {
            credential: "testcode".into(),
        }))
    }

    async fn upstream_peer(
        &self,
        _session: &mut Session,
        _ctx: &mut Self::CTX,
    ) -> Result<Box<HttpPeer>> {
        let peer = Box::new(HttpPeer::new(
            ("1.1.1.1", 443),
            true,
            "one.one.one.one".to_string(),
        ));
        Ok(peer)
    }
}

// RUST_LOG=INFO cargo run --example use_module
// curl 127.0.0.1:6193 -H "Host: one.one.one.one" -v
// curl 127.0.0.1:6193 -H "Host: one.one.one.one" -H "Authorization: basic testcode"
// curl 127.0.0.1:6193 -H "Host: one.one.one.one" -H "Authorization: basic wrong" -v
fn main() {
    env_logger::init();

    // read command line arguments
    let opt = Opt::parse();
    let mut my_server = Server::new(Some(opt)).unwrap();
    my_server.bootstrap();

    let mut my_proxy = pingora_proxy::http_proxy_service(&my_server.configuration, MyProxy);
    my_proxy.add_tcp("0.0.0.0:6193");

    my_server.add_service(my_proxy);
    my_server.run_forever();
}
