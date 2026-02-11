use async_trait::async_trait;

use pingora_core::modules::http::HttpModules;
use pingora_core::server::Server;
use pingora_core::upstreams::peer::HttpPeer;
use pingora_core::Result;
use pingora_http::RequestHeader;
use pingora_proxy::{ProxyHttp, Session};
use std::any::Any;

/// This example shows how to build and import 3rd party modules

/// A simple ACL to check "Authorization: basic $credential" header
mod my_acl {
    use super::*;
    use pingora_core::modules::http::{HttpModule, HttpModuleBuilder, Module};
    use pingora_error::{Error, ErrorType}; // Import Error and ErrorType here

    // This is the struct for per-request module context
    pub struct MyAclCtx {
        credential_header: String,
    }

    // Implement how the module would consume and/or modify request and/or response
    #[async_trait]
    impl HttpModule for MyAclCtx {
        async fn request_header_filter(&mut self, req: &mut RequestHeader) -> Result<()> {
            if let Some(auth) = req.headers.get("Authorization") {
                if auth.as_bytes() != self.credential_header.as_bytes() {
                    return Error::e_explain(
                        ErrorType::HTTPStatus(403),
                        "Auth failed, credential mismatch",
                    );
                } else {
                    Ok(())
                }
            } else {
                Error::e_explain(
                    ErrorType::HTTPStatus(403),
                    "Auth failed, no auth header",
                )
            }
        }

        // Boilerplate code for all modules
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
        // This function defines how to create each context. This function is called when a new request arrives
        fn init(&self) -> Module {
            Box::new(MyAclCtx {
                // Make it easier to compare header
                // We could also store this value in MyAcl and use Arc to share it with every context.
                credential_header: format!("basic {}", self.credential),
            })
        }
    }
}

pub struct MyProxy;

#[async_trait]
impl ProxyHttp for MyProxy {
    type CTX = ();

    fn new_ctx(&self) -> Self::CTX {
        ()
    }

    // This function is only called once when the server starts
    fn init_downstream_modules(&self, modules: &mut HttpModules) {
        // Add the module to MyProxy
        modules.add_module(Box::new(my_acl::MyAcl {
            credential: "testcode".into(),
        }));
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

// To run the example:
// RUST_LOG=INFO cargo run --example use_module
// curl 127.0.0.1:6193 -H "Host: one.one.one.one" -v
// curl 127.0.0.1:6193 -H "Host: one.one.one.one" -H "Authorization: basic testcode"
// curl 127.0.0.1:6193 -H "Host: one.one.one.one" -H "Authorization: basic wrong" -v
fn main() {
    env_logger::init();

    // Create the server without command line arguments
    let mut my_server = Server::new(None).unwrap();
    my_server.bootstrap();

    let mut my_proxy = pingora_proxy::http_proxy_service(&my_server.configuration, MyProxy);
    my_proxy.add_tcp("0.0.0.0:6193");

    my_server.add_service(my_proxy);
    my_server.run_forever();
}
