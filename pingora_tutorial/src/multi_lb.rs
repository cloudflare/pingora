use async_trait::async_trait;
use std::sync::Arc;

use pingora_core::{prelude::*, services::background::GenBackgroundService};
use pingora_core::upstreams::peer::HttpPeer;
use pingora_load_balancing::{
    health_check::TcpHealthCheck,
    selection::{BackendIter, BackendSelection, RoundRobin},
    LoadBalancer,
};
use pingora_proxy::{http_proxy_service, ProxyHttp, Session};

struct Router {
    cluster_one: Arc<LoadBalancer<RoundRobin>>,
    cluster_two: Arc<LoadBalancer<RoundRobin>>,
}

#[async_trait]
impl ProxyHttp for Router {
    type CTX = ();

    // Corrected the `new_ctx` method to return `Self::CTX`
    fn new_ctx(&self) -> Self::CTX {
        ()
    }

    // Updated the return type to `Result<Box<HttpPeer>>`
    async fn upstream_peer(
        &self,
        session: &mut Session,
        _ctx: &mut Self::CTX,
    ) -> Result<Box<HttpPeer>> {
        // Determine LB cluster based on request URI
        let cluster = if session.req_header().uri.path().starts_with("/one/") {
            &self.cluster_one
        } else {
            &self.cluster_two
        };

        let upstream = cluster
            .select(b"", 256) // Hash doesn't matter for round robin
            .unwrap();

        println!("upstream peer is: {:?}", upstream);

        // Create a new HttpPeer instance and wrap it in a Box
        let peer = Box::new(HttpPeer::new(
            upstream,
            true,
            "one.one.one.one".to_string(),
        ));
        Ok(peer)
    }
}

fn build_cluster_service<S>(upstreams: &[&str]) -> GenBackgroundService<LoadBalancer<S>>
where
    S: BackendSelection + 'static,
    S::Iter: BackendIter,
{
    let mut cluster = LoadBalancer::try_from_iter(upstreams).unwrap();
    cluster.set_health_check(TcpHealthCheck::new());
    cluster.health_check_frequency = Some(std::time::Duration::from_secs(1));

    background_service("cluster health check", cluster)
}

// RUST_LOG=INFO cargo run --example multi_lb
// curl 127.0.0.1:6188/one/
// curl 127.0.0.1:6188/two/
fn main() {
    let mut my_server = Server::new(None).unwrap();
    my_server.bootstrap();

    // Build multiple clusters
    let cluster_one = build_cluster_service::<RoundRobin>(&["1.1.1.1:443", "127.0.0.1:343"]);
    let cluster_two = build_cluster_service::<RoundRobin>(&["1.0.0.1:443", "127.0.0.2:343"]);

    let router = Router {
        cluster_one: cluster_one.task(),
        cluster_two: cluster_two.task(),
    };
    let mut router_service = http_proxy_service(&my_server.configuration, router);
    router_service.add_tcp("0.0.0.0:6188");

    my_server.add_service(router_service);
    my_server.add_service(cluster_one);
    my_server.add_service(cluster_two);

    my_server.run_forever();
}
