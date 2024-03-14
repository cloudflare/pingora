# 处理失败和故障转移

Pingora-proxy 允许用户定义如何处理代理请求的失败。

当在向下游发送响应头之前发生故障时，用户有几种选择：
1. 向下游发送错误页面，然后放弃。
2. 重试相同的上游。
3. 如果适用，尝试其他上游。

否则，一旦响应头已经向下游发送，代理无法做任何事情，只能记录错误然后放弃请求。

## 重试 / 故障转移
为了实现重试或故障转移，`fail_to_connect()` / `error_while_proxy()` 需要将错误标记为“可重试”。对于故障转移，`fail_to_connect() / error_while_proxy()` 还需要更新 `CTX`，告诉 `upstream_peer()` 不要再次使用相同的 `Peer`。

### 安全性
一般来说，幂等的 HTTP 请求，例如 `GET`，是可以安全重试的。其他请求，例如 `POST`，如果请求已经发送，则不安全重试。当调用 `fail_to_connect()` 时，pingora-proxy 保证未向上游发送任何内容。除非用户足够了解上游服务器以知道是否安全，否则不建议在 `error_while_proxy()` 后重试非幂等请求。

### 示例
在以下示例中，我们在 `CTX` 上设置了一个 `tries` 变量来跟踪我们尝试了多少次连接。当在 `upstream_peer` 中设置我们的对等体时，我们检查 `tries` 是否小于一，并连接到 192.0.2.1。在连接失败时，我们在 `fail_to_connect` 中递增 `tries`，并设置 `e.set_retry(true)`，告诉 Pingora 这是一个可重试的错误。在重试时，我们再次进入 `upstream_peer`，这次连接到 1.1.1.1。如果无法连接到 1.1.1.1，我们返回一个 502 错误，因为我们只在 `tries` 为零时在 `fail_to_connect` 中设置了 `e.set_retry(true)`。

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

        let mut peer = Box::new(HttpPeer::new(addr, true, "one.one.one.one".to_string()));
        peer.options.connection_timeout = Some(Duration::from_millis(100));
        Ok(peer)
    }
}
```