# Tutorial: Routing Requests Across Multiple Load Balancer Clusters with Pingora

This example demonstrates how to set up **multiple load-balancing clusters** in [Pingora](https://docs.rs/pingora-core/latest/pingora_core/) and then **route** incoming requests between them based on the request path. Specifically:

- Requests whose paths start with **`/one/`** go to **`cluster_one`**.  
- All other requests go to **`cluster_two`**.  

Each cluster has its own set of upstreams and **TCP health checks** to dynamically remove unhealthy servers from rotation.

---

## Table of Contents

1. [Introduction](#introduction)  
2. [Code Overview](#code-overview)  
   - [Imports & Setup](#imports--setup)  
   - [`Router` Struct & `ProxyHttp` Implementation](#router-struct--proxyhttp-implementation)  
     1. [Choosing the Right Cluster](#1-choosing-the-right-cluster)  
   - [Building Clusters with `build_cluster_service`](#building-clusters-with-build_cluster_service)  
   - [Main Function & Server Setup](#main-function--server-setup)  
3. [Examples of Modifications](#examples-of-modifications)  
   1. [Different Request Routing Logic](#1-different-request-routing-logic)  
   2. [Custom Health Checks](#2-custom-health-checks)  
   3. [TLS or Plain HTTP Upstreams](#3-tls-or-plain-http-upstreams)  
   4. [Linking This to a Local Test Server](#4-linking-this-to-a-local-test-server)  
4. [Complete Example Code](#complete-example-code)  
5. [Testing & Observing Behavior](#testing--observing-behavior)  
6. [Conclusion](#conclusion)

---

## Introduction

By **composing** multiple load-balanced clusters in Pingora, you can route traffic to different backend pools depending on HTTP properties—like path, headers, or methods. This is useful for splitting traffic between microservices, regions, or canary vs. production clusters.

---

## Code Overview

### Imports & Setup

```rust
use async_trait::async_trait;
use std::sync::Arc;

use pingora_core::{prelude::*, services::background::GenBackgroundService};
use pingora_core::upstreams::peer::HttpPeer;
use pingora_load_balancing::{
    health_check::TcpHealthCheck,
    selection::{BackendIter, BackendSelection, RoundRobin},
    LoadBalancer,
};
use pingora_proxy::{http_proxy_service, ProxyHttp, Session};
```

- **`RoundRobin`**: Our load-balancing strategy.  
- **`TcpHealthCheck`**: A basic health check that attempts a TCP connection.  
- **`LoadBalancer`**: The structure that manages a set of backends.  
- **`GenBackgroundService`**: Lets us run these health checks in the background.

### `Router` Struct & `ProxyHttp` Implementation

```rust
struct Router {
    cluster_one: Arc<LoadBalancer<RoundRobin>>,
    cluster_two: Arc<LoadBalancer<RoundRobin>>,
}
```

- We store references to **two** separate `LoadBalancer<RoundRobin>` instances in `cluster_one` and `cluster_two`.

#### 1. Choosing the Right Cluster

```rust
#[async_trait]
impl ProxyHttp for Router {
    type CTX = ();

    fn new_ctx(&self) -> Self::CTX {
        ()
    }

    async fn upstream_peer(
        &self,
        session: &mut Session,
        _ctx: &mut Self::CTX,
    ) -> Result<Box<HttpPeer>> {
        // Determine LB cluster based on request URI
        let cluster = if session.req_header().uri.path().starts_with("/one/") {
            &self.cluster_one
        } else {
            &self.cluster_two
        };

        let upstream = cluster.select(b"", 256).unwrap();
        println!("upstream peer is: {:?}", upstream);

        let peer = Box::new(HttpPeer::new(
            upstream,
            true,                        // TLS = true
            "one.one.one.one".to_string() // SNI host
        ));
        Ok(peer)
    }
}
```

- **`upstream_peer`** picks the **cluster** based on whether the request path starts with `"/one/"`. Otherwise, it uses the second cluster.
- `cluster.select(b"", 256)` chooses an upstream from the load balancer, skipping unhealthy servers.  
- We create an `HttpPeer` set to **TLS = `true`** with the SNI host `"one.one.one.one"` (placeholder example).

### Building Clusters with `build_cluster_service`

```rust
fn build_cluster_service<S>(upstreams: &[&str]) -> GenBackgroundService<LoadBalancer<S>>
where
    S: BackendSelection + 'static,
    S::Iter: BackendIter,
{
    let mut cluster = LoadBalancer::try_from_iter(upstreams).unwrap();
    cluster.set_health_check(TcpHealthCheck::new());
    cluster.health_check_frequency = Some(std::time::Duration::from_secs(1));

    background_service("cluster health check", cluster)
}
```

1. We create a `LoadBalancer` from the provided upstream addresses (like `["1.1.1.1:443", "127.0.0.1:343"]`).  
2. Attach a **TCP health check** with `set_health_check`.  
3. Set the **`health_check_frequency`** to 1 second, meaning we test each backend’s health once per second.  
4. **`background_service(...)`** returns a `GenBackgroundService<LoadBalancer<S>>` that we’ll add to the Pingora server. This service runs health checks in the background.

### Main Function & Server Setup

```rust
fn main() {
    let mut my_server = Server::new(None).unwrap();
    my_server.bootstrap();

    // Build two clusters
    let cluster_one = build_cluster_service::<RoundRobin>(&["1.1.1.1:443", "127.0.0.1:343"]);
    let cluster_two = build_cluster_service::<RoundRobin>(&["1.0.0.1:443", "127.0.0.2:343"]);

    let router = Router {
        cluster_one: cluster_one.task(),
        cluster_two: cluster_two.task(),
    };

    let mut router_service = http_proxy_service(&my_server.configuration, router);
    router_service.add_tcp("0.0.0.0:6188");

    my_server.add_service(router_service);
    my_server.add_service(cluster_one);
    my_server.add_service(cluster_two);

    my_server.run_forever();
}
```

1. **`build_cluster_service`** creates two separate clusters:
   - **Cluster One**: `[ "1.1.1.1:443", "127.0.0.1:343" ]`  
   - **Cluster Two**: `[ "1.0.0.1:443", "127.0.0.2:343" ]`
2. Each cluster returns a background service. We attach them to the server so they can run health checks.  
3. We create a `Router` that references **both** clusters.  
4. **`router_service.add_tcp("0.0.0.0:6188")`** => The proxy listens on port 6188.  
5. Finally, **`my_server.run_forever()`** starts the server.

---

## Examples of Modifications

### 1. Different Request Routing Logic

- Instead of checking `.path().starts_with("/one/")`, you could:
  - Check a **header** or **query param**.
  - Look at the **HTTP method**.  
  - Use **regex** or more complex path matching (like `/api/v1/` vs `/api/v2/`).

### 2. Custom Health Checks

- By default, `TcpHealthCheck` just tests TCP connectivity.  
- Use an **HTTP** health check if you need to confirm an endpoint is returning `200 OK`.  
- Implement your own health check trait if your upstream requires a specialized approach.

### 3. TLS or Plain HTTP Upstreams

- In `HttpPeer::new(upstream, true, "one.one.one.one")`, the second param is `true` for **HTTPS**. Switch to `false` if your upstream is plain **HTTP**.  
- Adjust the SNI host accordingly.

### 4. Linking This to a Local Test Server

- If you have a local server listening on `127.0.0.1:3000`, change the upstream strings to `"127.0.0.1:3000"`.  
- You may also disable health checks or set `health_check_frequency` to a higher interval for testing.

---

## Complete Example Code

```rust
use async_trait::async_trait;
use std::sync::Arc;

use pingora_core::{prelude::*, services::background::GenBackgroundService};
use pingora_core::upstreams::peer::HttpPeer;
use pingora_load_balancing::{
    health_check::TcpHealthCheck,
    selection::{BackendIter, BackendSelection, RoundRobin},
    LoadBalancer,
};
use pingora_proxy::{http_proxy_service, ProxyHttp, Session};

struct Router {
    cluster_one: Arc<LoadBalancer<RoundRobin>>,
    cluster_two: Arc<LoadBalancer<RoundRobin>>,
}

#[async_trait]
impl ProxyHttp for Router {
    type CTX = ();

    fn new_ctx(&self) -> Self::CTX {
        ()
    }

    async fn upstream_peer(
        &self,
        session: &mut Session,
        _ctx: &mut Self::CTX,
    ) -> Result<Box<HttpPeer>> {
        let cluster = if session.req_header().uri.path().starts_with("/one/") {
            &self.cluster_one
        } else {
            &self.cluster_two
        };

        let upstream = cluster.select(b"", 256).unwrap();
        println!("upstream peer is: {:?}", upstream);

        let peer = Box::new(HttpPeer::new(
            upstream,
            true,
            "one.one.one.one".to_string(),
        ));
        Ok(peer)
    }
}

fn build_cluster_service<S>(upstreams: &[&str]) -> GenBackgroundService<LoadBalancer<S>>
where
    S: BackendSelection + 'static,
    S::Iter: BackendIter,
{
    let mut cluster = LoadBalancer::try_from_iter(upstreams).unwrap();
    cluster.set_health_check(TcpHealthCheck::new());
    cluster.health_check_frequency = Some(std::time::Duration::from_secs(1));

    background_service("cluster health check", cluster)
}

fn main() {
    let mut my_server = Server::new(None).unwrap();
    my_server.bootstrap();

    // Two clusters, each with two upstreams
    let cluster_one = build_cluster_service::<RoundRobin>(&["1.1.1.1:443", "127.0.0.1:343"]);
    let cluster_two = build_cluster_service::<RoundRobin>(&["1.0.0.1:443", "127.0.0.2:343"]);

    let router = Router {
        cluster_one: cluster_one.task(),
        cluster_two: cluster_two.task(),
    };
    let mut router_service = http_proxy_service(&my_server.configuration, router);
    router_service.add_tcp("0.0.0.0:6188");

    // Register the services (the router + background health checks)
    my_server.add_service(router_service);
    my_server.add_service(cluster_one);
    my_server.add_service(cluster_two);

    my_server.run_forever();
}
```

---

## Testing & Observing Behavior

1. **Start the server**:
   ```bash
   cargo run
   ```
   The proxy listens on `127.0.0.1:6188` (plaintext).

2. **Send requests**:
   - `curl 127.0.0.1:6188/one/` -> Goes to `cluster_one`.  
   - `curl 127.0.0.1:6188/two/` -> Goes to `cluster_two`.

3. **Health checks**:
   - Each cluster runs health checks on its upstreams every second. Unhealthy ones are removed.  
   - You’ll see in logs if certain backends (`127.0.0.1:343`) fail and are removed.

4. **Logs**:
   - Look for `println!("upstream peer is: {:?}", upstream);` to see which IP/port was selected.

---

## Conclusion

By creating **multiple LoadBalancer** instances and referencing them in a **Router** that implements `ProxyHttp`, you can easily **split traffic** among distinct sets of backends. This is a flexible pattern for:

- **Versioned APIs**: e.g., `/v1/` vs. `/v2/`.  
- **Geo-based routing**: choose the nearest cluster to the user.  
- **Canary releases**: route a fraction of paths to a new server for testing.

Take this example further by customizing your route logic, adding advanced health checks, or using different load-balancing strategies. Happy coding! 
