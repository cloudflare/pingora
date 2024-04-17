# Prometheus

`Pingora` 内置了用于抓取的 `Prometheus` `HTTP` metrics 服务。

```rust
...
let mut prometheus_service_http = Service::prometheus_http_service();
prometheus_service_http.add_tcp("0.0.0.0:1234");
my_server.add_service(prometheus_service_http);
my_server.run_forever();
```

最简单的使用方法是拥有[静态指标](https://docs.rs/prometheus/latest/prometheus/#static-metrics)。

```rust
static MY_COUNTER: Lazy<IntGauge> = Lazy::new(|| {
register_int_gauge!("my_counter", "my counter").unwrap()
});

```

这个端点静态指标将自动出现在 `Prometheus` 指标中。
