# Tutorial: Bridging gRPC-Web Traffic with Pingora’s `GrpcWebBridge` Module

This example demonstrates how to **convert gRPC-web requests** into standard gRPC calls using the [Pingora](https://docs.rs/pingora-core) library’s **`GrpcWebBridge`** module. This is especially useful when you have front-end code (such as a browser client) that communicates via **gRPC-web**, but your backend server expects normal gRPC over HTTP/2.

Below, we’ll walk through:

1. **How the code works**—hooking into Pingora’s module system to handle gRPC-web.  
2. **Where to point the upstream**—i.e., the actual gRPC server address.  
3. **Modifying the code** to link it with another test server (like one you’ve built in an earlier example).

---

## Table of Contents

1. [Introduction](#introduction)  
2. [Code Overview](#code-overview)  
   - [Imports & Setup](#imports--setup)  
   - [`GrpcWebBridgeProxy` Struct](#grpcwebbridgeproxy-struct)  
     1. [Module Initialization](#1-module-initialization)  
     2. [Early Request Filter](#2-early-request-filter)  
     3. [Upstream Peer](#3-upstream-peer)  
   - [Main Function & Server Setup](#main-function--server-setup)  
3. [Connecting to a Test gRPC Server](#connecting-to-a-test-grpc-server)  
4. [Modifying for Other Upstreams or Local Test Servers](#modifying-for-other-upstreams-or-local-test-servers)  
5. [Complete Example Code](#complete-example-code)  
6. [Running & Testing](#running--testing)  

---

## Introduction

**gRPC-web** is a variant of gRPC protocol that works in modern browsers. However, your backend might only speak **standard gRPC**. This code uses Pingora to:

- Accept **gRPC-web** requests from the client.  
- Use the **`GrpcWebBridge`** module to unwrap the gRPC-web request format into a standard gRPC request.  
- Forward it to the real gRPC server (e.g., `1.1.1.1:443`).  
- Convert the gRPC response back into **gRPC-web** format before returning to the browser.

---

## Code Overview

Below is a step-by-step guide to the relevant parts of the code.

### Imports & Setup

```rust
use async_trait::async_trait;

use pingora_core::server::Server;
use pingora_core::upstreams::peer::HttpPeer;
use pingora_core::Result;
use pingora_core::{
    modules::http::{
        grpc_web::{GrpcWeb, GrpcWebBridge},
        HttpModules,
    },
};
use pingora_proxy::{ProxyHttp, Session};
```

Key points:

- **`grpc_web::{GrpcWeb, GrpcWebBridge}`**: The Pingora module for bridging gRPC-web to standard gRPC.
- **`ProxyHttp`**: The main trait we implement to define our custom proxy logic.
- **`HttpModules`**: A registry where we attach our modules (like `GrpcWeb`).

### `GrpcWebBridgeProxy` Struct

```rust
pub struct GrpcWebBridgeProxy;
```

A simple unit struct that implements `ProxyHttp`.

#### 1. Module Initialization

```rust
fn init_downstream_modules(&self, modules: &mut HttpModules) {
    // Add the gRPC web module
    modules.add_module(Box::new(GrpcWeb))
}
```

- **`init_downstream_modules`** is called **once** when the server starts.  
- We add the **`GrpcWeb`** module, which allows the proxy to parse gRPC-web requests and wrap them in a **`GrpcWebBridge`** context.

#### 2. Early Request Filter

```rust
async fn early_request_filter(
    &self,
    session: &mut Session,
    _ctx: &mut Self::CTX,
) -> Result<()> {
    let grpc = session
        .downstream_modules_ctx
        .get_mut::<GrpcWebBridge>()
        .expect("GrpcWebBridge module added");

    // Initialize gRPC module for this request
    grpc.init();
    Ok(())
}
```

- **`early_request_filter`** is called before other filters.  
- We grab the **`GrpcWebBridge`** context (inserted by the `GrpcWeb` module) and call `init()` on it. This sets up the bridging logic for this individual request.

#### 3. Upstream Peer

```rust
async fn upstream_peer(
    &self,
    _session: &mut Session,
    _ctx: &mut Self::CTX,
) -> Result<Box<HttpPeer>> {
    let grpc_peer = Box::new(HttpPeer::new(
        ("1.1.1.1", 443),
        true,
        "one.one.one.one".to_string(),
    ));
    Ok(grpc_peer)
}
```

- Specifies **where** to forward the request—here, `"1.1.1.1:443"`.  
- The second parameter (`true`) indicates **TLS**, and `"one.one.one.one"` is the **SNI** hostname.  
- You would replace this with the actual address of your **gRPC server**.

---

## Main Function & Server Setup

```rust
fn main() {
    env_logger::init();

    // Create server without command line arguments
    let mut my_server = Server::new(None).unwrap();
    my_server.bootstrap();

    let mut my_proxy =
        pingora_proxy::http_proxy_service(&my_server.configuration, GrpcWebBridgeProxy);
    my_proxy.add_tcp("0.0.0.0:6194");

    my_server.add_service(my_proxy);
    my_server.run_forever();
}
```

1. **Create the Pingora `Server`** and call `bootstrap()`.  
2. **Create** an HTTP proxy service with `GrpcWebBridgeProxy`.  
3. **Listen** on port `6194`.  
4. **Add** the service to the server and call `run_forever()` to block.

At this point, the proxy is listening on `0.0.0.0:6194`, expecting gRPC-web requests.

---

## Connecting to a Test gRPC Server

If you have a local gRPC server running (for example, **`test_server`** from a previous example or a separate codebase), you can **change** the `upstream_peer` method to point to it. For instance:

```rust
async fn upstream_peer(&self, _session: &mut Session, _ctx: &mut Self::CTX) -> Result<Box<HttpPeer>> {
    // If your test gRPC server is on localhost:50051
    let grpc_peer = Box::new(HttpPeer::new(
        ("127.0.0.1", 50051),
        false, // false if your server doesn't use TLS
        "localhost".to_string(),
    ));
    Ok(grpc_peer)
}
```

- **`("127.0.0.1", 50051)`**: Adjust host/port to match your local server.  
- Set the second argument to `false` if your server is plain HTTP/2 (non-TLS).  
- Provide an SNI hostname if TLS is `true`; otherwise, it can be a placeholder.

When the code references **`test_server`**, make sure that **test_server** is indeed **a gRPC server** or gRPC-enabled. If it’s a plain HTTP server, bridging gRPC-web traffic to it won’t work as intended, because the bridging module expects standard gRPC frames.

---

## Modifying for Other Upstreams or Local Test Servers

1. **Different Remote gRPC**: If you have a remote gRPC server at `grpc.example.com:443`, swap in those details:
   ```rust
   let grpc_peer = Box::new(HttpPeer::new(
       ("grpc.example.com", 443),
       true,
       "grpc.example.com".to_string(),
   ));
   ```
2. **Local Environments**: If your server doesn’t use TLS, set the second parameter to `false`:
   ```rust
   let grpc_peer = Box::new(HttpPeer::new(
       ("127.0.0.1", 50051),
       false,
       "127.0.0.1".to_string(),
   ));
   ```
3. **Custom bridging**: If you want to do something else inside `early_request_filter`, like logging or extra header checks, you can do so before calling `grpc.init()`.

---

## Complete Example Code

```rust
use async_trait::async_trait;

use pingora_core::server::Server;
use pingora_core::upstreams::peer::HttpPeer;
use pingora_core::Result;
use pingora_core::{
    modules::http::{
        grpc_web::{GrpcWeb, GrpcWebBridge},
        HttpModules,
    },
};
use pingora_proxy::{ProxyHttp, Session};

/// This example shows how to use the gRPC-web bridge module

pub struct GrpcWebBridgeProxy;

#[async_trait]
impl ProxyHttp for GrpcWebBridgeProxy {
    type CTX = ();

    fn new_ctx(&self) -> Self::CTX {
        ()
    }

    fn init_downstream_modules(&self, modules: &mut HttpModules) {
        // Add the gRPC web module
        modules.add_module(Box::new(GrpcWeb))
    }

    async fn early_request_filter(
        &self,
        session: &mut Session,
        _ctx: &mut Self::CTX,
    ) -> Result<()> {
        let grpc = session
            .downstream_modules_ctx
            .get_mut::<GrpcWebBridge>()
            .expect("GrpcWebBridge module added");

        // Initialize gRPC module for this request
        grpc.init();
        Ok(())
    }

    async fn upstream_peer(
        &self,
        _session: &mut Session,
        _ctx: &mut Self::CTX,
    ) -> Result<Box<HttpPeer>> {
        // Replace this address with your gRPC server
        let grpc_peer = Box::new(HttpPeer::new(
            ("1.1.1.1", 443),
            true,
            "one.one.one.one".to_string(),
        ));
        Ok(grpc_peer)
    }
}

fn main() {
    env_logger::init();

    // Create server without command line arguments
    let mut my_server = Server::new(None).unwrap();
    my_server.bootstrap();

    let mut my_proxy =
        pingora_proxy::http_proxy_service(&my_server.configuration, GrpcWebBridgeProxy);
    my_proxy.add_tcp("0.0.0.0:6194");

    my_server.add_service(my_proxy);
    my_server.run_forever();
}
```

---

## Running & Testing

1. **Start the Proxy**:
   ```bash
   cargo run
   ```
   The proxy will listen on `0.0.0.0:6194`.

2. **Configure Your Front-End**:
   - Set your browser client or gRPC-web client to send requests to `http://<proxy-host>:6194`.
   - The `GrpcWeb` module handles the translation from gRPC-web to standard gRPC.

3. **Check Upstream**:
   - If your local server runs at `127.0.0.1:50051`, switch the code in `upstream_peer` accordingly.
   - Ensure your real gRPC server is running and accepts requests.

4. **Integration with a Test Server**:
   - If you previously wrote a **gRPC test server** in Rust or another language, just ensure that it’s listening on a known host/port and that you adjust the `HttpPeer::new(...)` arguments here. That will let the bridging proxy connect to the test server seamlessly.

You can now send gRPC-web requests to port **6194**, and they will be bridged to your gRPC server. If everything is set up correctly, you’ll see successful traffic going through.

**That’s it!** You have a working gRPC-web bridge in Rust using Pingora, ready to adapt to your real-world gRPC environment.
