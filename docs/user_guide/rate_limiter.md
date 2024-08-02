# **RateLimiter quickstart**
Pingora provides a crate `pingora-limits` which provides a simple and easy to use rate limiter for your application. Below is an example of how you can use [`Rate`](https://docs.rs/pingora-limits/latest/pingora_limits/rate/struct.Rate.html) to create an application that uses multiple limiters to restrict the rate at which requests can be made on a per-app basis (determined by a request header).

## Steps
1. Add the following dependencies to your `Cargo.toml`:
   ```toml
   async-trait="0.1"
   pingora = { version = "0.3", features = [ "lb" ] }
   pingora-limits = "0.3.0"
   once_cell = "1.19.0"
   ```
2. Declare a global rate limiter map to store the rate limiter for each client. In this example, we use `appid`.
3. Override the `request_filter` method in the `ProxyHttp` trait to implement rate limiting.
   1. Retrieve the client appid from header.
   2. Retrieve the current window requests from the rate limiter map. If there is no rate limiter for the client, create a new one and insert it into the map.
   3. If the current window requests exceed the limit, return 429 and set RateLimiter associated headers.
   4. If the request is not rate limited, return `Ok(false)` to continue the request.

## Example
```rust
use async_trait::async_trait;
use once_cell::sync::Lazy;
use pingora::http::ResponseHeader;
use pingora::prelude::*;
use pingora_limits::rate::Rate;
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
```

## Testing
To use the example above, 

1. Run your program with `cargo run`. 
2. Verify the program is working with a few executions of ` curl localhost:6188 -H "appid:1" -v`
   - The first request should work and any later requests that arrive within 1s of a previous request should fail with: 
     ```
     *   Trying 127.0.0.1:6188...
     * Connected to localhost (127.0.0.1) port 6188 (#0)
     > GET / HTTP/1.1
     > Host: localhost:6188
     > User-Agent: curl/7.88.1
     > Accept: */*
     > appid:1
     > 
     < HTTP/1.1 429 Too Many Requests
     < X-Rate-Limit-Limit: 1
     < X-Rate-Limit-Remaining: 0
     < X-Rate-Limit-Reset: 1
     < Date: Sun, 14 Jul 2024 20:29:02 GMT
     < Connection: close
     < 
     * Closing connection 0
     ```

## Complete Example
You can run the pre-made example code in the [`pingora-proxy` examples folder](https://github.com/cloudflare/pingora/tree/main/pingora-proxy/examples/rate_limiter.rs) with 

```
cargo run --example rate_limiter
```