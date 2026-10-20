# Tutorial: Using a Load Balancer with Health Checks and TLS in Pingora

This example demonstrates how to set up a **load-balanced** HTTP proxy with:
- **Multiple upstream servers**, including a “bad” server that will fail health checks.
- **TCP health checks** that remove unhealthy servers from rotation automatically.
- **TLS listeners** configured for HTTP/2 (and HTTP/1.1 over TLS).

## Table of Contents

1. [Introduction](#introduction)  
2. [Code Overview](#code-overview)  
   - [Imports & Setup](#imports--setup)  
   - [Defining the `LB` Struct](#defining-the-lb-struct)  
   - [Implementing the `ProxyHttp` Trait on `LB`]](#implementing-the-proxyhttp-trait-on-lb)  
     1. [`upstream_peer` Method](#1-upstream_peer-method)  
     2. [`upstream_request_filter` Method](#2-upstream_request_filter-method)  
   - [Main Function & Server Setup](#main-function--server-setup)  
     1. [Creating the Server & Bootstrapping](#1-creating-the-server--bootstrapping)  
     2. [Configuring the Load Balancer & Health Checks](#2-configuring-the-load-balancer--health-checks)  
     3. [Background Service for Health Checks](#3-background-service-for-health-checks)  
     4. [Proxy Service & TLS Configuration](#4-proxy-service--tls-configuration)  
3. [Modifying the Code](#modifying-the-code)  
   1. [Adding More Upstreams or Changing Their Addresses](#1-adding-more-upstreams-or-changing-their-addresses)  
   2. [Adjusting the Health Check Logic](#2-adjusting-the-health-check-logic)  
   3. [Changing TLS Settings and Certificates](#3-changing-tls-settings-and-certificates)  
   4. [Connecting to a Test Server Locally](#4-connecting-to-a-test-server-locally)  
4. [Complete Example Code](#complete-example-code)  
5. [Running & Testing](#running--testing)  
6. [Conclusion](#conclusion)

---

## Introduction

**Load balancing** in Pingora allows you to distribute incoming requests across multiple upstream servers. By combining **health checks**, you can automatically avoid sending traffic to servers that are down or misbehaving. Additionally, Pingora makes it simple to expose both **plaintext** and **TLS** endpoints, supporting HTTP/1.1 and HTTP/2.

---

## Code Overview

Below, we’ll highlight each section of the code that creates a load balancer with three upstreams, sets up TCP health checks, and configures two listeners:  
- **Port 6188** for plaintext traffic.  
- **Port 6189** for TLS-encrypted traffic.

### Imports & Setup

```rust
use async_trait::async_trait;
use log::info;
use pingora_core::services::background::background_service;
use std::{sync::Arc, time::Duration};

use pingora_core::server::Server;
use pingora_core::upstreams::peer::HttpPeer;
use pingora_core::Result;
use pingora_core::{
    listeners::tls::TlsSettings,
};
use pingora_load_balancing::{health_check, selection::RoundRobin, LoadBalancer};
use pingora_proxy::{ProxyHttp, Session};
use pingora_http::RequestHeader;
```

- **`pingora_core::services::background::background_service`**: Allows us to run health checks in the background.  
- **`pingora_load_balancing::{health_check, selection::RoundRobin, LoadBalancer}`**: Key load balancing constructs, including the `TcpHealthCheck` type and a `RoundRobin` strategy.  
- **`pingora_core::listeners::tls::TlsSettings`**: Lets us configure TLS for a listening port.

### Defining the `LB` Struct

```rust
pub struct LB(Arc<LoadBalancer<RoundRobin>>);
```

- Holds an **Arc** reference to the `LoadBalancer`.  
- We’ll implement `ProxyHttp` on this struct to define how traffic is routed.

### Implementing the `ProxyHttp` Trait on `LB`

#### 1. `upstream_peer` Method

```rust
#[async_trait]
impl ProxyHttp for LB {
    type CTX = ();

    fn new_ctx(&self) -> Self::CTX {
        ()
    }

    async fn upstream_peer(&self, _session: &mut Session, _ctx: &mut Self::CTX) -> Result<Box<HttpPeer>> {
        let upstream = self
            .0
            .select(b"", 256) // hash doesn't matter here
            .unwrap();

        info!("upstream peer is: {:?}", upstream);

        let peer = Box::new(HttpPeer::new(upstream, true, "one.one.one.one".to_string()));
        Ok(peer)
    }

    // ...
}
```

- **`select(b"", 256)`** picks an upstream from the load balancer using **RoundRobin** strategy.  
- The second parameter `256` is the maximum tries if the first one is unhealthy.  
- We create a `HttpPeer` with TLS (`true`) and an SNI hostname `"one.one.one.one"`.  
  - Replace `("one.one.one.one")` with the real SNI host if needed, or set TLS to `false` for HTTP.

#### 2. `upstream_request_filter` Method

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

- Before sending the request upstream, we set `Host` to `"one.one.one.one"`.  
- In a production scenario, change this to your real upstream host.

---

## Main Function & Server Setup

```rust
fn main() {
    env_logger::init();

    let mut my_server = Server::new(None).unwrap();
    my_server.bootstrap();
    // ...
}
```

### 1. Creating the Server & Bootstrapping

```rust
let mut my_server = Server::new(None).unwrap();
my_server.bootstrap();
```

- **`Server::new(None)`** starts a Pingora server with no custom config.  
- **`bootstrap()`** prepares background tasks or other internal logic.

### 2. Configuring the Load Balancer & Health Checks

```rust
let mut upstreams =
    LoadBalancer::try_from_iter(["1.1.1.1:443", "1.0.0.1:443", "127.0.0.1:343"]).unwrap();
```

- We pass a slice of upstream addresses. Note that `"127.0.0.1:343"` is deliberately a “bad” server.  
- `try_from_iter` creates a `LoadBalancer<RoundRobin>` by default.

```rust
let hc = health_check::TcpHealthCheck::new();
upstreams.set_health_check(hc);
upstreams.health_check_frequency = Some(Duration::from_secs(1));
```

- **TCP** health check tries to connect to each server. If it fails, that server is marked unhealthy.  
- **`health_check_frequency`** determines how often the checks run (every 1 second here).

### 3. Background Service for Health Checks

```rust
let background = background_service("health check", upstreams);
let upstreams = background.task();
```

- **`background_service`** spawns a task to handle these checks in the background.  
- We then retrieve an `Arc<LoadBalancer<RoundRobin>>` via `background.task()` to share with our proxy.

### 4. Proxy Service & TLS Configuration

```rust
let mut lb = pingora_proxy::http_proxy_service(&my_server.configuration, LB(upstreams));
lb.add_tcp("0.0.0.0:6188");
```

- We create an **HTTP proxy** and bind it to `0.0.0.0:6188` for plaintext connections.

```rust
let cert_path = format!("{}/tests/keys/server.crt", env!("CARGO_MANIFEST_DIR"));
let key_path = format!("{}/tests/keys/key.pem", env!("CARGO_MANIFEST_DIR"));

let mut tls_settings = TlsSettings::intermediate(&cert_path, &key_path).unwrap();
tls_settings.enable_h2();
lb.add_tls_with_settings("0.0.0.0:6189", None, tls_settings);
```

- We load a self-signed certificate and key (in the `tests/keys` folder).  
- **`TlsSettings::intermediate`** applies a default TLS security policy.  
- **`enable_h2()`** allows HTTP/2 connections.  
- We bind TLS on **port 6189**.  

Finally, we register both the **proxy service** and the **background** health check service with `my_server`:

```rust
my_server.add_service(lb);
my_server.add_service(background);
my_server.run_forever();
```

---

## Modifying the Code

### 1. Adding More Upstreams or Changing Their Addresses

- Swap out `["1.1.1.1:443", "1.0.0.1:443"]` for your own servers.  
- You can include additional addresses like `["service1:443", "service2:443"]`.

### 2. Adjusting the Health Check Logic

- If you want an **HTTP** health check (sending a GET to `/health` endpoint), you’d need to implement a different `TcpHealthCheck` or use an **HTTP** approach.  
- Increase or decrease `health_check_frequency` to reduce overhead or improve detection times.

### 3. Changing TLS Settings and Certificates

- To support a real certificate, put the correct paths to your `.crt` and `.key` files.  
- If you want a stricter or custom policy (e.g., requiring client certs), explore Pingora’s `TlsSettings` methods.

### 4. Connecting to a Test Server Locally

- If you have a local server on `127.0.0.1:3000`, switch the upstream list.  
- Update **`upstream_request_filter`** if you need a different `Host` header.  
- In a testing scenario, you can comment out the TLS code if you just want to confirm load balancing without SSL overhead.

---

## Complete Example Code

```rust
use async_trait::async_trait;
use log::info;
use pingora_core::services::background::background_service;
use std::{sync::Arc, time::Duration};

use pingora_core::server::Server;
use pingora_core::upstreams::peer::HttpPeer;
use pingora_core::Result;
use pingora_core::{
    listeners::tls::TlsSettings,
};
use pingora_load_balancing::{health_check, selection::RoundRobin, LoadBalancer};
use pingora_proxy::{ProxyHttp, Session};
use pingora_http::RequestHeader;

pub struct LB(Arc<LoadBalancer<RoundRobin>>);

#[async_trait]
impl ProxyHttp for LB {
    type CTX = ();

    fn new_ctx(&self) -> Self::CTX {
        ()
    }

    async fn upstream_peer(&self, _session: &mut Session, _ctx: &mut Self::CTX) -> Result<Box<HttpPeer>> {
        let upstream = self
            .0
            .select(b"", 256)
            .unwrap();

        info!("upstream peer is: {:?}", upstream);

        // Indicate TLS = true, with SNI "one.one.one.one"
        let peer = Box::new(HttpPeer::new(upstream, true, "one.one.one.one".to_string()));
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

fn main() {
    env_logger::init();

    // Create the server without command line arguments
    let mut my_server = Server::new(None).unwrap();
    my_server.bootstrap();

    // "127.0.0.1:343" is intentionally a bad server
    let mut upstreams =
        LoadBalancer::try_from_iter(["1.1.1.1:443", "1.0.0.1:443", "127.0.0.1:343"]).unwrap();

    // We add health checks to automatically exclude unhealthy servers
    let hc = health_check::TcpHealthCheck::new();
    upstreams.set_health_check(hc);
    upstreams.health_check_frequency = Some(Duration::from_secs(1));

    // Start the health checks in the background
    let background = background_service("health check", upstreams);
    let upstreams = background.task();

    // Create a proxy service using LB
    let mut lb = pingora_proxy::http_proxy_service(&my_server.configuration, LB(upstreams));
    // Plaintext listener on port 6188
    lb.add_tcp("0.0.0.0:6188");

    // Setup TLS on port 6189
    let cert_path = format!("{}/tests/keys/server.crt", env!("CARGO_MANIFEST_DIR"));
    let key_path = format!("{}/tests/keys/key.pem", env!("CARGO_MANIFEST_DIR"));
    let mut tls_settings = TlsSettings::intermediate(&cert_path, &key_path).unwrap();
    tls_settings.enable_h2();
    lb.add_tls_with_settings("0.0.0.0:6189", None, tls_settings);

    // Add the proxy service and background service to the server
    my_server.add_service(lb);
    my_server.add_service(background);
    my_server.run_forever();
}
```

---

## Running & Testing

1. **Build and Run**:
   ```bash
   cargo run
   ```
   Make sure `server.crt` and `key.pem` exist at the specified paths (`tests/keys`), or adjust the paths.

2. **Plaintext Test**:
   - Send requests to `http://127.0.0.1:6188/`.  
   - The proxy will round-robin between `1.1.1.1:443` and `1.0.0.1:443`, skipping `127.0.0.1:343` after health checks fail.

3. **TLS Test**:
   - Use `curl --insecure https://127.0.0.1:6189/` if using self-signed certs.  
   - This tests the TLS listener; traffic is still proxied upstream with a TLS handshake to `1.1.1.1`.

4. **Observe Logs**:
   - You should see logs indicating which upstream is selected.  
   - After a second, the bad server (`127.0.0.1:343`) should be marked unhealthy.

---

## Conclusion

With this Pingora-based setup, you can:

- **Automatically** exclude unhealthy servers from your load-balancing pool.  
- Expose both **plaintext** (HTTP) and **TLS** (HTTPS) endpoints, letting clients connect over their preferred protocols.  
- Customize health checks, load balancing strategies, and TLS settings to match your production environment.

Feel free to **modify** the code to add more advanced routing, more robust health checks (HTTP-based), or different TLS/cert configurations. Good luck building your next load-balanced Rust proxy with Pingora!
