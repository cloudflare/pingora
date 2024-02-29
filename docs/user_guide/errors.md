# How to return errors

For easy error handling, the `pingora-error` crate exports a custom `Result` type used throughout other Pingora crates.

The `Error` struct used in this `Result`'s error variant is a wrapper around arbitrary error types. It allows the user to tag the source of the underlying error and attach other custom context info.

Users will often need to return errors by propagating an existing error or creating a wholly new one. `pingora-error` makes this easy with its error building functions.

## Examples

For example, one could return an error when an expected header is not present:

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

`validate_req_header` returns an `Error` if the `host` header is not found, using `Error::explain` to create a new `Error` along with an associated type (`InvalidHTTPHeader`) and helpful context that may be logged in an error log.

This error will eventually propagate to the request filter, where it is returned as a new `HTTPStatus` error using `or_err`. (As part of the default pingora-proxy `fail_to_proxy()` phase, not only will this error be logged, but it will result in sending a `400 Bad Request` response downstream.)

Note that the original causing error will be visible in the error logs as well. `or_err` wraps the original causing error in a new one with additional context, but `Error`'s `Display` implementation also prints the chain of causing errors.

## Guidelines

An error has a _type_ (e.g. `ConnectionClosed`), a _source_ (e.g. `Upstream`, `Downstream`, `Internal`), and optionally, a _cause_ (another wrapped error) and a _context_ (arbitrary user-provided string details).

A minimal error can be created using functions like `new_in` / `new_up` / `new_down`, each of which specifies a source and asks the user to provide a type.

Generally speaking:
* To create a new error, without a direct cause but with more context, use `Error::explain`. You can also use `explain_err` on a `Result` to replace the potential error inside it with a new one.
* To wrap a causing error in a new one with more context, use `Error::because`. You can also use `or_err` on a `Result` to replace the potential error inside it by wrapping the original one.

## Retry

Errors can be "retry-able." If the error is retry-able, pingora-proxy will be allowed to retry the upstream request. Some errors are only retry-able on [reused connections](pooling.md), e.g. to handle situations where the remote end has dropped a connection we attempted to reuse.

By default a newly created `Error` either takes on its direct causing error's retry status, or, if left unspecified, is considered not retry-able.
