# Handling panics

Any panic that happens to particular requests does not affect other ongoing requests or the server's ability to handle other requests. Sockets acquired by the panicking requests are dropped (closed). The panics will be captured by the tokio runtime and then ignored.

In order to monitor the panics, Pingora server has built-in Sentry integration.
```rust
my_server.sentry = Some("SENTRY_DSN");
```

Even though a panic is not fatal in Pingora, it is still not the preferred way to handle failures like network timeouts. Panics should be reserved for unexpected logic errors.
