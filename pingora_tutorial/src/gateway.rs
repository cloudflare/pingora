use async_trait::async_trait;
use log::info;
use prometheus::{IntCounter, register_int_counter};

use pingora_core::server::Server;
use pingora_core::upstreams::peer::HttpPeer;
use pingora_core::Result;
use pingora_http::{RequestHeader, ResponseHeader};
use pingora_proxy::{ProxyHttp, Session};

fn check_login(req: &RequestHeader) -> bool {
    req.headers
        .get("Authorization")
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

    // Decide whether to short-circuit this request (e.g. returning an error)
    // before even forwarding to the upstream.
    async fn request_filter(&self, session: &mut Session, _ctx: &mut Self::CTX) -> Result<bool> {
        // Check if the user is authorized for /login
        if session.req_header().uri.path().starts_with("/login") && !check_login(session.req_header()) {
            let _ = session.respond_error(403).await;
            return Ok(true); // Stop further processing
        }
        Ok(false)
    }

    // Choose the upstream server.
    async fn upstream_peer(
        &self,
        session: &mut Session,
        _ctx: &mut Self::CTX,
    ) -> Result<Box<HttpPeer>> {
        // Suppose your local server is listening on 127.0.0.1:3000, plain HTTP:
        let addr = ("127.0.0.1", 3000);
        info!("Connecting to {:?}", addr);

        // The second param is `false` if it's HTTP, `true` if it's HTTPS.
        // The third param is used for SNI if HTTPS. If HTTP, it's typically a placeholder.
        let peer = Box::new(HttpPeer::new(addr, false, "127.0.0.1".to_string()));
        Ok(peer)
    }

    // After receiving a response from the upstream, optionally modify headers/status.
    async fn response_filter(
        &self,
        _session: &mut Session,
        upstream_response: &mut ResponseHeader,
        _ctx: &mut Self::CTX,
    ) -> Result<()> {
        upstream_response
            .insert_header("Server", "MyGateway")
            .unwrap();
        upstream_response.remove_header("alt-svc");
        Ok(())
    }

    // Called after we finish handling the request, for logging or additional metrics.
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

        // Prometheus counter
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
    // This is where the gateway listens for incoming traffic
    my_proxy.add_tcp("0.0.0.0:6191");
    my_server.add_service(my_proxy);

    // Prometheus metrics service on port 6192
    let mut prometheus_service_http =
        pingora_core::services::listening::Service::prometheus_http_service();
    prometheus_service_http.add_tcp("127.0.0.1:6192");
    my_server.add_service(prometheus_service_http);

    my_server.run_forever();
}
