use async_trait::async_trait;
use once_cell::sync::Lazy;
use pingora_core::prelude::*;
use pingora_http::{RequestHeader, ResponseHeader};
use pingora_limits::rate::Rate;
use pingora_load_balancing::prelude::{RoundRobin, TcpHealthCheck};
use pingora_load_balancing::LoadBalancer;
use pingora_proxy::{http_proxy_service, ProxyHttp, Session};
use std::sync::Arc;
use std::time::Duration;

fn main() {
    env_logger::init();

    let mut server = Server::new(None).unwrap();
    server.bootstrap();

    let mut upstreams =
        LoadBalancer::try_from_iter(["1.1.1.1:443", "1.0.0.1:443"]).unwrap();

    // Set health check
    let hc = TcpHealthCheck::new();
    upstreams.set_health_check(hc);
    upstreams.health_check_frequency = Some(Duration::from_secs(1));

    // Set background service
    let background = background_service("health check", upstreams);
    let upstreams = background.task();

    // Set load balancer
    let mut lb = http_proxy_service(&server.configuration, LB(upstreams));
    lb.add_tcp("0.0.0.0:6188");

    server.add_service(background);
    server.add_service(lb);
    server.run_forever();
}

pub struct LB(Arc<LoadBalancer<RoundRobin>>);

impl LB {
    pub fn get_request_appid(&self, session: &Session) -> Option<String> {
        session
            .req_header()
            .headers
            .get("appid")
            .and_then(|v| v.to_str().ok().map(|s| s.to_string()))
    }
}

// Rate limiter
static RATE_LIMITER: Lazy<Rate> = Lazy::new(|| Rate::new(Duration::from_secs(1)));

// Max requests per second per client
const MAX_REQ_PER_SEC: isize = 1;

#[async_trait]
impl ProxyHttp for LB {
    type CTX = ();

    fn new_ctx(&self) -> Self::CTX {
        ()
    }

    async fn request_filter(
        &self,
        session: &mut Session,
        _ctx: &mut Self::CTX,
    ) -> Result<bool> {
        let appid = match self.get_request_appid(session) {
            None => return Ok(false), // No client appid found, skip rate limiting
            Some(appid) => appid,
        };

        // Retrieve the current window requests
        let curr_window_requests = RATE_LIMITER.observe(&appid, 1);
        if curr_window_requests > MAX_REQ_PER_SEC {
            // Rate limited, return 429
            let mut header = ResponseHeader::build(429, None).unwrap();
            header
                .insert_header("X-Rate-Limit-Limit", MAX_REQ_PER_SEC.to_string())
                .unwrap();
            header
                .insert_header("X-Rate-Limit-Remaining", "0")
                .unwrap();
            header.insert_header("X-Rate-Limit-Reset", "1").unwrap();
            session.set_keepalive(None);
            session
                .write_response_header(Box::new(header), true)
                .await?;
            return Ok(true);
        }
        Ok(false)
    }

    async fn upstream_peer(
        &self,
        _session: &mut Session,
        _ctx: &mut Self::CTX,
    ) -> Result<Box<HttpPeer>> {
        let upstream = self.0.select(b"", 256).unwrap();
        // Set SNI
        let peer = Box::new(HttpPeer::new(
            upstream,
            true,
            "one.one.one.one".to_string(),
        ));
        Ok(peer)
    }

    async fn upstream_request_filter(
        &self,
        _session: &mut Session,
        upstream_request: &mut RequestHeader,
        _ctx: &mut Self::CTX,
    ) -> Result<()> {
        upstream_request
            .insert_header("Host", "one.one.one.one")
            .unwrap();
        Ok(())
    }
}
