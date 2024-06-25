# Quick Start: load balancer

## Introduction

This quick start shows how to build a bare-bones load balancer using pingora and pingora-proxy.

The goal of the load balancer is for every incoming HTTP request, select one of the two backends: https://1.1.1.1 and https://1.0.0.1 in a round-robin fashion.

## Build a basic load balancer

Create a new cargo project for our load balancer. Let's call it `load_balancer`

```
cargo new load_balancer
```

### Include the Pingora Crate and Basic Dependencies

In your project's `cargo.toml` file add the following to your dependencies
```
async-trait="0.1"
pingora = { version = "0.1", features = [ "lb" ] }
```

### Create a pingora server
First, let's create a pingora server. A pingora `Server` is a process which can host one or many
services. The pingora `Server` takes care of configuration and CLI argument parsing, daemonization,
signal handling, and graceful restart or shutdown.

The preferred usage is to initialize the `Server` in the `main()` function and
use `run_forever()` to spawn all the runtime threads and block the main thread until the server is
ready to exit.


```rust
use async_trait::async_trait;
use pingora::prelude::*;
use std::sync::Arc;

fn main() {
    let mut my_server = Server::new(None).unwrap();
    my_server.bootstrap();
    my_server.run_forever();
}
```

This will compile and run, but it doesn't do anything interesting.

### Create a load balancer proxy
Next let's create a load balancer. Our load balancer holds a static list of upstream IPs. The `pingora-load-balancing` crate already provides the `LoadBalancer` struct with common selection algorithms such as round robin and hashing. So let’s just use it. If the use case requires more sophisticated or customized server selection logic, users can simply implement it themselves in this function.


```rust
pub struct LB(Arc<LoadBalancer<RoundRobin>>);
```

In order to make the server a proxy, we need to implement the `ProxyHttp` trait for it.

Any object that implements the `ProxyHttp` trait essentially defines how a request is handled in
the proxy. The only required method in the `ProxyHttp` trait is `upstream_peer()` which returns
the address where the request should be proxied to.

In the body of the `upstream_peer()`, let's use the `select()` method for the `LoadBalancer` to round-robin across the upstream IPs. In this example we use HTTPS to connect to the backends, so we also need to specify to `use_tls` and set the SNI when constructing our [`Peer`](user_guide/peer.md)) object.

```rust
#[async_trait]
impl ProxyHttp for LB {

    /// For this small example, we don't need context storage
    type CTX = ();
    fn new_ctx(&self) -> () {
        ()
    }

    async fn upstream_peer(&self, _session: &mut Session, _ctx: &mut ()) -> Result<Box<HttpPeer>> {
        let upstream = self.0
            .select(b"", 256) // hash doesn't matter for round robin
            .unwrap();

        println!("upstream peer is: {upstream:?}");

        // Set SNI to one.one.one.one
        let peer = Box::new(HttpPeer::new(upstream, true, "one.one.one.one".to_string()));
        Ok(peer)
    }
}
```

In order for the 1.1.1.1 backends to accept our requests, a host header must be present. Adding this header
can be done by the `upstream_request_filter()` callback which modifies the request header after
the connection to the backends are established and before the request header is sent.

```rust
impl ProxyHttp for LB {
    // ...
    async fn upstream_request_filter(
        &self,
        _session: &mut Session,
        upstream_request: &mut RequestHeader,
        _ctx: &mut Self::CTX,
    ) -> Result<()> {
        upstream_request.insert_header("Host", "one.one.one.one").unwrap();
        Ok(())
    }
}
```


### Create a pingora-proxy service
Next, let's create a proxy service that follows the instructions of the load balancer above.

A pingora `Service` listens to one or multiple (TCP or Unix domain socket) endpoints. When a new connection is established
the `Service` hands the connection over to its "application." `pingora-proxy` is such an application
which proxies the HTTP request to the given backend as configured above.

In the example below, we create a `LB` instance with two backends `1.1.1.1:443` and `1.0.0.1:443`.
We put that `LB` instance to a proxy `Service` via the  `http_proxy_service()` call and then tell our
`Server` to host that proxy `Service`.

```rust
fn main() {
    let mut my_server = Server::new(None).unwrap();
    my_server.bootstrap();

    let upstreams =
        LoadBalancer::try_from_iter(["1.1.1.1:443", "1.0.0.1:443"]).unwrap();

    let mut lb = http_proxy_service(&my_server.configuration, LB(Arc::new(upstreams)));
        lb.add_tcp("0.0.0.0:6188");

    my_server.add_service(lb);

    my_server.run_forever();
}
```

### Run it

Now that we have added the load balancer to the service, we can run our new 
project with 

```cargo run```

To test it, simply send the server a few requests with the command:
```
curl 127.0.0.1:6188 -svo /dev/null
```

