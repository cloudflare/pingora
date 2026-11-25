# Tutorial: Adding a Custom ACL Module to a Pingora-based HTTP Proxy in Rust

This tutorial demonstrates how to implement a **custom ACL (Access Control List)** module in a Pingora-based Rust HTTP proxy. Using Pingora’s **module system**, you can attach third-party or custom logic to inspect and modify requests before they are forwarded to an upstream.

Below, we’ll break down each part of the code, show how the module enforces a simple **`Authorization: basic ...`** check, and explain how you can adapt it for your own needs.

---

## Table of Contents

1. [Introduction](#introduction)  
2. [Code Overview](#code-overview)  
   - [Imports & Dependencies](#imports--dependencies)  
   - [Module Basics](#module-basics)  
   - [Custom ACL Module (`my_acl`)](#custom-acl-module-my_acl)  
     1. [Context Struct (`MyAclCtx`)](#1-context-struct-myclctx)  
     2. [Module Implementation (`HttpModule`)](#2-module-implementation-httpmodule)  
     3. [Module Builder (`MyAcl`)](#3-module-builder-myacl)  
   - [`MyProxy` Struct](#myproxy-struct)  
     1. [Attaching Modules](#1-attaching-modules)  
     2. [Upstream Peer Selection](#2-upstream-peer-selection)  
   - [Main Function & Server Setup](#main-function--server-setup)  
3. [How It Works At Runtime](#how-it-works-at-runtime)  
4. [Testing the Example](#testing-the-example)  
5. [Extending or Modifying the Code](#extending-or-modifying-the-code)  
6. [Complete Example Code](#complete-example-code)  

---

## Introduction

In Pingora, **modules** are small, pluggable pieces of logic that can inspect and transform HTTP requests and responses. For example, you can create a module that:

- **Enforces an authentication policy**  
- **Adds or removes certain headers**  
- **Implements rate limiting**  

In this example, our module checks whether the **`Authorization`** header matches a given credential. If it doesn’t match, the request is rejected with a `403 Forbidden` response.

---

## Code Overview

Below is an annotated version of our code.

### Imports & Dependencies

```rust
use async_trait::async_trait;

use pingora_core::modules::http::HttpModules;
use pingora_core::server::Server;
use pingora_core::upstreams::peer::HttpPeer;
use pingora_core::Result;
use pingora_http::RequestHeader;
use pingora_proxy::{ProxyHttp, Session};
use std::any::Any;
```

- **`async_trait`**: Allows async functions in trait implementations.  
- **`pingora_core::modules::http`**: Where Pingora’s HTTP module interfaces live.  
- **`Server`, `HttpPeer`, `Result`**: Core Pingora types for setting up a server, defining upstreams, and handling results.  
- **`pingora_http::RequestHeader`**: Provides access to incoming HTTP request headers.  
- **`pingora_proxy::{ProxyHttp, Session}`**: The main Pingora traits that let you define a proxy’s logic and interact with a request “session.”

### Module Basics

A **module** in Pingora is an object that implements certain hooks—like `request_header_filter`—allowing you to inspect or modify requests. You can attach these modules to a proxy at startup. Each incoming request triggers a new **module context** object.

### Custom ACL Module (`my_acl`)

We define our custom module logic in the `my_acl` submodule.

#### 1. Context Struct (`MyAclCtx`)

```rust
pub struct MyAclCtx {
    credential_header: String,
}
```

- **`MyAclCtx`**: Holds the per-request context data for our ACL check. In this example, we store a string (`credential_header`) that the request must match in its `Authorization` header.

#### 2. Module Implementation (`HttpModule`)

```rust
#[async_trait]
impl HttpModule for MyAclCtx {
    async fn request_header_filter(&mut self, req: &mut RequestHeader) -> Result<()> {
        if let Some(auth) = req.headers.get("Authorization") {
            if auth.as_bytes() != self.credential_header.as_bytes() {
                return Error::e_explain(
                    ErrorType::HTTPStatus(403),
                    "Auth failed, credential mismatch",
                );
            } else {
                Ok(())
            }
        } else {
            Error::e_explain(
                ErrorType::HTTPStatus(403),
                "Auth failed, no auth header",
            )
        }
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}
```

- **`HttpModule`** trait: Defines hooks like `request_header_filter`, `request_body_filter`, `response_header_filter`, etc. (We’re only using `request_header_filter` here.)  
- If the `Authorization` header doesn’t match our `credential_header` string, we immediately return an error of type `ErrorType::HTTPStatus(403)`, producing a **403 Forbidden** HTTP response.  
- **`as_any`** methods: Boilerplate for downcasting; required by the Pingora module framework.

#### 3. Module Builder (`MyAcl`)

```rust
pub struct MyAcl {
    pub credential: String,
}

impl HttpModuleBuilder for MyAcl {
    fn init(&self) -> Module {
        Box::new(MyAclCtx {
            credential_header: format!("basic {}", self.credential),
        })
    }
}
```

- **`MyAcl`**: The **singleton** struct that lives for the server’s lifetime. It holds data needed by the module context, such as the `credential`.  
- **`HttpModuleBuilder`** trait: Lets Pingora create a fresh **module context** for each request. Here, `init` returns a new `MyAclCtx`. We format `credential_header` to `"basic <credential>"`, so we can compare it against the request’s `Authorization` header.

---

### `MyProxy` Struct

```rust
pub struct MyProxy;
```

A simple struct implementing `ProxyHttp`—the primary trait for defining a Pingora-based proxy.

#### 1. Attaching Modules

```rust
fn init_downstream_modules(&self, modules: &mut HttpModules) {
    modules.add_module(Box::new(my_acl::MyAcl {
        credential: "testcode".into(),
    }));
}
```

- **`init_downstream_modules`** is called **once** when the server starts.  
- We create an instance of `MyAcl` with the credential `"testcode"`.  
- Then we add this module to the `HttpModules` list. When a request arrives, Pingora will call the `init()` method on each module builder, producing a context for the request.

#### 2. Upstream Peer Selection

```rust
async fn upstream_peer(
    &self,
    _session: &mut Session,
    _ctx: &mut Self::CTX,
) -> Result<Box<HttpPeer>> {
    let peer = Box::new(HttpPeer::new(
        ("1.1.1.1", 443),
        true,
        "one.one.one.one".to_string(),
    ));
    Ok(peer)
}
```

This is where we define the **upstream**. All requests that pass our ACL check will be forwarded to **`1.1.1.1`** on port **443** with TLS enabled (SNI host: `"one.one.one.one"`). You can change this to suit your environment (e.g. `"127.0.0.1:3000"` for a local server, or a load balancer, etc.).

---

## Main Function & Server Setup

```rust
fn main() {
    env_logger::init();

    let mut my_server = Server::new(None).unwrap();
    my_server.bootstrap();

    let mut my_proxy = pingora_proxy::http_proxy_service(&my_server.configuration, MyProxy);
    my_proxy.add_tcp("0.0.0.0:6193");

    my_server.add_service(my_proxy);
    my_server.run_forever();
}
```

1. **`Server::new(None)`**: Creates a Pingora server with no command-line options.  
2. **`bootstrap()`**: Prepares internal tasks or resources for the server.  
3. **`pingora_proxy::http_proxy_service(...)`**: Builds an HTTP proxy service using `MyProxy`.  
4. **`add_tcp("0.0.0.0:6193")`**: Binds the proxy to listen on port **6193**.  
5. **`my_server.add_service(my_proxy)`**: Registers our proxy as a service within the Pingora server.  
6. **`run_forever()`**: Starts the event loop and keeps the server running.

---

## How It Works At Runtime

1. **Server Start**: When `main()` runs, it creates the server, boots it, and attaches the `MyProxy` service.  
2. **Module Registration**: `MyProxy::init_downstream_modules` is called; we register our `MyAcl` module with the server.  
3. **Incoming Request**: For each request, Pingora will:  
   - Call `MyAcl::init()` to create a `MyAclCtx`.  
   - Run the module’s `request_header_filter` method. If it sees that `"Authorization: basic testcode"` is **not** present, it returns a 403.  
   - If the request passes the check, control proceeds to `MyProxy::upstream_peer`.  
   - The request is forwarded to `1.1.1.1:443`.  
4. **Response**: The response from the upstream (1.1.1.1) is forwarded back to the client.

---

## Testing the Example

Compile and run this example:

```bash
RUST_LOG=INFO cargo run
```

(Can also use `cargo run --example use_module` if you placed it in an `examples` folder, depending on your setup.)

Then, test via `curl`:

1. **No `Authorization` header** -> Expect `403 Forbidden`:
   ```bash
   curl -v http://127.0.0.1:6193 -H "Host: one.one.one.one"
   ```
2. **Correct `Authorization`** -> Request passes and is proxied upstream:
   ```bash
   curl -v http://127.0.0.1:6193 -H "Host: one.one.one.one" -H "Authorization: basic testcode"
   ```
3. **Wrong `Authorization`** -> Again, `403 Forbidden`:
   ```bash
   curl -v http://127.0.0.1:6193 -H "Host: one.one.one.one" -H "Authorization: basic wrong"
   ```

---

## Extending or Modifying the Code

- **Change the credential**: In `init_downstream_modules`, set your own string or generate it dynamically.  
- **Use a more complex auth scheme**: Instead of checking `basic testcode`, parse JWT tokens, check a username/password store, or integrate with OAuth.  
- **Multiple modules**: You can add more than one module. For instance, you could have an ACL module plus a logging or caching module.  
- **Different upstream**: In `upstream_peer`, direct traffic to any HTTP or HTTPS server by changing the IP/port and the SNI host if TLS is enabled.

---

## Complete Example Code

Below is the entire code for convenience:

```rust
use async_trait::async_trait;

use pingora_core::modules::http::HttpModules;
use pingora_core::server::Server;
use pingora_core::upstreams::peer::HttpPeer;
use pingora_core::Result;
use pingora_http::RequestHeader;
use pingora_proxy::{ProxyHttp, Session};
use std::any::Any;

/// This example shows how to build and import 3rd party modules

/// A simple ACL to check "Authorization: basic $credential" header
mod my_acl {
    use super::*;
    use pingora_core::modules::http::{HttpModule, HttpModuleBuilder, Module};
    use pingora_error::{Error, ErrorType}; // Import Error and ErrorType here

    // This is the struct for per-request module context
    pub struct MyAclCtx {
        credential_header: String,
    }

    // Implement how the module would consume and/or modify request and/or response
    #[async_trait]
    impl HttpModule for MyAclCtx {
        async fn request_header_filter(&mut self, req: &mut RequestHeader) -> Result<()> {
            if let Some(auth) = req.headers.get("Authorization") {
                if auth.as_bytes() != self.credential_header.as_bytes() {
                    return Error::e_explain(
                        ErrorType::HTTPStatus(403),
                        "Auth failed, credential mismatch",
                    );
                } else {
                    Ok(())
                }
            } else {
                Error::e_explain(
                    ErrorType::HTTPStatus(403),
                    "Auth failed, no auth header",
                )
            }
        }

        // Boilerplate code for all modules
        fn as_any(&self) -> &dyn Any {
            self
        }
        fn as_any_mut(&mut self) -> &mut dyn Any {
            self
        }
    }

    // This is the singleton object which will be attached to the server
    pub struct MyAcl {
        pub credential: String,
    }

    impl HttpModuleBuilder for MyAcl {
        // This function defines how to create each context. This function is called when a new request arrives
        fn init(&self) -> Module {
            Box::new(MyAclCtx {
                // Make it easier to compare header
                credential_header: format!("basic {}", self.credential),
            })
        }
    }
}

pub struct MyProxy;

#[async_trait]
impl ProxyHttp for MyProxy {
    type CTX = ();

    fn new_ctx(&self) -> Self::CTX {
        ()
    }

    // This function is only called once when the server starts
    fn init_downstream_modules(&self, modules: &mut HttpModules) {
        // Add the module to MyProxy
        modules.add_module(Box::new(my_acl::MyAcl {
            credential: "testcode".into(),
        }));
    }

    async fn upstream_peer(
        &self,
        _session: &mut Session,
        _ctx: &mut Self::CTX,
    ) -> Result<Box<HttpPeer>> {
        let peer = Box::new(HttpPeer::new(
            ("1.1.1.1", 443),
            true,
            "one.one.one.one".to_string(),
        ));
        Ok(peer)
    }
}

fn main() {
    env_logger::init();

    // Create the server without command line arguments
    let mut my_server = Server::new(None).unwrap();
    my_server.bootstrap();

    let mut my_proxy = pingora_proxy::http_proxy_service(&my_server.configuration, MyProxy);
    my_proxy.add_tcp("0.0.0.0:6193");

    my_server.add_service(my_proxy);
    my_server.run_forever();
}
```

Use the instructions in the comments to **test** it with `curl` commands, verifying you get `403 Forbidden` when you don’t meet the `Authorization` requirement.

---

**Happy coding!** Feel free to extend this example to integrate more complex authentication, logging, or transformations in your custom Pingora modules.
