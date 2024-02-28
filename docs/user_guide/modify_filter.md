# Examples: taking control of the request

In this section we will go through how to route, modify or reject requests.

## Routing
Any information from the request can be used to make routing decision. Pingora doesn't impose any constraints on how users could implement their own routing logic.

In the following example, the proxy sends traffic to 1.0.0.1 only when the request path start with `/family/`. All the other requests are routed to 1.1.1.1.

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

        info!("connecting to {addr:?}");

        let peer = Box::new(HttpPeer::new(addr, true, "one.one.one.one".to_string()));
        Ok(peer)
    }
}
```


## Modifying headers

Both request and response headers can be added, removed or modified in their corresponding phases. In the following example, we add logic to the `response_filter` phase to update the `Server` header and remove the `alt-svc` header.

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
        // replace existing header if any
        upstream_response
            .insert_header("Server", "MyGateway")
            .unwrap();
        // because we don't support h3
        upstream_response.remove_header("alt-svc");

        Ok(())
    }
}
```

## Return Error pages

Sometimes instead of proxying the traffic, under certain conditions, such as authentication failures, you might want the proxy to just return an error page.

```Rust
fn check_login(req: &pingora_http::RequestHeader) -> bool {
    // implement you logic check logic here
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
            // true: tell the proxy that the response is already written
            return Ok(true);
        }
        Ok(false)
    }
```
## Logging

Logging logic can be added to the `logging` phase of Pingora. The logging phase runs on every request right before Pingora proxy finish processing it. This phase runs for both successful and failed requests.

In the example below, we add Prometheus metric and access logging to the proxy. In order for the metrics to be scraped, we also start a Prometheus metric server on a different port.


``` Rust
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
        // access log
        info!(
            "{} response code: {response_code}",
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