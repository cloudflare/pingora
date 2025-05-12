# Prometheus

Pingora has a built-in prometheus HTTP metric server for scraping.

## Enabling Prometheus Support

Prometheus support is an optional feature in Pingora. To use it, you need to enable the `prometheus` feature in your `Cargo.toml`:

```toml
# If using the main pingora crate
pingora = { version = "0.5.0", features = ["prometheus"] }

# If using pingora-core directly
pingora-core = { version = "0.5.0", features = ["prometheus"] }

# If using pingora-proxy crate
pingora-proxy = { version = "0.5.0", features = ["prometheus"] }
```

## Setting up a Prometheus Metrics Endpoint

Once the feature is enabled, you can set up a Prometheus metrics endpoint like this:

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
