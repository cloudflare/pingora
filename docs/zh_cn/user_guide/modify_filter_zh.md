# 示例：控制请求

在本节中，我们将介绍如何路由、修改或拒绝请求。

## 路由
请求中的任何信息都可以用于进行路由决策。`Pingora` 开发者可以轻松实现自己的路由逻辑，没有任何约束。

在下面的示例中，代理仅在请求路径以 `/family/` 开头时将流量发送到 `1.0.0.1`。所有其他请求都路由到 `1.1.1.1`。

```Rust
pub struct MyGateway;

#[async_trait]
impl ProxyHttp for MyGateway {
type CTX = ();
fn new_ctx(&self) -> Self::CTX {}

    async fn upstream_peer(
        &self,
        session: &mut Session,
        _ctx: &mut Self::CTX,
    ) -> Result<Box<HttpPeer>> {
        let addr = if session.req_header().uri.path().starts_with("/family/") {
            ("1.0.0.1", 443)
        } else {
            ("1.1.1.1", 443)
        };

        info!("连接到 {addr:?}");

        let peer = Box::new(HttpPeer::new(addr, true, "one.one.one.one".to_string()));
        Ok(peer)
    }
}
```


## 修改 Header

可以在相应的阶段中添加、删除或修改请求和响应的`Header`。在下面的示例中，我们在 `response_filter` 阶段中添加了逻辑用于修改 `Server`  的`Header`并删除 `alt-svc` 请求头。

```Rust
#[async_trait]
impl ProxyHttp for MyGateway {
...
async fn response_filter(
&self,
_session: &mut Session,
upstream_response: &mut ResponseHeader,
_ctx: &mut Self::CTX,
) -> Result<()>
where
Self::CTX: Send + Sync,
{
// 如果存在，替换现有头部
upstream_response
.insert_header("Server", "MyGateway")
.unwrap();
// 因为我们不支持 h3
upstream_response.remove_header("alt-svc");

        Ok(())
    }
}
```

## 返回异常页面

有时，代理在某些条件下，如身份验证失败时，可能希望直接返回异常页面，而不是代理流量。

```Rust
fn check_login(req: &pingora_http::RequestHeader) -> bool {
    // 在此实现您的检查逻辑
    req.headers.get("Authorization").map(|v| v.as_bytes()) == Some(b"password")
}

#[async_trait]
impl ProxyHttp for MyGateway {
    ...
    async fn request_filter(&self, session: &mut Session, _ctx: &mut Self::CTX) -> Result<bool> {
    if session.req_header().uri.path().starts_with("/login")
    && !check_login(session.req_header())
    {
        let _ = session.respond_error(403).await;
        // true：告诉代理响应已经写入
        return Ok(true);
    }
    Ok(false)
}
```
## 日志记录

可以在 `Pingora` 的 `logging` 阶段中添加日志记录逻辑。日志记录阶段在 `Pingora` 代理完成处理请求之前的每个请求上运行。此阶段对于成功和失败的请求都会运行。

在下面的示例中，我们为代理添加了 `Prometheus` 指标和访问日志记录。为了使指标可以被抓取，我们还在不同的端口上启动了一个 `Prometheus` 指标服务器。


```Rust
pub struct MyGateway {
    req_metric: prometheus::IntCounter,
}

#[async_trait]
impl ProxyHttp for MyGateway {
    ...
    async fn logging(
        &self,
        session: &mut Session,
        _e: Option<&pingora::Error>,
        ctx: &mut Self::CTX,
        ) {
        let response_code = session
        .response_written()
        .map_or(0, |resp| resp.status.as_u16());
        // 访问日志
        info!(
        "{} 响应码：{response_code}",
        self.request_summary(session, ctx)
        );

        self.req_metric.inc();
    }

    fn main() {
        ...
        let mut prometheus_service_http =
        pingora::services::listening::Service::prometheus_http_service();
        prometheus_service_http.add_tcp("127.0.0.1:6192");
        my_server.add_service(prometheus_service_http);
    
        my_server.run_forever();
    }
```
