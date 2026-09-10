
# Building a Custom Reverse Proxy Gateway in Rust with Pingora

In this tutorial, we’ll walk through a Rust-based gateway (reverse proxy) built on the [**Pingora**](https://docs.rs/pingora-core/latest/pingora_core/) ecosystem. By the end, you’ll understand how the code works, how it handles incoming requests, and how to customize it for your own needs—whether for logging, authentication, header manipulation, routing, or metrics collection.

---

## Table of Contents

1. [Introduction](#introduction)  
2. [Code Overview](#code-overview)  
   - [Imports & Dependencies](#imports--dependencies)  
   - [`check_login` Function](#check_login-function)  
   - [`MyGateway` Struct](#mygateway-struct)  
   - [Implementing `ProxyHttp` Trait](#implementing-proxyhttp-trait)  
     1. [Context](#1-context)  
     2. [Request Filter](#2-request-filter)  
     3. [Upstream Peer](#3-upstream-peer)  
     4. [Response Filter](#4-response-filter)  
     5. [Logging](#5-logging)  
3. [Main Function & Server Setup](#main-function--server-setup)  
   - [Server Creation & Bootstrap](#server-creation--bootstrap)  
   - [Proxy & Service Setup](#proxy--service-setup)  
   - [Prometheus Metrics Service](#prometheus-metrics-service)  
   - [Running the Server Forever](#running-the-server-forever)  
4. [How to Modify for Your Own Purposes](#how-to-modify-for-your-own-purposes)  
   1. [Custom Authentication Logic](#1-custom-authentication-logic)  
   2. [Different Upstream Servers](#2-different-upstream-servers)  
   3. [Response Manipulation](#3-response-manipulation)  
   4. [Additional Metrics & Logging](#4-additional-metrics--logging)  
   5. [HTTPS vs. HTTP](#5-https-vs-http)  
5. [Complete Example Code](#complete-example-code)  
6. [Running and Testing](#running-and-testing)  
7. [Conclusion](#conclusion)

---

## Introduction

A reverse proxy (or **gateway**) sits in front of your servers to control and process requests:

- **Forward** client requests to the appropriate backend server (often called the “upstream”).  
- Optionally **filter**, **modify**, or **inspect** requests and responses (e.g., authentication, header injection).  
- **Measure metrics** (requests, latencies, errors, etc.) and **log** request details.

Here, we use **Pingora**, a set of Rust crates that simplify building asynchronous proxy servers. You’ll see how we can wire up our own logic at various stages: before forwarding the request, after receiving the upstream response, and after sending the response back to the client.

---

## Code Overview

Below is an annotated version of our gateway code.

### Imports & Dependencies

```rust
use async_trait::async_trait;
use log::info;
use prometheus::{IntCounter, register_int_counter};

use pingora_core::server::Server;
use pingora_core::upstreams::peer::HttpPeer;
use pingora_core::Result;
use pingora_http::{RequestHeader, ResponseHeader};
use pingora_proxy::{ProxyHttp, Session};
```

- **`async_trait`**: Makes it easier to define async methods in a trait implementation (like `ProxyHttp`).  
- **`log::info`**: For logging. You can use `env_logger` or another logging solution for output.  
- **`prometheus`**: For defining a Prometheus counter to track the number of requests.  
- **`pingora_core`** and **`pingora_proxy`**: The core Pingora libraries that enable server creation, request filtering, upstream selection, and more.  
- **`pingora_http`**: Provides types for HTTP request/response headers.

### `check_login` Function

```rust
fn check_login(req: &RequestHeader) -> bool {
    // Implement your login check logic here
    req.headers.get("Authorization")
        .map(|v| v.as_bytes() == b"password")
        .unwrap_or(false)
}
```

A simple function to check if the request has an `Authorization` header equal to `"password"`.  
- For real production use, you would implement more robust logic (JWT validation, API keys, etc.).

### `MyGateway` Struct

```rust
pub struct MyGateway {
    req_metric: IntCounter,
}
```

- We define a struct to hold any **gateway-wide** state or configuration.  
- Here, it contains `req_metric`, a Prometheus counter that we’ll increment on each request.

### Implementing `ProxyHttp` Trait

This trait tells Pingora **how** to process requests at each stage of the proxy cycle.

#### 1. Context

```rust
type CTX = ();

fn new_ctx(&self) -> Self::CTX {
    ()
}
```

- **`type CTX`**: Defines the session context type. Here, we use the unit type `()`, meaning our per-session context is empty.  
- **`new_ctx`**: Each incoming connection creates a new context. You can store per-request data here (e.g., authentication info, counters, etc.).

#### 2. Request Filter

```rust
async fn request_filter(&self, session: &mut Session, _ctx: &mut Self::CTX) -> Result<bool> {
    if session.req_header().uri.path().starts_with("/login")
        && !check_login(session.req_header())
    {
        let _ = session.respond_error(403).await;
        // Return true to indicate early response
        return Ok(true);
    }
    Ok(false)
}
```

Called **before** we select an upstream. We can choose to **short-circuit** (e.g., respond with an error) and not forward the request at all.

- If the URI path starts with `/login` and the user fails `check_login`, respond with `403 Forbidden` and return `Ok(true)`.  
  - Returning `true` means “**I already sent a response, stop processing**.”

#### 3. Upstream Peer

```rust
async fn upstream_peer(
    &self,
    session: &mut Session,
    _ctx: &mut Self::CTX,
) -> Result<Box<HttpPeer>> {
    let addr = if session.req_header().uri.path().starts_with("/family") {
        ("1.0.0.1", 443)
    } else {
        ("1.1.1.1", 443)
    };

    info!("Connecting to {:?}", addr);

    let peer = Box::new(HttpPeer::new(
        addr,
        true,
        "one.one.one.one".to_string(),
    ));
    Ok(peer)
}
```

Tells Pingora **where** to forward requests. You return a [`HttpPeer`](https://docs.rs/pingora-core/latest/pingora_core/upstreams/peer/struct.HttpPeer.html) that indicates:

- The IP/port to connect to (`addr`).  
- Whether it’s **HTTPS** (`true`) or **HTTP** (`false`).  
- The hostname used for SNI (`"one.one.one.one".to_string()`).

In this sample, `/family` is routed to `1.0.0.1:443` and everything else goes to `1.1.1.1:443` (Cloudflare addresses). You could, for example, change this to `("127.0.0.1", 3000)` if you have a local server on port 3000.

#### 4. Response Filter

```rust
async fn response_filter(
    &self,
    _session: &mut Session,
    upstream_response: &mut ResponseHeader,
    _ctx: &mut Self::CTX,
) -> Result<()> {
    // Replace existing header if any
    upstream_response
        .insert_header("Server", "MyGateway")
        .unwrap();
    // Remove unsupported header
    upstream_response.remove_header("alt-svc");

    Ok(())
}
```

After receiving the **upstream** response but before sending it back to the client, we can modify it:

- Insert or replace the `Server` header with `"MyGateway"`.  
- Remove the `alt-svc` header.

#### 5. Logging

```rust
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

    self.req_metric.inc();
}
```

This is called **after** the response is written to the client, whether successful or not. Common uses:

- **Log** the request/response details.  
- **Record metrics** (in this case, increment a Prometheus counter).

---

## Main Function & Server Setup

Here’s where we initialize and run the Pingora server:

```rust
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
    my_proxy.add_tcp("0.0.0.0:6191");
    my_server.add_service(my_proxy);

    let mut prometheus_service_http =
        pingora_core::services::listening::Service::prometheus_http_service();
    prometheus_service_http.add_tcp("127.0.0.1:6192");
    my_server.add_service(prometheus_service_http);

    my_server.run_forever();
}
```

### Server Creation & Bootstrap

```rust
let mut my_server = Server::new(None).unwrap();
my_server.bootstrap();
```

- Creates a `Server` with an optional configuration (here, `None`).  
- `bootstrap()` typically sets up internal tasks or resources.

### Proxy & Service Setup

```rust
let req_metric = register_int_counter!("req_counter", "Number of requests").unwrap();

let mut my_proxy = pingora_proxy::http_proxy_service(
    &my_server.configuration,
    MyGateway {
        req_metric,
    },
);
my_proxy.add_tcp("0.0.0.0:6191");
my_server.add_service(my_proxy);
```

- We create a Prometheus counter called `"req_counter"`.  
- Create an **HTTP proxy service** using our `MyGateway` struct, passing in the Pingora configuration and the newly created counter.  
- **`add_tcp("0.0.0.0:6191")`** means this proxy listens on port `6191` for incoming traffic.  
- Finally, add this proxy to the server as a **service**.

### Prometheus Metrics Service

```rust
let mut prometheus_service_http =
    pingora_core::services::listening::Service::prometheus_http_service();
prometheus_service_http.add_tcp("127.0.0.1:6192");
my_server.add_service(prometheus_service_http);
```

- Pingora has a built-in Prometheus service that automatically collects metrics from the `prometheus` crate.  
- Listening on `127.0.0.1:6192` means you can access metrics at `http://127.0.0.1:6192/metrics`.

### Running the Server Forever

```rust
my_server.run_forever();
```

- This blocks the current thread and starts the event loop.  
- The server will run until the process is killed or otherwise stopped.

---

## How to Modify for Your Own Purposes

### 1. Custom Authentication Logic

The current example checks if `Authorization` equals `"password"`. For production, you might:

- Check for a **JWT** token.  
- Verify a **Basic Auth** credential.  
- Query a database or third-party service to validate a user.

Just update the `check_login` function and the `request_filter` logic accordingly:

```rust
fn check_login(req: &RequestHeader) -> bool {
    // Example of checking a custom Bearer token
    if let Some(auth_header) = req.headers.get("Authorization") {
        // parse "Bearer <token>"
        // validate <token> somehow
    }
    false
}
```

### 2. Different Upstream Servers

Instead of `1.1.1.1:443`, you might have multiple backend services or a local server. Adjust `upstream_peer`:

```rust
async fn upstream_peer(&self, session: &mut Session, _ctx: &mut Self::CTX) -> Result<Box<HttpPeer>> {
    // Example: forward everything to local server at 127.0.0.1:3000, plain HTTP:
    let addr = ("127.0.0.1", 3000);
    let peer = Box::new(HttpPeer::new(addr, false, "127.0.0.1".to_string()));
    Ok(peer)
}
```

You can also do path-based routing or choose the upstream dynamically based on headers, cookies, load balancing, etc.

### 3. Response Manipulation

Currently, we set `Server: MyGateway` and remove `alt-svc`. You could:

- Add or remove **other** headers like `X-Frame-Options`, `Content-Security-Policy`, or `Location`.  
- Modify **status codes** or handle edge cases (e.g. certain 5xx responses).  
- Cache responses or rewrite body content (though you’d need additional logic to manage the body stream).

### 4. Additional Metrics & Logging

Inside the `logging` method (or throughout other methods), you can:

- **Log** request paths, times, user agents, etc.  
- Create **Prometheus histograms** to measure response times. For example:

  ```rust
  let histogram = register_histogram!("request_duration_seconds", "Request duration").unwrap();
  // Then measure how long each request takes
  ```

### 5. HTTPS vs. HTTP

In `HttpPeer::new(addr, use_tls, sni_hostname)`:

- If you connect to an **HTTPS** server, set `use_tls` to `true`, and provide a real hostname for TLS SNI.  
- If **HTTP**, set `use_tls` to `false`.  

For local servers, you typically do `false`, e.g.:

```rust
HttpPeer::new(("127.0.0.1", 3000), false, "127.0.0.1".to_string())
```

---

## Complete Example Code

Below is the full code for the gateway:

```rust
use async_trait::async_trait;
use log::info;
use prometheus::{IntCounter, register_int_counter};

use pingora_core::server::Server;
use pingora_core::upstreams::peer::HttpPeer;
use pingora_core::Result;
use pingora_http::{RequestHeader, ResponseHeader};
use pingora_proxy::{ProxyHttp, Session};

fn check_login(req: &RequestHeader) -> bool {
    // Implement your login check logic here
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

    async fn request_filter(&self, session: &mut Session, _ctx: &mut Self::CTX) -> Result<bool> {
        if session.req_header().uri.path().starts_with("/login")
            && !check_login(session.req_header())
        {
            let _ = session.respond_error(403).await;
            // Return true to indicate early response
            return Ok(true);
        }
        Ok(false)
    }

    async fn upstream_peer(
        &self,
        session: &mut Session,
        _ctx: &mut Self::CTX,
    ) -> Result<Box<HttpPeer>> {
        let addr = if session.req_header().uri.path().starts_with("/family") {
            ("1.0.0.1", 443)
        } else {
            ("1.1.1.1", 443)
        };

        info!("Connecting to {:?}", addr);

        let peer = Box::new(HttpPeer::new(
            addr,
            true,
            "one.one.one.one".to_string(),
        ));
        Ok(peer)
    }

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
    my_proxy.add_tcp("0.0.0.0:6191");
    my_server.add_service(my_proxy);

    let mut prometheus_service_http =
        pingora_core::services::listening::Service::prometheus_http_service();
    prometheus_service_http.add_tcp("127.0.0.1:6192");
    my_server.add_service(prometheus_service_http);

    my_server.run_forever();
}
```

---

## Running and Testing

1. **Add Dependencies**: In your `Cargo.toml`, ensure you have the relevant crates:

   ```toml
   [dependencies]
   async-trait = "0.1"
   env_logger = "0.9"
   log = "0.4"
   prometheus = "0.14"
   pingora-core = "0.1"
   pingora-proxy = "0.1"
   pingora-http = "0.1"
   # (Check crates.io or docs.rs for exact versions)
   ```

2. **Build and Run**:

   ```bash
   cargo run
   ```

   You’ll see log output, and the gateway will listen on `0.0.0.0:6191`.

3. **Test**:

   - Open another terminal and run:
     ```bash
     curl -v http://localhost:6191/login
     ```
     You should get a `403 Forbidden` if you don’t send the `Authorization: password` header.

     ```bash
     curl -v -H "Authorization: password" http://localhost:6191/login
     ```
     Should forward the request upstream (in this example, to `1.1.1.1` by default).
   
   - **Prometheus Metrics**:
     ```bash
     curl -v http://localhost:6192/metrics
     ```
     You should see various metrics, including `req_counter`.

---

## Conclusion

With **Pingora**, you can create a highly customizable reverse proxy in Rust, handling authentication, routing, logging, and more. By modifying the methods in the `ProxyHttp` trait, you can:

- Decide whether to drop or alter requests (`request_filter`).  
- Dynamically choose an upstream target (`upstream_peer`).  
- Inject or remove headers in the response (`response_filter`).  
- Log request details and record metrics (`logging`).

For real-world scenarios, you might add:

- Complex **load balancing** across multiple backends.  
- **Caching** of responses.  
- **Rate limiting** or **access control**.  
- **TLS termination** if you need to handle HTTPS traffic directly.

Feel free to build on this foundation, add your own logic, and deploy a production-ready Rust gateway tailored to your needs.
