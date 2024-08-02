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
    let mut server = Server::new(Some(Opt::default())).unwrap();
    server.bootstrap();
    let mut upstreams = LoadBalancer::try_from_iter(["1.1.1.1:443", "1.0.0.1:443"]).unwrap();
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

    // let rate = Rate
    server.add_service(background);
    server.add_service(lb);
    server.run_forever();
}

pub struct LB(Arc<LoadBalancer<RoundRobin>>);

impl LB {
    pub fn get_request_appid(&self, session: &mut Session) -> Option<String> {
        match session
            .req_header()
            .headers
            .get("appid")
            .map(|v| v.to_str())
        {
            None => None,
            Some(v) => match v {
                Ok(v) => Some(v.to_string()),
                Err(_) => None,
            },
        }
    }
}

// Rate limiter
static RATE_LIMITER: Lazy<Rate> = Lazy::new(|| Rate::new(Duration::from_secs(1)));

// max request per second per client
static MAX_REQ_PER_SEC: isize = 1;

#[async_trait]
impl ProxyHttp for LB {
    type CTX = ();

    fn new_ctx(&self) {}

    async fn upstream_peer(
        &self,
        _session: &mut Session,
        _ctx: &mut Self::CTX,
    ) -> Result<Box<HttpPeer>> {
        let upstream = self.0.select(b"", 256).unwrap();
        // Set SNI
        let peer = Box::new(HttpPeer::new(upstream, true, "one.one.one.one".to_string()));
        Ok(peer)
    }

    async fn upstream_request_filter(
        &self,
        _session: &mut Session,
        upstream_request: &mut RequestHeader,
        _ctx: &mut Self::CTX,
    ) -> Result<()>
    where
        Self::CTX: Send + Sync,
    {
        upstream_request
            .insert_header("Host", "one.one.one.one")
            .unwrap();
        Ok(())
    }

    async fn request_filter(&self, session: &mut Session, _ctx: &mut Self::CTX) -> Result<bool>
    where
        Self::CTX: Send + Sync,
    {
        let appid = match self.get_request_appid(session) {
            None => return Ok(false), // no client appid found, skip rate limiting
            Some(addr) => addr,
        };

        // retrieve the current window requests
        let curr_window_requests = RATE_LIMITER.observe(&appid, 1);
        if curr_window_requests > MAX_REQ_PER_SEC {
            // rate limited, return 429
            let mut header = ResponseHeader::build(429, None).unwrap();
            header
                .insert_header("X-Rate-Limit-Limit", MAX_REQ_PER_SEC.to_string())
                .unwrap();
            header.insert_header("X-Rate-Limit-Remaining", "0").unwrap();
            header.insert_header("X-Rate-Limit-Reset", "1").unwrap();
            session.set_keepalive(None);
            session
                .write_response_header(Box::new(header), true)
                .await?;
            return Ok(true);
        }
        Ok(false)
    }
}
