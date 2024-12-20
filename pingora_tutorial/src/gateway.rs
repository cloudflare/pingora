use async_trait::async_trait;
use log::info;
use prometheus::{IntCounter, register_int_counter};

use pingora_core::server::Server;
use pingora_core::upstreams::peer::HttpPeer;
use pingora_core::Result;
use pingora_http::{RequestHeader, ResponseHeader};
use pingora_proxy::{ProxyHttp, Session};

fn check_login(req: &RequestHeader) -> bool {
    // Implement your login check logic here
    req.headers.get("Authorization")
        .map(|v| v.as_bytes() == b"password")
        .unwrap_or(false)
}

pub struct MyGateway {
    req_metric: IntCounter,
}

#[async_trait]
impl ProxyHttp for MyGateway {
    type CTX = ();

    fn new_ctx(&self) -> Self::CTX {
        ()
    }

    async fn request_filter(&self, session: &mut Session, _ctx: &mut Self::CTX) -> Result<bool> {
        if session.req_header().uri.path().starts_with("/login")
            && !check_login(session.req_header())
        {
            let _ = session.respond_error(403).await;
            // Return true to indicate early response
            return Ok(true);
        }
        Ok(false)
    }

    async fn upstream_peer(
        &self,
        session: &mut Session,
        _ctx: &mut Self::CTX,
    ) -> Result<Box<HttpPeer>> {
        let addr = if session.req_header().uri.path().starts_with("/family") {
            ("1.0.0.1", 443)
        } else {
            ("1.1.1.1", 443)
        };

        info!("Connecting to {:?}", addr);

        let peer = Box::new(HttpPeer::new(
            addr,
            true,
            "one.one.one.one".to_string(),
        ));
        Ok(peer)
    }

    async fn response_filter(
        &self,
        _session: &mut Session,
        upstream_response: &mut ResponseHeader,
        _ctx: &mut Self::CTX,
    ) -> Result<()> {
        // Replace existing header if any
        upstream_response
            .insert_header("Server", "MyGateway")
            .unwrap();
        // Remove unsupported header
        upstream_response.remove_header("alt-svc");

        Ok(())
    }

    async fn logging(
        &self,
        session: &mut Session,
        _e: Option<&pingora_core::Error>,
        _ctx: &mut Self::CTX,
    ) {
        let response_code = session
            .response_written()
            .map_or(0, |resp| resp.status.as_u16());
        info!(
            "Request to {} responded with status code {}",
            session.req_header().uri.path(),
            response_code
        );

        self.req_metric.inc();
    }
}

fn main() {
    env_logger::init();

    // Create the server without options
    let mut my_server = Server::new(None).unwrap();
    my_server.bootstrap();

    let req_metric = register_int_counter!("req_counter", "Number of requests").unwrap();

    let mut my_proxy = pingora_proxy::http_proxy_service(
        &my_server.configuration,
        MyGateway {
            req_metric,
        },
    );
    my_proxy.add_tcp("0.0.0.0:6191");
    my_server.add_service(my_proxy);

    let mut prometheus_service_http =
        pingora_core::services::listening::Service::prometheus_http_service();
    prometheus_service_http.add_tcp("127.0.0.1:6192");
    my_server.add_service(prometheus_service_http);

    my_server.run_forever();
}
