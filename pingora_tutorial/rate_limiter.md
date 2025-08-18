# Tutorial: Building a Rate-Limited, Load-Balanced HTTP Proxy in Rust with Pingora

This tutorial shows how to create a **load-balanced** and **rate-limited** HTTP proxy using the [Pingora](https://docs.rs/pingora-core/latest/pingora_core/) ecosystem in Rust. We’ll break down the code step by step, explaining how the **load balancer**, **health checks**, and **rate limiting** are configured. We’ll also discuss how you can modify or extend each component for different scenarios (multiple upstreams, custom health checks, varying rate limits, etc.).

---

## Table of Contents

1. [Introduction](#introduction)  
2. [Code Overview](#code-overview)  
   - [Imports & Core Setup](#imports--core-setup)  
   - [Main Function](#main-function)  
     1. [Creating the Pingora `Server`](#1-creating-the-pingora-server)  
     2. [Setting Up the Load Balancer](#2-setting-up-the-load-balancer)  
       - [Load Balancer Construction](#load-balancer-construction)  
       - [Health Check Configuration](#health-check-configuration)  
     3. [Background Service & Final Service Registration](#3-background-service--final-service-registration)  
   - [`LB` Struct and `ProxyHttp` Implementation](#lb-struct-and-proxyhttp-implementation)  
     1. [Rate Limiting with `RATE_LIMITER`](#1-rate-limiting-with-rate_limiter)  
     2. [Request Filter & Limit Enforcement](#2-request-filter--limit-enforcement)  
     3. [Upstream Selection](#3-upstream-selection)  
     4. [Upstream Request Filter](#4-upstream-request-filter)  
3. [Examples of Customizations](#examples-of-customizations)  
   1. [Changing the Rate Limit Policy](#1-changing-the-rate-limit-policy)  
   2. [Multiple or Dynamic Upstreams](#2-multiple-or-dynamic-upstreams)  
   3. [Custom Health Checks](#3-custom-health-checks)  
   4. [TLS Configuration & SNI](#4-tls-configuration--sni)  
   5. [Additional Filters & Logging](#5-additional-filters--logging)  
4. [Complete Example Code](#complete-example-code)  
5. [Testing & Verification](#testing--verification)  
6. [Conclusion](#conclusion)

---

## Introduction

In a **modern proxy** setup, you might need to:

- **Distribute** traffic among multiple upstream servers.  
- **Check** the health of your servers and only send traffic to healthy endpoints.  
- **Rate-limit** certain clients, so they don’t overwhelm your service.  

This code uses **Pingora**’s load balancing and rate-limiting modules to demonstrate all these capabilities.

---

## Code Overview

Below is an annotated version of the code. Read on to see how the pieces fit together, and skip to “[Examples of Customizations](#examples-of-customizations)” if you want practical tips for adapting this code to various use cases.

### Imports & Core Setup

```rust
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
```

- **`pingora_core::prelude::*`**: Brings common Pingora items like `Server`, `Result`, etc. into scope.  
- **`pingora_http`**: For dealing with request/response headers.  
- **`pingora_limits::rate::Rate`**: The rate-limiting mechanism we’ll use.  
- **`pingora_load_balancing`**: Contains load-balancing strategies (e.g. `RoundRobin`) and health checks (e.g. `TcpHealthCheck`).  
- **`pingora_proxy`**: Where `ProxyHttp` (the main trait for handling proxy logic) and `http_proxy_service` live.  
- **`once_cell::sync::Lazy`**: Lets us lazily initialize static data (e.g. the rate limiter).  
- **`std::sync::Arc`**: Allows shared ownership of data (like our `LoadBalancer`).  
- **`std::time::Duration`**: For specifying time intervals (e.g. the health check frequency).

### Main Function

```rust
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
```

#### 1. Creating the Pingora `Server`

```rust
let mut server = Server::new(None).unwrap();
server.bootstrap();
```

- We create a `Server` (with no custom configuration) and **bootstrap** it. This typically prepares internal tasks and resources.

#### 2. Setting Up the Load Balancer

##### Load Balancer Construction

```rust
let mut upstreams =
    LoadBalancer::try_from_iter(["1.1.1.1:443", "1.0.0.1:443"]).unwrap();
```

- We create a **`LoadBalancer`** from a list of upstream addresses. Here, we use DNS resolvers for `1.1.1.1:443` and `1.0.0.1:443` (Cloudflare DNS addresses).  
- The default policy is **`RoundRobin`**, meaning new requests alternate between these servers.

##### Health Check Configuration

```rust
let hc = TcpHealthCheck::new();
upstreams.set_health_check(hc);
upstreams.health_check_frequency = Some(Duration::from_secs(1));
```

- We use a **TCP health check**, which tries to connect to the upstream.  
- `health_check_frequency` specifies how often Pingora runs these checks. If a server fails, it’s temporarily removed from rotation until it’s healthy again.

#### 3. Background Service & Final Service Registration

```rust
let background = background_service("health check", upstreams);
let upstreams = background.task();
```

- We wrap the `LoadBalancer` in a “background service,” allowing Pingora to regularly run the health checks.  
- `background.task()` returns an `Arc<LoadBalancer<RoundRobin>>` that we can use in our `LB` struct.

```rust
let mut lb = http_proxy_service(&server.configuration, LB(upstreams));
lb.add_tcp("0.0.0.0:6188");

server.add_service(background);
server.add_service(lb);
server.run_forever();
```

- We create an **HTTP proxy service** using our `LB` struct, listening on port `6188`.  
- We add both the background service (for health checks) and the `lb` proxy service to our `server`.  
- `run_forever()` blocks the main thread and starts the server loop.

---

## `LB` Struct and `ProxyHttp` Implementation

```rust
pub struct LB(Arc<LoadBalancer<RoundRobin>>);
```

- **`LB`** wraps our Arc-ed load balancer. We’ll implement `ProxyHttp` on it.

### 1. Rate Limiting with `RATE_LIMITER`

```rust
static RATE_LIMITER: Lazy<Rate> = Lazy::new(|| Rate::new(Duration::from_secs(1)));
const MAX_REQ_PER_SEC: isize = 1;
```

- **`RATE_LIMITER`**: A static instance of Pingora’s `Rate` limiter, configured with a **1-second** window.  
- **`MAX_REQ_PER_SEC`**: The maximum number of requests allowed per **appid** per second.

### 2. Request Filter & Limit Enforcement

```rust
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
    // ...
}
```

- **`get_request_appid`**: A helper method that reads an `appid` header from the client request. This is how we track which “client” is sending the request.  
- If `appid` is absent, **rate limiting is skipped**. (You might want to handle it differently—see [Customizations](#examples-of-customizations) for ideas.)  
- If the `appid` has exceeded `MAX_REQ_PER_SEC`, we respond with **HTTP 429** (“Too Many Requests”) and add some custom rate limit headers to inform the client about the limit.  
- Returning `Ok(true)` means “**I’ve already sent a response; stop processing**.”

### 3. Upstream Selection

```rust
async fn upstream_peer(
    &self,
    _session: &mut Session,
    _ctx: &mut Self::CTX,
) -> Result<Box<HttpPeer>> {
    let upstream = self.0.select(b"", 256).unwrap();
    let peer = Box::new(HttpPeer::new(
        upstream,
        true,
        "one.one.one.one".to_string(),
    ));
    Ok(peer)
}
```

- We call `self.0.select(b"", 256).unwrap()` on our load balancer to pick an upstream (using the RoundRobin strategy).  
  - `b""` is a placeholder for a key that can be used for consistent hashing or other load balancing strategies.  
  - `256` is the maximum number of tries (to skip unhealthy servers).  
- **`HttpPeer::new(..., true, "one.one.one.one")`** indicates **TLS** is enabled and sets `"one.one.one.one"` as the SNI hostname.  
- You could easily swap this out for your own domain or disable TLS (`false`) for a plain HTTP backend.

### 4. Upstream Request Filter

```rust
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
```

- Before sending the request to the upstream, we set the `Host` header to `"one.one.one.one"`.  
  - This is important if the upstream server expects a specific `Host` for routing or TLS handshake.  
- You can modify or add other headers here, too.

---

## Examples of Customizations

### 1. Changing the Rate Limit Policy

- **Different Window**: Change `Duration::from_secs(1)` to `Duration::from_secs(60)` if you want a per-minute limit.  
- **Per-IP or Per-Header**: Instead of an `appid`, you can track the client’s IP from `X-Forwarded-For` or `session.remote_addr()` for rate limiting.  
- **Adjust the Limit**: Change `MAX_REQ_PER_SEC` to a higher value or make it dynamic based on the `appid`:

  ```rust
  fn get_client_limit(appid: &str) -> isize {
      match appid {
          "premium_user" => 10,
          _ => 1,
      }
  }
  ```

### 2. Multiple or Dynamic Upstreams

- **Add More Servers**: `["1.1.1.1:443", "1.0.0.1:443", "8.8.8.8:443"]`.  
- **Different Load Balancing Strategies**: Pingora also supports Weighted Round Robin, Least Connections, Consistent Hash, etc.  
- **Runtime Changes**: If you need to add or remove upstreams on the fly, you can explore Pingora’s dynamic reconfiguration features.

### 3. Custom Health Checks

- **HTTP Health Check**: Instead of `TcpHealthCheck`, you can define a custom module that sends an HTTP GET to a specific endpoint (e.g. `/health`).  
- **Interval**: Change `health_check_frequency` to run less (like `Duration::from_secs(5)`) or more often.

### 4. TLS Configuration & SNI

- **Disable TLS**: Change the second argument of `HttpPeer::new(...)` to `false`.  
- **Custom Hostname**: Instead of `"one.one.one.one".to_string()`, use your actual domain, e.g. `"api.example.com"`.  
- **Cert Validation**: For internal or dev environments, you might skip certificate validation or specify custom root CAs. This typically requires additional Pingora or Rust TLS configuration.

### 5. Additional Filters & Logging

- **Request & Response Filters**: Add more methods (e.g. `response_filter`) to transform or log the upstream response (inserting, removing, or modifying headers).  
- **Body Filters**: For advanced transformations, you can process request or response bodies asynchronously.  
- **Logging**: Override `logging(&self, session, error, ctx)` from `ProxyHttp` to record request metrics, response codes, or custom logs.

---

## Complete Example Code

```rust
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

    // Create a load balancer with two upstream servers
    let mut upstreams = LoadBalancer::try_from_iter(["1.1.1.1:443", "1.0.0.1:443"]).unwrap();

    // Configure health checks
    let hc = TcpHealthCheck::new();
    upstreams.set_health_check(hc);
    // Check health every 1 second
    upstreams.health_check_frequency = Some(Duration::from_secs(1));

    // Create a background service to manage health checks
    let background = background_service("health check", upstreams);
    let upstreams = background.task();

    // Create the proxy service that uses LB
    let mut lb = http_proxy_service(&server.configuration, LB(upstreams));
    lb.add_tcp("0.0.0.0:6188");

    // Register services (the background service + the load-balanced proxy)
    server.add_service(background);
    server.add_service(lb);
    server.run_forever();
}

// A struct that holds the load balancer inside an Arc
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

// Rate limit: 1 request/second per 'appid'
static RATE_LIMITER: Lazy<Rate> = Lazy::new(|| Rate::new(Duration::from_secs(1)));
const MAX_REQ_PER_SEC: isize = 1;

#[async_trait]
impl ProxyHttp for LB {
    type CTX = ();

    fn new_ctx(&self) -> Self::CTX {
        ()
    }

    // Rate-limit requests based on "appid" header
    async fn request_filter(
        &self,
        session: &mut Session,
        _ctx: &mut Self::CTX,
    ) -> Result<bool> {
        let appid = match self.get_request_appid(session) {
            None => return Ok(false), // No client ID found, skip rate limiting
            Some(appid) => appid,
        };

        let curr_window_requests = RATE_LIMITER.observe(&appid, 1);
        if curr_window_requests > MAX_REQ_PER_SEC {
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
            // Returning Ok(true) means we've already responded
            return Ok(true);
        }
        Ok(false)
    }

    // Select an upstream using round-robin
    async fn upstream_peer(
        &self,
        _session: &mut Session,
        _ctx: &mut Self::CTX,
    ) -> Result<Box<HttpPeer>> {
        let upstream = self.0.select(b"", 256).unwrap();
        let peer = Box::new(HttpPeer::new(
            upstream,
            true,
            "one.one.one.one".to_string(),
        ));
        Ok(peer)
    }

    // Insert/modify headers before forwarding to the upstream
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
```

---

## Testing & Verification

1. **Build and Run**:
   ```bash
   cargo run
   ```
   The server starts, listening on port `6188` for incoming traffic.

2. **Check Health**: Every second, Pingora attempts a TCP connection to `1.1.1.1:443` and `1.0.0.1:443` to verify they are online. If one fails, it’s removed from the rotation.

3. **Rate Limit Test**:
   ```bash
   # Without 'appid' header: no rate limit
   curl -v http://localhost:6188/

   # With 'appid: myclient', first request is allowed:
   curl -v http://localhost:6188/ -H "appid: myclient"

   # Immediately send another with the same 'appid':
   curl -v http://localhost:6188/ -H "appid: myclient"
   # If quick enough, you should get "429 Too Many Requests"
   ```

4. **Load Balancing**: If both upstreams are healthy, your requests should alternate between `1.1.1.1` and `1.0.0.1`. You can monitor logs or add your own logging to confirm.

---

## Conclusion

This example shows how to **rate-limit** requests, **health check** your upstream servers, and **distribute** traffic among multiple backends. By modifying the code in the suggested ways, you can:

- Implement more **complex rate limits** (e.g., multiple tiers, request bursts).  
- Choose **different load balancing** strategies (weighted round robin, consistent hash).  
- Customize or replace the **health checks**.  
- Inject or remove HTTP headers **before** sending the request upstream.  
- Integrate additional **request/response filters** for logging, authentication, rewriting, or other functionalities.

Happy coding! With Pingora, you have a flexible toolset to build production-ready proxies tailored to your application’s needs.