You can also navigate your browser to [http://localhost:6188](http://localhost:6188)

The following output shows that the load balancer is doing its job to balance across the two backends:
```
upstream peer is: Backend { addr: Inet(1.0.0.1:443), weight: 1 }
upstream peer is: Backend { addr: Inet(1.1.1.1:443), weight: 1 }
upstream peer is: Backend { addr: Inet(1.0.0.1:443), weight: 1 }
upstream peer is: Backend { addr: Inet(1.1.1.1:443), weight: 1 }
upstream peer is: Backend { addr: Inet(1.0.0.1:443), weight: 1 }
...
```

Well done! At this point you have a functional load balancer. It is a _very_ 
basic load balancer though, so the next section will walk you through how to
make it more robust with some built-in pingora tooling.

## Add functionality

Pingora provides several helpful features that can be enabled and configured 
with just a few lines of code. These range from simple peer health checks to 
the ability to seamlessly update running binary with zero service interruptions.

### Peer health checks

To make our load balancer more reliable, we would like to add some health checks 
to our upstream peers. That way if there is a peer that has gone down, we can 
quickly stop routing our traffic to that peer.

First let's see how our simple load balancer behaves when one of the peers is
down. To do this, we'll update the list of peers to include a peer that is 
guaranteed to be broken.

```rust
fn main() {
    // ...
    let upstreams =
        LoadBalancer::try_from_iter(["1.1.1.1:443", "1.0.0.1:443", "127.0.0.1:343"]).unwrap();
    // ...
}
```

Now if we run our load balancer again with `cargo run`, and test it with 

```
curl 127.0.0.1:6188 -svo /dev/null
```

We can see that one in every 3 request fails with `502: Bad Gateway`. This is 
because our peer selection is strictly following the `RoundRobin` selection 
pattern we gave it with no consideration to whether that peer is healthy. We can
fix this by adding a basic health check service. 

```rust
fn main() {
    let mut my_server = Server::new(None).unwrap();
    my_server.bootstrap();

    // Note that upstreams needs to be declared as `mut` now
    let mut upstreams =
        LoadBalancer::try_from_iter(["1.1.1.1:443", "1.0.0.1:443", "127.0.0.1:343"]).unwrap();

    let hc = TcpHealthCheck::new();
    upstreams.set_health_check(hc);
    upstreams.health_check_frequency = Some(std::time::Duration::from_secs(1));

    let background = background_service("health check", upstreams);
    let upstreams = background.task();

    // `upstreams` no longer need to be wrapped in an arc
    let mut lb = http_proxy_service(&my_server.configuration, LB(upstreams));
    lb.add_tcp("0.0.0.0:6188");

    my_server.add_service(background);

    my_server.add_service(lb);
    my_server.run_forever();
}
```

Now if we again run and test our load balancer, we see that all requests 
succeed and the broken peer is never used. Based on the configuration we used, 
if that peer were to become healthy again, it would be re-included in the round
robin again in within 1 second.

### Command line options

The pingora `Server` type provides a lot of built-in functionality that we can
take advantage of with single-line change. 

```rust
fn main() {
    let mut my_server = Server::new(Some(Opt::parse_args())).unwrap();
    ...
}
```

With this change, the command-line arguments passed to our load balancer will be 
consumed by Pingora. We can test this by running:

```
cargo run -- -h
```

We should see a help menu with the list of arguments now available to us. We 
will take advantage of those in the next sections to do more with our load 
balancer for free

### Running in the background

Passing the parameter `-d` or `--daemon` will tell the program to run in the background.

```
cargo run -- -d
```

To stop this service, you can send `SIGTERM` signal to it for a graceful shutdown, in which the service will stop accepting new request but try to finish all ongoing requests before exiting.
```
pkill -SIGTERM load_balancer
```
 (`SIGTERM` is the default signal for `pkill`.)

### Configurations
Pingora configuration files help define how to run the service. Here is an 
example config file that defines how many threads the service can have, the 
location of the pid file, the error log file, and the upgrade coordination 
socket (which we will explain later). Copy the contents below and put them into
a file called `conf.yaml` in your `load_balancer` project directory.

```yaml
---
version: 1
threads: 2
pid_file: /tmp/load_balancer.pid
error_log: /tmp/load_balancer_err.log
upgrade_sock: /tmp/load_balancer.sock
```

To use this conf file:
```
RUST_LOG=INFO cargo run -- -c conf.yaml -d
```
`RUST_LOG=INFO` is here so that the service actually populate the error log.

Now you can find the pid of the service.
```
 cat /tmp/load_balancer.pid
```

### Gracefully upgrade the service
(Linux only)

Let's say we changed the code of the load balancer and recompiled the binary. Now we want to upgrade the service running in the background to this newer version.

If we simply stop the old service, then start the new one, some request arriving in between could be lost. Fortunately, Pingora provides a graceful way to upgrade the service.

This is done by, first, send `SIGQUIT` signal to the running server, and then start the new server with the parameter `-u` \ `--upgrade`.

```
pkill -SIGQUIT load_balancer &&\
RUST_LOG=INFO cargo run -- -c conf.yaml -d -u
```

In this process, The old running server will wait and hand over its listening sockets to the new server. Then the old server runs until all its ongoing requests finish.

From a client's perspective, the service is always running because the listening socket is never closed.

## Full examples

The full code for this example is available in this repository under

[pingora-proxy/examples/load_balancer.rs](../pingora-proxy/examples/load_balancer.rs)

Other examples that you may find helpful are also available here

[pingora-proxy/examples/](../pingora-proxy/examples/)
[pingora/examples](../pingora/examples/)