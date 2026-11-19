# Runtime groups

By default, every service runs on a Tokio runtime of its own, with the number of worker threads
from `threads` in the [configuration](conf.md) or from the service's own setting. A server with
several busy services therefore runs several separate worker pools: an idle worker of one service
cannot run tasks queued for another, and the total number of workers grows with each service.

Services with similar workloads can share one worker pool by naming the same runtime group.

```rust
use pingora_core::server::{RuntimeGroupConf, Server};
use pingora_proxy::http_proxy_service;

let mut server = Server::new(None).unwrap();
server.bootstrap();

// Optional. Without it, the group uses the largest `threads` of its members.
server.add_runtime_group("traffic", RuntimeGroupConf::new().threads(16));

let mut public = http_proxy_service(&server.configuration, PublicProxy);
public.add_tcp("0.0.0.0:6188");
public.set_runtime_group("traffic");

let mut internal = http_proxy_service(&server.configuration, InternalProxy);
internal.add_uds("/tmp/internal.sock", None);
internal.set_runtime_group("traffic");

server.add_service(public);
server.add_service(internal);
server.run_forever();
```

A service joins a group through `runtime_group()` on its service trait. Listening services have
`set_runtime_group()`, and `ProxyServiceBuilder` has `runtime_group()`.

What stays per service:
- its name, in logs;
- its dependencies, startup delay and readiness. A member that waits for a dependency does not hold
  back the other members.

What the group shares:
- one runtime, whose worker threads are named after the group;
- one blocking thread pool. `max_blocking_threads` applies to each runtime, so services that each
  had their own limit share one limit once grouped. Set `RuntimeGroupConf::max_blocking_threads()` to
  keep their combined capacity.

How the group's settings are chosen is described on `Server::add_runtime_group()`. In short, the
group's own configuration wins; otherwise the largest member thread count is used, and at most one
member may override the runtime options.

The server refuses to start, with an error naming the group and services, when:
- the group's thread count is zero, whether set with `RuntimeGroupConf::threads(0)` or resolved
  from the members and `ServerConf::threads`;
- two members of one group override the runtime options and the group does not set its own;
- a member of a group without its own thread count asks for zero threads;
- two members of one group listen on the same address. Listeners share descriptors by address
  through the server's listening-fd table, whether the descriptor was inherited or freshly bound,
  so run such services in separate runtimes or give them distinct addresses. Only exactly equal
  address strings are caught, and services whose `listen_addresses()` returns `None` are not
  checked.

The bootstrap service from `bootstrap_as_a_service()` blocks a thread while it receives inherited
sockets during an upgrade, so it always keeps a runtime of its own.
