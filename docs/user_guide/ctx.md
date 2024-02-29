# Sharing state across phases with `CTX`

## Using `CTX`
The custom filters users implement in different phases of the request don't interact with each other directly. In order to share information and state across the filters, users can define a `CTX` struct. Each request owns a single `CTX` object. All the filters are able to read and update members of the `CTX` object. The CTX object will be dropped at the end of the request.

### Example

In the following example, the proxy parses the request header in the `request_filter` phase, it stores the boolean flag so that later in the `upstream_peer` phase the flag is used to decide which server to route traffic to. (Technically, the header can be parsed in `upstream_peer` phase, but we just do it in an earlier phase just for the demonstration.)

```Rust
pub struct MyProxy();

pub struct MyCtx {
    beta_user: bool,
}

fn check_beta_user(req: &pingora_http::RequestHeader) -> bool {
    // some simple logic to check if user is beta
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
        Ok(false)
    }

    async fn upstream_peer(
        &self,
        _session: &mut Session,
        ctx: &mut Self::CTX,
    ) -> Result<Box<HttpPeer>> {
        let addr = if ctx.beta_user {
            info!("I'm a beta user");
            ("1.0.0.1", 443)
        } else {
            ("1.1.1.1", 443)
        };

        let peer = Box::new(HttpPeer::new(addr, true, "one.one.one.one".to_string()));
        Ok(peer)
    }
}
```

## Sharing state across requests
Sharing state such as a counter, cache and other info across requests is common. There is nothing special needed for sharing resources and data across requests in Pingora. `Arc`, `static` or any other mechanism can be used.


### Example
Let's modify the example above to track the number of beta visitors as well as the number of total visitors. The counters can either be defined in the `MyProxy` struct itself or defined as a global variable. Because the counters can be concurrently accessed, Mutex is used here.

```Rust
// global counter
static REQ_COUNTER: Mutex<usize> = Mutex::new(0);

pub struct MyProxy {
    // counter for the service
    beta_counter: Mutex<usize>, // AtomicUsize works too
}

pub struct MyCtx {
    beta_user: bool,
}

fn check_beta_user(req: &pingora_http::RequestHeader) -> bool {
    // some simple logic to check if user is beta
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
        Ok(false)
    }

    async fn upstream_peer(
        &self,
        _session: &mut Session,
        ctx: &mut Self::CTX,
    ) -> Result<Box<HttpPeer>> {
        let mut req_counter = REQ_COUNTER.lock().unwrap();
        *req_counter += 1;

        let addr = if ctx.beta_user {
            let mut beta_count = self.beta_counter.lock().unwrap();
            *beta_count += 1;
            info!("I'm a beta user #{beta_count}");
            ("1.0.0.1", 443)
        } else {
            info!("I'm an user #{req_counter}");
            ("1.1.1.1", 443)
        };

        let peer = Box::new(HttpPeer::new(addr, true, "one.one.one.one".to_string()));
        Ok(peer)
    }
}
```

The complete example can be found under [`pingora-proxy/examples/ctx.rs`](../../pingora-proxy/examples/ctx.rs). You can run it using `cargo`:
```
RUST_LOG=INFO cargo run --example ctx
```