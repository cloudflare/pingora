# 处理 panic

对于指定请求发生的任何 `panic` 都不会影响其他正在进行的请求和处理其他请求的能力。
当前请求发生 `panic` 的的时候相对于应的`Socket`将被关闭。这些 `panic` 将被 `tokio` 运行时捕获，然后被忽略。

为了监视 `panic`，`Pingora` 服务器内置了 `Sentry` 集成。

```Rust
my_server.sentry = Some("SENTRY_DSN");
```

尽管在 `Pingora` 中 `panic` 不会导致致命异常，但仍不建议将其用于处理网络超时等故障。`panic` 应该保留用于处理意外的逻辑异常。
