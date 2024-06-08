# **RateLimiter quickstart**
Pingora provides a crate `pingora-limits` which provides a simple and easy to use rate limiter for your application.

## Steps
1. Add the following dependencies to your `Cargo.toml`:
   ```toml
   pingora-limits = "0.1.0"
   lazy_static = "1.4.0"
   ```
2. Declare a global rate limiter map to store the rate limiter for each client. In this example, we use `appid`.
3. Override the `request_filter` method in the `ProxyHttp` trait to implement rate limiting.
   1. Retrieve the client appid from header.
   2. Retrieve the current window requests from the rate limiter map. If there is no rate limiter for the client, create a new one and insert it into the map.
   3. If the current window requests exceed the limit, return 429 and set RateLimiter associated headers.
   4. If the request is not rate limited, return `Ok(false)` to continue the request.

## Example
```rust
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use async_trait::async_trait;
use lazy_static::lazy_static;
use pingora::http::ResponseHeader;
use pingora::prelude::*;
use pingora_limits::rate::Rate;

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
      match session.req_header().headers.get("appid").map(|v| v.to_str()) {
         None => None,
         Some(v) => match v {
            Ok(v) => Some(v.to_string()),
            Err(_) => None
         }
      }
   }
}


// global limiter
lazy_static! {
    static ref RATE_LIMITER_MAP: Arc<Mutex<HashMap<String, Rate>>> = {
       Arc::new(Mutex::new(HashMap::new()))
    };
}
// max request per second per client
static MAX_REQ_PER_SEC: isize = 1;

#[async_trait]
impl ProxyHttp for LB {
   type CTX = ();

   fn new_ctx(&self) -> Self::CTX {
      ()
   }
   async fn upstream_peer(&self, _session: &mut Session, _ctx: &mut Self::CTX) -> Result<Box<HttpPeer>> {
      let upstream = self.0
              .select(b"", 256)
              .unwrap();
      // Set SNI
      let peer = Box::new(HttpPeer::new(upstream, true, "one.one.one.one".to_string()));
      Ok(peer)
   }
   async fn upstream_request_filter(&self, _session: &mut Session, upstream_request: &mut RequestHeader, _ctx: &mut Self::CTX) -> Result<()> where Self::CTX: Send + Sync {
      upstream_request.insert_header("Host", "one.one.one.one").unwrap();
      Ok(())
   }
   async fn request_filter(&self, session: &mut Session, _ctx: &mut Self::CTX) -> Result<bool> where Self::CTX: Send + Sync {
      let appid = match self.get_request_appid(session) {
         None => return Ok(false), // no client appid found, skip rate limiting
         Some(addr) => addr
      };

      // retrieve the current window requests
      let curr_window_requests = {
         let mut rate_limiter_map = RATE_LIMITER_MAP.lock().unwrap();
         let rate_limiter = rate_limiter_map.entry(appid.clone()).insert_or_with(|| Rate::new(Duration::from_secs(1)));
         rate_limiter.observe(&appid, 1)
      };
      if curr_window_requests > MAX_REQ_PER_SEC { // rate limited, return 429
         let mut header = ResponseHeader::build(429, None).unwrap();
         header.insert_header("X-Rate-Limit-Limit", MAX_REQ_PER_SEC.to_string()).unwrap();
         header.insert_header("X-Rate-Limit-Remaining", "0").unwrap();
         header.insert_header("X-Rate-Limit-Reset", "1").unwrap();
         session.set_keepalive(None);
         session.write_response_header(Box::new(header)).await?;
         return Ok(true);
      }
      Ok(false)
   }
}
```
