# Tutorial: Distinguishing Between Beta and Regular Users with a Custom Pingora Proxy

This example showcases how to **maintain user-specific context** in a Pingora-based proxy. Specifically, we’ll show you how to:

- Track whether a request comes from a **beta user** (via a header, e.g. `beta-flag`).  
- Use separate **counters**:
  - A global counter tracking all requests.
  - A beta user counter tracking only beta users.

The proxy then routes beta users to **`("1.0.0.1", 443)`** and regular users to **`("1.1.1.1", 443)`**, all while maintaining and logging the counters.

---

## Table of Contents

1. [Introduction](#introduction)
2. [Code Overview](#code-overview)
   - [Global Request Counter](#global-request-counter)
   - [`MyProxy` Struct & `MyCtx` Context](#myproxy-struct--myctx-context)
   - [Implementing `ProxyHttp` on `MyProxy`](#implementing-proxyhttp-on-myproxy)
     1. [Creating Context (`new_ctx`)](#1-creating-context-new_ctx)
     2. [Request Filter (`request_filter`)](#2-request-filter-request_filter)
     3. [Upstream Peer Selection (`upstream_peer`)](#3-upstream-peer-selection-upstream_peer)
   - [Main Function & Server Setup](#main-function--server-setup)
3. [Testing the Proxy](#testing-the-proxy)
4. [Customization Ideas](#customization-ideas)
   1. [Enhanced Beta Logic](#1-enhanced-beta-logic)
   2. [Multiple Upstreams or Feature Flags](#2-multiple-upstreams-or-feature-flags)
   3. [Counter Storage Options](#3-counter-storage-options)
5. [Complete Example Code](#complete-example-code)
6. [Conclusion](#conclusion)

---

## Introduction

When proxying requests, you may want to **tag** certain users as “beta” (or “VIP,” or “internal”) based on some condition—here, a request header (`beta-flag`). This example:

- Uses **thread-safe atomic counters** (from the standard library’s `std::sync::atomic`) to track:
  - **Global** number of requests.  
  - **Beta** user requests.
- Prints log messages identifying how many total requests and how many beta requests have come in.

---

## Code Overview

### Global Request Counter

```rust
static REQ_COUNTER: AtomicUsize = AtomicUsize::new(0);
```

- A **static** global variable to store the total request count.  
- `AtomicUsize` allows us to increment it from multiple threads without additional locking.

### `MyProxy` Struct & `MyCtx` Context

```rust
pub struct MyProxy {
    beta_counter: AtomicUsize,
}

pub struct MyCtx {
    beta_user: bool,
}
```

1. **`MyProxy`**: Holds a **beta_counter** that tracks requests specifically flagged as “beta.”  
2. **`MyCtx`**: Per-request context storing whether the request is from a beta user (`beta_user: bool`).

### Implementing `ProxyHttp` on `MyProxy`

```rust
#[async_trait]
impl ProxyHttp for MyProxy {
    type CTX = MyCtx;

    fn new_ctx(&self) -> Self::CTX {
        MyCtx { beta_user: false }
    }
    // ...
}
```

#### 1. Creating Context (`new_ctx`)

```rust
fn new_ctx(&self) -> Self::CTX {
    MyCtx { beta_user: false }
}
```

- Each request/connection gets a new `MyCtx`.  
- Initially, we set `beta_user` to `false`; we’ll update it later.

#### 2. Request Filter (`request_filter`)

```rust
async fn request_filter(&self, session: &mut Session, ctx: &mut MyCtx) -> Result<bool> {
    ctx.beta_user = check_beta_user(session.req_header());
    Ok(false) // Continue processing
}
```

- **`check_beta_user`** checks if the request header has a key `beta-flag`.  
- We store the result in `ctx.beta_user`.  
- Returning `Ok(false)` means “don’t short-circuit; keep going.”

#### 3. Upstream Peer Selection (`upstream_peer`)

```rust
async fn upstream_peer(
    &self,
    _session: &mut Session,
    ctx: &mut MyCtx,
) -> Result<Box<HttpPeer>> {
    // Increment global request counter
    let req_counter = REQ_COUNTER.fetch_add(1, Ordering::Relaxed) + 1;

    let addr = if ctx.beta_user {
        // Increment beta counter
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
```

- Atomically increments the **global** request counter (`REQ_COUNTER`).  
- If `ctx.beta_user` is `true`, also increments the **beta** counter (`beta_counter`).  
- Logs info either as “beta user” or “user” with the respective counter.  
- Chooses the upstream address:  
  - Beta: **`1.0.0.1:443`**  
  - Regular: **`1.1.1.1:443`**

---

## Main Function & Server Setup

```rust
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
```

1. **`Opt::parse_args()`**: Reads any command line arguments (like `--help` or config file paths).  
2. Creates a `Server` and calls `bootstrap()`.  
3. **`MyProxy { beta_counter: AtomicUsize::new(0) }`** -> Our proxy instance.  
4. We then add a TCP listener on **port 6190**.  
5. Finally, **`run_forever()`** blocks the thread and runs the server until it’s shut down.

---

## Testing the Proxy

Compile and run:

```bash
RUST_LOG=INFO cargo run
```

Then, issue some requests:

1. **Regular user** (no `beta-flag`):
   ```bash
   curl -v http://127.0.0.1:6190/ -H "Host: one.one.one.one"
   ```
   - Logs might show: **`I'm a user #1`**.  
   - Proxy routes to **`1.1.1.1:443`**.

2. **Beta user** (with `beta-flag` header):
   ```bash
   curl -v http://127.0.0.1:6190/ -H "Host: one.one.one.one" -H "beta-flag: 1"
   ```
   - Logs might show: **`I'm a beta user #1`**.  
   - Proxy routes to **`1.0.0.1:443`**.

Check your console logs to confirm which path was chosen and watch the counters increment.

---

## Customization Ideas

### 1. Enhanced Beta Logic

- Instead of checking for any `beta-flag`, parse an actual token or cookie.  
- Possibly store user info in `ctx` (like user ID) if you need more advanced logic.

### 2. Multiple Upstreams or Feature Flags

- Add more upstream addresses if you want load balancing.  
- Use additional headers or flags to pick a different cluster.  
- e.g., a “gamma user” or “internal user” going to a new address.

### 3. Counter Storage Options

- Right now, we use **`AtomicUsize`**. If you want persistent storage, consider a database or an in-memory data store.  
- For large-scale production, you might use metrics libraries like Prometheus to track these counters in a more robust way.

---

## Complete Example Code

```rust
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
```

---

## Conclusion

In this simple but powerful **Pingora** example, we differentiate between “beta” and “regular” users based on a header. We track each request via atomic counters, proving how easily **per-request context** and **global counters** can be integrated in a Rust proxy. Adapt this pattern to any scenario where you need to label users or distribute them across different upstreams.
