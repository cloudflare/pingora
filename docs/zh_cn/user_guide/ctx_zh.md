# 跨阶段的状态共享使用 `CTX`

## 使用 `CTX`
用户在请求的不同阶段实现的自定义过滤器不直接相互交互。为了在过滤器之间共享信息和状态，用户可以定义一个 `CTX` 结构。每个请求拥有一个独立的 `CTX` 对象。所有过滤器都能够读取和修改 `CTX` 对象的值。`CTX` 对象将在请求结束时被丢弃。

### 示例

在以下示例中，在 `request_filter` 阶段解析请求头，在 `upstream_peer` 阶段使用 `bool` 值决定将流量路由到哪个服务器。（从技术上讲，请求头可以在 `upstream_peer` 阶段解析出来，但我们为了演示而在较早的阶段执行解析。

```Rust
pub struct MyProxy();

pub struct MyCtx {
    beta_user: bool,
}

fn check_beta_user(req: &pingora_http::RequestHeader) -> bool {
    // 一些简单的逻辑来检查用户是否是 beta 用户
    req.headers.get("beta-flag").is_some()
}

#[async_trait]
impl ProxyHttp for MyProxy {
    type CTX = MyCtx;
    fn new_ctx(&self) -> Self::CTX {
        MyCtx { beta_user: false }
    }

    async fn request_filter(&self, session: &mut Session, ctx: &mut Self::CTX) -> Result<bool> {
        ctx.beta_user = check_beta_user(session.req_header());
        Ok(false)
    }

    async fn upstream_peer(
        &self,
        _session: &mut Session,
        ctx: &mut Self::CTX,
    ) -> Result<Box<HttpPeer>> {
        let addr = if ctx.beta_user {
            info!("I'm a beta user");
            ("1.0.0.1", 443)
        } else {
            ("1.1.1.1", 443)
        };

        let peer = Box::new(HttpPeer::new(addr, true, "one.one.one.one".to_string()));
        Ok(peer)
    }
}
```

## 跨请求共享状态

在请求之间共享状态，例如计数器、缓存和其他信息是很常见的。在 `Pingora` 中，共享资源和数据跨请求并不需要特殊处理。可以使用 `Arc`、`static` 或任何其他机制。



### 示例

让我们修改上面的实例代码，用来跟踪 `beta` 访问者的数量以及总访问者的数量。计数器可以定义在 `MyProxy` 结构体本身中，也可以定义为全局变量。因为计数器可以同时访问，所以这里使用 `Mutex`。


```Rust
// 全局计数器
static REQ_COUNTER: Mutex<usize> = Mutex::new(0);

pub struct MyProxy {
    // 服务的计数器
    beta_counter: Mutex<usize>, // 也可以使用 AtomicUsize
}

pub struct MyCtx {
    beta_user: bool,
}

fn check_beta_user(req: &pingora_http::RequestHeader) -> bool {
    // 一些简单的逻辑来检查用户是否是 beta 用户
    req.headers.get("beta-flag").is_some()
}

#[async_trait]
impl ProxyHttp for MyProxy {
    type CTX = MyCtx;
    fn new_ctx(&self) -> Self::CTX {
        MyCtx { beta_user: false }
    }

    async fn request_filter(&self, session: &mut Session, ctx: &mut Self::CTX) -> Result<bool> {
        ctx.beta_user = check_beta_user(session.req_header());
        Ok(false)
    }

    async fn upstream_peer(
        &self,
        _session: &mut Session,
        ctx: &mut Self::CTX,
    ) -> Result<Box<HttpPeer>> {
        let mut req_counter = REQ_COUNTER.lock().unwrap();
        *req_counter += 1;

        let addr = if ctx.beta_user {
            let mut beta_count = self.beta_counter.lock().unwrap();
            *beta_count += 1;
            info!("I'm a beta user #{beta_count}");
            ("1.0.0.1", 443)
        } else {
            info!("I'm an user #{req_counter}");
            ("1.1.1.1", 443)
        };

        let peer = Box::new(HttpPeer::new(addr, true, "one.one.one.one".to_string()));
        Ok(peer)
    }
}
```

完整示例可以在 [`pingora-proxy/examples/ctx.rs`](../../pingora-proxy/examples/ctx.rs) 中找到。您可以使用 `cargo` 运行它：

```bash
RUST_LOG=INFO cargo run --example ctx
```