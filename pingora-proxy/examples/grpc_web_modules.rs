use async_trait::async_trait;

use pingora_core::server::Server;
use pingora_core::upstreams::peer::HttpPeer;
use pingora_core::Result;
use pingora_core::{
    modules::http::{
        grpc_web::{GrpcWeb, GrpcWebBridge},
        HttpModules,
    },
};
use pingora_proxy::{ProxyHttp, Session};

/// This example shows how to use the gRPC-web bridge module

pub struct GrpcWebBridgeProxy;

#[async_trait]
impl ProxyHttp for GrpcWebBridgeProxy {
    type CTX = ();

    fn new_ctx(&self) -> Self::CTX {
        ()
    }

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

        // Initialize gRPC module for this request
        grpc.init();
        Ok(())
    }

    async fn upstream_peer(
        &self,
        _session: &mut Session,
        _ctx: &mut Self::CTX,
    ) -> Result<Box<HttpPeer>> {
        // This needs to be your gRPC server
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

    // Create server without command line arguments
    let mut my_server = Server::new(None).unwrap();
    my_server.bootstrap();

    let mut my_proxy =
        pingora_proxy::http_proxy_service(&my_server.configuration, GrpcWebBridgeProxy);
    my_proxy.add_tcp("0.0.0.0:6194");

    my_server.add_service(my_proxy);
    my_server.run_forever();
}
