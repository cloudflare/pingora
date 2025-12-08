# Prometheus

Pingora has a built-in prometheus HTTP metric server for scraping.

```rust
    ...
    let mut prometheus_service_http = Service::prometheus_http_service();
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

## Prometheus Crate Version Compatibility

**Important:** Your application must use the same version of the `prometheus` crate as Pingora. Using different versions will result in separate metric registries, causing your metrics to not appear in the Prometheus endpoint.

To ensure compatibility, either:

1. Match the exact `prometheus` version that Pingora uses (check `pingora-core`'s `Cargo.toml`)
2. Use `version = "*"` in your `Cargo.toml` to automatically match Pingora's version:

```toml
[dependencies]
prometheus = "*"
```

This is necessary because the `prometheus` crate uses semantic versioning where 0.x versions are considered incompatible (e.g., 0.13 and 0.14 maintain separate global registries).
