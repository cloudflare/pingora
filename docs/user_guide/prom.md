# Prometheus

The [`pingora-prometheus`](https://docs.rs/pingora-prometheus) crate provides a
Prometheus HTTP metrics server for scraping.

## Adding the Dependency

Add `pingora-prometheus` to your `Cargo.toml`:

```toml
pingora-prometheus = "0.8.0"
```

## Setting up a Prometheus Metrics Endpoint

```rust
    ...
    let mut prometheus_service_http = pingora_prometheus::prometheus_http_service();
    prometheus_service_http.add_tcp("0.0.0.0:1234");
    my_server.add_service(prometheus_service_http);
    my_server.run_forever();
```

The simplest way to use it is to have [static metrics](https://docs.rs/prometheus/latest/prometheus/#static-metrics).

```rust
static MY_COUNTER: Lazy<IntGauge> = Lazy::new(|| {
    register_int_gauge!("my_counter", "my counter").unwrap()
});

```

This static metric will automatically appear in the Prometheus metric endpoint.
