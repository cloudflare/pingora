use async_trait::async_trait;
use log::info;
use std::sync::atomic::{AtomicUsize, Ordering};

use pingora_core::server::configuration::Opt;
use pingora_core::server::Server;
use pingora_core::upstreams::peer::HttpPeer;
use pingora_core::Result;
use pingora_http::RequestHeader;
use pingora_proxy::{ProxyHttp, Session};

/// Global request counter using `AtomicUsize` for thread-safe atomic operations
static REQ_COUNTER: AtomicUsize = AtomicUsize::new(0);

pub struct MyProxy {
    /// Counter for beta users
    beta_counter: AtomicUsize,
}

pub struct MyCtx {
    beta_user: bool,
}

fn check_beta_user(req: &RequestHeader) -> bool {
    // Simple logic to check if user is beta
    req.headers.get("beta-flag").is_some()
}

#[async_trait]
impl ProxyHttp for MyProxy {
    type CTX = MyCtx;

    fn new_ctx(&self) -> Self::CTX {
        MyCtx { beta_user: false }
    }

    async fn request_filter(&self, session: &mut Session, ctx: &mut Self::CTX) -> Result<bool> {
        ctx.beta_user = check_beta_user(session.req_header());
        Ok(false) // Continue processing the request
    }

    async fn upstream_peer(
        &self,
        _session: &mut Session,
        ctx: &mut Self::CTX,
    ) -> Result<Box<HttpPeer>> {
        // Increment the global request counter atomically
        let req_counter = REQ_COUNTER.fetch_add(1, Ordering::Relaxed) + 1;

        let addr = if ctx.beta_user {
            // Increment the beta user counter atomically
            let beta_count = self.beta_counter.fetch_add(1, Ordering::Relaxed) + 1;
            info!("I'm a beta user #{beta_count}");
            ("1.0.0.1", 443)
        } else {
            info!("I'm a user #{req_counter}");
            ("1.1.1.1", 443)
        };

        let peer = Box::new(HttpPeer::new(
            addr,
            true,
            "one.one.one.one".to_string(),
        ));
        Ok(peer)
    }
}

// To run the example:
// RUST_LOG=INFO cargo run --example ctx
// curl 127.0.0.1:6190 -H "Host: one.one.one.one"
// curl 127.0.0.1:6190 -H "Host: one.one.one.one" -H "beta-flag: 1"
fn main() {
    env_logger::init();

    // Read command line arguments
    let opt = Opt::parse_args();
    let mut my_server = Server::new(Some(opt)).unwrap();
    my_server.bootstrap();

    let mut my_proxy = pingora_proxy::http_proxy_service(
        &my_server.configuration,
        MyProxy {
            beta_counter: AtomicUsize::new(0),
        },
    );
    my_proxy.add_tcp("0.0.0.0:6190");

    my_server.add_service(my_proxy);
    my_server.run_forever();
}
