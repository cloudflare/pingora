# 如何返回错误

为了方便错误处理，`pingora-error` 包导出了一个自定义的 `Result` 类型，该类型在其他 Pingora 包中广泛使用。

在这个 `Result` 类型的错误变体中使用的 `Error` 结构体是一个围绕任意错误类型的包装器。它允许用户标记底层错误的来源并附加其他自定义上下文信息。

用户经常需要通过传播现有错误或创建全新的错误来返回错误。`pingora-error` 通过其错误构建函数使这变得容易。

## 示例

例如，当缺少预期的标头时，可以返回一个错误：

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

如果找不到 `host` 标头，`validate_req_header` 返回一个 `Error`，使用 `Error::explain` 创建一个新的 `Error`，并附带一个相关类型（`InvalidHTTPHeader`）和可能在错误日志中记录的有用上下文。


此错误最终将传播到请求过滤器，其中它作为新的 `HTTPStatus` 错误使用 `or_err` 返回。 （作为默认 `pingora-proxy` 的 `fail_to_proxy()` 阶段的一部分，此错误不仅会被记录，而且会导致向下游发送 `400 Bad Request` 响应。）



请注意，原始的导致错误也将在错误日志中可见。`or_err` 用更多的上下文包装原始导致错误，但 `Error` 的 `Display` 实现也会打印出导致错误的链。


## 指南

错误有一个 类型（例如，`ConnectionClosed`），一个 来源（例如，`Upstream`、`Downstream`、`Internal`），以及可选的 原因（另一个被包装的错误）和 上下文（任意用户提供的字符串细节）。


可以使用 `new_in` / `new_up` / `new_down` 等函数创建最小的错误，每个函数指定一个来源并要求用户提供一个类型。


一般来说    :
* 要创建一个新错误，没有直接原因但有更多上下文，请使用 `Error::explain`。您还可以在 `Result` 上使用 `explain_err` 来替换其中可能存在的错误为新的错误。
* 要在新的错误中包装一个导致错误以提供更多上下文，请使用 `Error::because`。您还可以在 `Result` 上使用 `or_err` 来替换其中可能存在的错误为原始错误的包装。


## 重试


错误可以是“可重试”的。如果错误是可重试的，则允许 pingora-proxy 重试上游请求。有些错误只能在 重用连接 时可重试，例如处理远程端已放弃的我们尝试[重用](pooling.md)的连接的情况。


默认情况下，新创建的 `Error` 要么继承其直接导致错误的重试状态，要么（如果未指定）被认为是不可重试的。

