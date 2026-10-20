# Tutorial: Transforming JSON Responses to YAML via Pingora Proxy

This code demonstrates a **Pingora**-based Rust proxy that **fetches JSON** from an upstream server, **transforms** it into **YAML**, and returns the YAML body to the client. Along the way, you’ll see how to:

1. Implement the `ProxyHttp` trait to define custom request/response handling.  
2. **Rewrite** the response body by buffering data, deserializing JSON, and then serializing it to YAML.  
3. Adjust headers to ensure the client accepts the modified body correctly (e.g., removing `Content-Length` and setting `Transfer-Encoding: chunked`).

---

## Table of Contents

1. [Introduction](#introduction)  
2. [Code Overview](#code-overview)  
   - [Imports & Dependencies](#imports--dependencies)  
   - [Structs: `Json2Yaml` & `MyCtx`](#structs-json2yaml--myctx)  
   - [Implementing `ProxyHttp` for `Json2Yaml`](#implementing-proxyhttp-for-json2yaml)  
     1. [Context & Upstream Selection](#1-context--upstream-selection)  
     2. [Upstream Request Filter](#2-upstream-request-filter)  
     3. [Response Filter (Headers)](#3-response-filter-headers)  
     4. [Response Body Filter (Transform JSON to YAML)](#4-response-body-filter-transform-json-to-yaml)  
   - [`main` Function & Running the Server](#main-function--running-the-server)  
3. [Testing the Proxy](#testing-the-proxy)  
4. [Modifying the Code](#modifying-the-code)  
   - [Different Upstream Host](#different-upstream-host)  
   - [Other Transformations (XML, CSV, etc.)](#other-transformations-xml-csv-etc)  
   - [Body Streaming vs. Buffering](#body-streaming-vs-buffering)  
5. [Complete Example Code](#complete-example-code)  
6. [Conclusion](#conclusion)

---

## Introduction

**Pingora** is a Rust-based framework for building custom proxies. In this snippet, we use Pingora to:

- **Connect** to an HTTP endpoint (in this case, `ip.jsontest.com` which returns a JSON containing your IP).  
- **Receive** the JSON response.  
- **Parse** it into a Rust struct (`Resp`).  
- **Serialize** it as **YAML**.  
- **Send** the transformed YAML back to the client.

---

## Code Overview

### Imports & Dependencies

```rust
use async_trait::async_trait;
use bytes::Bytes;
use serde::{Deserialize, Serialize};

use pingora_core::server::Server;
use pingora_core::upstreams::peer::HttpPeer;
use pingora_core::Result;
use pingora_http::{RequestHeader, ResponseHeader};
use pingora_proxy::{ProxyHttp, Session};
```

Key libraries:

- **`serde`, `serde_json`, `serde_yaml`** (via `serde` and `serde_yaml` crates) for serialization and deserialization.  
- **`bytes::Bytes`** to handle chunked data in memory.  
- **`Pingora`** components for proxy logic, servers, and HTTP peers.

### Structs: `Json2Yaml` & `MyCtx`

```rust
pub struct Json2Yaml {
    addr: std::net::SocketAddr,
}

pub struct MyCtx {
    buffer: Vec<u8>,
}
```

1. **`Json2Yaml`**: Holds a `std::net::SocketAddr` for the upstream server. We’ll implement `ProxyHttp` on this struct.  
2. **`MyCtx`**: A per-connection (or per-request) context storing a buffer `Vec<u8>` where we accumulate the response body before converting to YAML.

### Implementing `ProxyHttp` for `Json2Yaml`

```rust
#[async_trait]
impl ProxyHttp for Json2Yaml {
    type CTX = MyCtx;

    fn new_ctx(&self) -> Self::CTX {
        MyCtx { buffer: vec![] }
    }
    ...
}
```

#### 1. Context & Upstream Selection

```rust
async fn upstream_peer(
    &self,
    _session: &mut Session,
    _ctx: &mut Self::CTX,
) -> Result<Box<HttpPeer>> {
    let peer = Box::new(HttpPeer::new(self.addr, false, HOST.to_owned()));
    Ok(peer)
}
```

- **`upstream_peer`**: Chooses the upstream server.  
  - `false` => Not using TLS (it’s plain HTTP).  
  - `HOST` is `"ip.jsontest.com"`, and `addr` is `142.251.2.121:80` in the example.  
- You can change these to point to a different server.

#### 2. Upstream Request Filter

```rust
async fn upstream_request_filter(
    &self,
    _session: &mut Session,
    upstream_request: &mut RequestHeader,
    _ctx: &mut Self::CTX,
) -> Result<()> {
    upstream_request.insert_header("Host", HOST).unwrap();
    Ok(())
}
```

- Before sending the request upstream, we set the `Host` header to match `ip.jsontest.com`.  
- This is required if the remote server relies on the Host header to dispatch requests.

#### 3. Response Filter (Headers)

```rust
async fn response_filter(
    &self,
    _session: &mut Session,
    upstream_response: &mut ResponseHeader,
    _ctx: &mut Self::CTX,
) -> Result<()> {
    // Remove Content-Length because the size of the new body is unknown
    upstream_response.remove_header("Content-Length");
    upstream_response.insert_header("Transfer-Encoding", "chunked").unwrap();
    Ok(())
}
```

- We remove the `Content-Length` because we’re changing the body size.  
- Then, we set **`Transfer-Encoding: chunked`** so the client can read data in streaming chunks rather than expecting a fixed length.

#### 4. Response Body Filter (Transform JSON to YAML)

```rust
fn response_body_filter(
    &self,
    _session: &mut Session,
    body: &mut Option<Bytes>,
    end_of_stream: bool,
    ctx: &mut Self::CTX,
) -> Result<Option<std::time::Duration>> {
    // Accumulate data in the context buffer
    if let Some(b) = body.take() {
        ctx.buffer.extend_from_slice(&b);
    }

    if end_of_stream {
        // The upstream finished sending data
        let json_body: Resp = serde_json::from_slice(&ctx.buffer).unwrap();
        let yaml_body = serde_yaml::to_string(&json_body).unwrap();

        // Replace the body with the new YAML data
        *body = Some(Bytes::copy_from_slice(yaml_body.as_bytes()));
    }

    Ok(None)
}
```

- **Buffering**: We store the body chunks in `ctx.buffer` until the entire response is received (`end_of_stream == true`).  
- **Transform**: Once we have the full JSON body, we deserialize into `Resp` and then serialize to YAML.  
- Finally, we replace the body with the new YAML-encoded data.

---

## `main` Function & Running the Server

```rust
fn main() {
    env_logger::init();

    let mut my_server = Server::new(None).unwrap();
    my_server.bootstrap();

    let mut my_proxy = pingora_proxy::http_proxy_service(
        &my_server.configuration,
        Json2Yaml {
            addr: "142.251.2.121:80".parse().unwrap(),
        },
    );
    my_proxy.add_tcp("127.0.0.1:6191");

    my_server.add_service(my_proxy);
    my_server.run_forever();
}
```

1. **`Server::new(None)`** creates a Pingora server.  
2. **`bootstrap()`** readies internal tasks.  
3. **`Json2Yaml`** instance: Hard-coded to `142.251.2.121:80`, which is one of Google IPs for `ip.jsontest.com`.  
4. **`my_proxy.add_tcp("127.0.0.1:6191")`** => The proxy listens on port **6191** for incoming connections.  
5. **`my_server.run_forever()`** => Start the event loop and serve until interrupted.

---

## Testing the Proxy

After compiling and running, test by sending a request to `127.0.0.1:6191`:

```bash
curl -v http://127.0.0.1:6191/
```

1. The proxy connects to `ip.jsontest.com` at `142.251.2.121:80`.  
2. It receives a JSON body similar to:
   ```json
   {
       "ip": "123.123.123.123"
   }
   ```
3. The proxy transforms this JSON into YAML, e.g.:
   ```yaml
   ip: 123.123.123.123
   ```
4. The client sees the **YAML** response instead of JSON.

---

## Modifying the Code

### Different Upstream Host

- Change `HOST` from `"ip.jsontest.com"` to your desired server domain.  
- Adjust `addr` to the correct IP/port, or replace `HttpPeer::new(self.addr, false, HOST.to_owned())` with DNS resolution logic.

### Other Transformations (XML, CSV, etc.)

- Instead of `serde_yaml`, you could parse the JSON data into a struct, then serialize to **XML** (using `serde_xml_rs`) or **CSV** (using `csv`).  
- The principle remains the same: **accumulate** body data, **convert** it, then **replace** the final body.

### Body Streaming vs. Buffering

- This example **buffers** the entire body to parse JSON in one go. For large bodies, you might prefer a streaming parser approach.  
- You’d parse each chunk as it arrives, but that may get more complex.

---

## Complete Example Code

```rust
use async_trait::async_trait;
use bytes::Bytes;
use serde::{Deserialize, Serialize};

use pingora_core::server::Server;
use pingora_core::upstreams::peer::HttpPeer;
use pingora_core::Result;
use pingora_http::{RequestHeader, ResponseHeader};
use pingora_proxy::{ProxyHttp, Session};

const HOST: &str = "ip.jsontest.com";

#[derive(Serialize, Deserialize)]
pub struct Resp {
    ip: String,
}

pub struct Json2Yaml {
    addr: std::net::SocketAddr,
}

pub struct MyCtx {
    buffer: Vec<u8>,
}

#[async_trait]
impl ProxyHttp for Json2Yaml {
    type CTX = MyCtx;

    fn new_ctx(&self) -> Self::CTX {
        MyCtx { buffer: vec![] }
    }

    async fn upstream_peer(
        &self,
        _session: &mut Session,
        _ctx: &mut Self::CTX,
    ) -> Result<Box<HttpPeer>> {
        let peer = Box::new(HttpPeer::new(self.addr, false, HOST.to_owned()));
        Ok(peer)
    }

    async fn upstream_request_filter(
        &self,
        _session: &mut Session,
        upstream_request: &mut RequestHeader,
        _ctx: &mut Self::CTX,
    ) -> Result<()> {
        upstream_request
            .insert_header("Host", HOST)
            .unwrap();
        Ok(())
    }

    async fn response_filter(
        &self,
        _session: &mut Session,
        upstream_response: &mut ResponseHeader,
        _ctx: &mut Self::CTX,
    ) -> Result<()> {
        // Remove Content-Length because the size of the new body is unknown
        upstream_response.remove_header("Content-Length");
        upstream_response
            .insert_header("Transfer-Encoding", "chunked")
            .unwrap();
        Ok(())
    }

    fn response_body_filter(
        &self,
        _session: &mut Session,
        body: &mut Option<Bytes>,
        end_of_stream: bool,
        ctx: &mut Self::CTX,
    ) -> Result<Option<std::time::Duration>>
    where
        Self::CTX: Send + Sync,
    {
        // Buffer the data
        if let Some(b) = body.take() {
            ctx.buffer.extend_from_slice(&b);
        }

        if end_of_stream {
            // This is the last chunk; we can process the data now
            let json_body: Resp = serde_json::from_slice(&ctx.buffer).unwrap();
            let yaml_body = serde_yaml::to_string(&json_body).unwrap();
            *body = Some(Bytes::copy_from_slice(yaml_body.as_bytes()));
        }

        Ok(None)
    }
}

fn main() {
    env_logger::init();

    let mut my_server = Server::new(None).unwrap();
    my_server.bootstrap();

    let mut my_proxy = pingora_proxy::http_proxy_service(
        &my_server.configuration,
        Json2Yaml {
            addr: "142.251.2.121:80".parse().unwrap(),
        },
    );

    my_proxy.add_tcp("127.0.0.1:6191");

    my_server.add_service(my_proxy);
    my_server.run_forever();
}
```

---

## Conclusion

This **Json2Yaml** example highlights how Pingora allows you to **reshape** the response body in real-time—useful for tasks like:

- **Protocol translation** (JSON -> YAML, or even gRPC-web -> gRPC).  
- **Content filtering or injection** (e.g., removing sensitive data).  
- **Logging or analytics** (inspecting the body before proxying).

With a few simple steps—removing the `Content-Length`, setting chunked encoding, and hooking into Pingora’s `response_body_filter`—you can achieve complex transformations in a lightweight, asynchronous Rust proxy.
