# 如何返回异常

为了方便异常处理，`pingora-error` 包导出了一个自定义的 `Result` 类型，该类型在其他 `Pingora` 包中广泛使用。

在这个 `Result` 类型的异常变量中使用的 `Error` 结构体是一个包装任意异常类型的包装器。它允许用户标记底层异常的来源并附加其他自定义上下文信息。

用户经常需要抛出现有异常或对异常进行包装来返回新的异常信息。`pingora-error` 通过这个构建函数其构建异常信息变得容易。

## 示例

例如，当缺少预期的请求头时，可以返回一个异常：


```rust
fn validate_req_header(req: &RequestHeader) -> Result<()> {
    // validate that the `host` header exists
    req.headers()
        .get(http::header::HOST)
        .ok_or_else(|| Error::explain(InvalidHTTPHeader, "No host header detected"))
}

impl MyServer {
    pub async fn handle_request_filter(
        &self,
        http_session: &mut Session,
        ctx: &mut CTX,
    ) -> Result<bool> {
        validate_req_header(session.req_header()?).or_err(HTTPStatus(400), "Missing required headers")?;
        Ok(true)
    }
}
```

如果找不到 `host` 请求头，`validate_req_header` 返回一个 `Error`，使用 `Error::explain` 创建一个 `Error`，并附带一个相关类型（`InvalidHTTPHeader`）并记录去可能在异常日志中有用上下文信息。


此异常最终将传播到请求过滤器，其中它作为新的 `HTTPStatus` 异常使用 `or_err` 返回。 （作为默认 `pingora-proxy` 的 `fail_to_proxy()` 阶段的一部分，此异常不仅会被记录，而且会导致向下游发送 `400 Bad Request` 响应。）


请注意，捕获的原始异常也将在异常日志中可见。`or_err` 用更多的上下文包装原始捕获的异常，但 `Error` 的 `Display` 实现也会打印出捕获异常的链。


## 指南

异常有一个 类型（例如，`ConnectionClosed`），一个 来源（例如，`Upstream`、`Downstream`、`Internal`），以及可选的 原因（另一个被包装的异常）和 上下文（任意用户提供的字符串细节）。


可以使用 `new_in` / `new_up` / `new_down` 等函数创建最小的异常，每个函数指定一个来源并要求用户提供一个类型。


一般来说    :
* 要创建一个新异常，没有直接原因但有更多上下文，请使用 `Error::explain`。您还可以在 `Result` 上使用 `explain_err` 来替换其中可能存在的异常为新的异常。
* 要在新的异常中包装一个捕获到的异常并且提供更多上下文，请使用 `Error::because`。您还可以在 `Result` 上使用 `or_err` 来替换其中可能存在的异常为原始异常的包装。


## 重试


异常可以是“可重试”的。如果异常是可重试的，则允许 pingora-proxy 重试上游请求。有些异常只能在 复用连接 时可重试，例如处理远程端已放弃的我们尝试[复用](pooling_zh.md)的连接的情况。


默认情况下，新创建的 `Error` 要么继承其直接导致异常的重试状态，要么（如果未指定）被认为是不可重试的。

