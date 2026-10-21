# Handling failures and failover

Pingora-proxy allows users to define how to handle failures throughout the life of a proxied request.

When a failure happens before the response header is sent downstream, users have a few options:
1. Send an error page downstream and then give up.
2. Retry the same upstream again.
3. Try another upstream if applicable.

Otherwise, once the response header is already sent downstream, there is nothing the proxy can do other than logging an error and then giving up on the request.


## Retry / Failover
In order to implement retry or failover, `fail_to_connect()` / `error_while_proxy()` needs to mark the error as "retry-able." For failover, `fail_to_connect() / error_while_proxy()` also needs to update the `CTX` to tell `upstream_peer()` not to use the same `Peer` again.

### Stop retries when the client is gone
Pingora does not check whether the client is still there before it retries. To stop the retries, call `session.downstream_gone()` in `upstream_peer()`, which Pingora calls before every attempt. If it returns a value, return a downstream `ConnectionClosed` error. This error is not retryable, so it stops all retries: after a failed connection, and after an error on an established connection. The default `fail_to_proxy()` then sends no response to the client.

```Rust
async fn upstream_peer(&self, session: &mut Session, ctx: &mut Self::CTX) -> Result<Box<HttpPeer>> {
    if session.downstream_gone().is_some() {
        return Err(Error::explain(ConnectionClosed, "client gone").into_down());
    }
    // select the peer
}
```

This check also stops the first attempt. To give every request a first attempt, skip the check on the first call, with a counter in the `CTX`. To log or label a request whose client is gone, save the result in the `CTX` and read it in `fail_to_proxy()`. Do not call the check again there, because a later call can give a different result.

The check does not wait. A connection attempt that is in progress continues. If the client leaves during a connection attempt that is successful, Pingora sends the request.

For HTTP/1 on a TCP or Unix socket, with or without TLS, the check does not read data from the socket. It finds a reset at all times. It finds a closed connection only after Pingora reads all of the request body, and only if `abort_on_close` is on (the default). Pingora reads the request body only after it connects to the upstream, unless the `early_body_buffer` feature read the body before. For this reason, if the client sends a request body and then closes the connection, the retries continue while the connections fail. The check does not find a TLS `close_notify` if the TCP connection stays open.

For HTTP/2, the check finds a stream reset or a closed connection at all times. It does not wait for Pingora to read the request body.

When `should_handle_downstream_abandonment()` is on, Pingora can retry a request after its client left during a cache fill. This retry only fills the cache. The check in `upstream_peer()` also stops this retry, and Pingora then releases the cache lock for the next request. To keep this retry, do not do the check when `session.downstream_abandoned_during_cache_fill()` returns `true`.

### Safety
In general, idempotent HTTP requests, e.g., `GET`, are safe to retry. Other requests, e.g., `POST`, are not safe to retry if the requests have already been sent. When `fail_to_connect()` is called, pingora-proxy guarantees that nothing was sent upstream. Users are not recommended to retry a non-idempotent request after `error_while_proxy()` unless they know the upstream server enough to know whether it is safe.

### Example
In the following example we set a `tries` variable on the `CTX` to track how many connection attempts we've made. When setting our peer in `upstream_peer` we check if `tries` is less than one and connect to 192.0.2.1. On connect failure we increment `tries` in `fail_to_connect` and set `e.set_retry(true)` which tells Pingora this is a retryable error. On retry, we enter `upstream_peer` again and this time connect to 1.1.1.1. If we're unable to connect to 1.1.1.1 we return a 502 since we only set `e.set_retry(true)` in `fail_to_connect` when `tries` is zero.

```Rust
pub struct MyProxy();

pub struct MyCtx {
    tries: usize,
}

#[async_trait]
impl ProxyHttp for MyProxy {
    type CTX = MyCtx;
    fn new_ctx(&self) -> Self::CTX {
        MyCtx { tries: 0 }
    }

    fn fail_to_connect(
        &self,
        _session: &mut Session,
        _peer: &HttpPeer,
        ctx: &mut Self::CTX,
        mut e: Box<Error>,
    ) -> Box<Error> {
        if ctx.tries > 0 {
            return e;
        }
        ctx.tries += 1;
        e.set_retry(true);
        e
    }

    async fn upstream_peer(
        &self,
        _session: &mut Session,
        ctx: &mut Self::CTX,
    ) -> Result<Box<HttpPeer>> {
        let addr = if ctx.tries < 1 {
            ("192.0.2.1", 443)
        } else {
            ("1.1.1.1", 443)
        };

        let mut peer = Box::new(HttpPeer::new(addr, true, "one.one.one.one".to_string())?);
        peer.options.connection_timeout = Some(Duration::from_millis(100));
        Ok(peer)
    }
}
```
