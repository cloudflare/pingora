# Life of a request: pingora-proxy phases and filters

## Intro
The pingora-proxy HTTP proxy framework supports highly programmable proxy behaviors. This is done by allowing users to inject custom logic into different phases (stages) in the life of a request.

## Life of a proxied HTTP request
1. The life of a proxied HTTP request starts when the proxy reads the request header from the **downstream** (i.e., the client).
2. Then, the proxy connects to the **upstream** (i.e., the remote server). This step is skipped if there is a previously established [connection to reuse](pooling.md).
3. The proxy then sends the request header to the upstream.
4. Once the request header is sent, the proxy enters a duplex mode, which simultaneously proxies:
    a. upstream response (both header and body) to the downstream, and
    b. downstream request body to upstream (if any).
5. Once the entire request/response finishes, the life of the request is ended. All resources are released. The downstream connections and the upstream connections are recycled to be reused if applicable.

## Pingora-proxy phases and filters
Pingora-proxy allows users to insert arbitrary logic into the life of a request.
```mermaid
 graph TD;
    start("new request")-->early_request_filter;
    early_request_filter-->request_filter;
    request_filter-->upstream_peer;

    upstream_peer-->Connect{{IO: connect to upstream}};

    Connect--connection success-->connected_to_upstream;
    Connect--connection failure-->fail_to_connect;

    connected_to_upstream-->upstream_request_filter;
    upstream_request_filter --> request_body_filter;
    request_body_filter --> SendReq{{IO: send request to upstream}};
    SendReq-->RecvResp{{IO: read response from upstream}};
    RecvResp-.feature: adjust_upstream_modules.->adjust_upstream_modules;
    adjust_upstream_modules-->upstream_response_filter-->response_filter-->upstream_response_body_filter-->response_body_filter-->logging-->endreq("request done");

    fail_to_connect --can retry-->upstream_peer;
    fail_to_connect --can't retry-->fail_to_proxy--send error response-->logging;

    RecvResp--failure-->IOFailure;
    SendReq--failure-->IOFailure;
    error_while_proxy--can retry-->upstream_peer;
    error_while_proxy--can't retry-->fail_to_proxy;

    request_filter --send response-->logging


    Error>any response filter error]-->error_while_proxy
    IOFailure>IO error]-->error_while_proxy
```

### General filter usage guidelines
* Most filters return a [`pingora_error::Result<_>`](errors.md). When the returned value is `Result::Err`, `fail_to_proxy()` will be called and the request will be terminated.
* Most filters are async functions, which allows other async operations such as IO to be performed within the filters.
* A per-request `CTX` object can be defined to share states across the filters of the same request. All filters have mutable access to this object.
* Most filters are optional.
* The reason both `upstream_response_*_filter()` and `response_*_filter()` exist is for HTTP caching integration reasons (still WIP).


### `early_request_filter()`
This is the first phase of every request.

This function is similar to `request_filter()` but executes before any other logic, including downstream module logic. The main purpose of this function is to provide finer-grained control of the behavior of the modules.

Right after this phase, before downstream modules and `request_filter()` run, the proxy calls `should_handle_downstream_abandonment()` once to decide whether this request opts in to Pingora's handling of a client that leaves while its response is still being produced. The decision can use the context set up in this phase. It returns `false` by default, which keeps the proxy behaving as it did before that handling existed.

### `request_filter()`
This phase is usually for validating request inputs, rate limiting, and initializing context.

### `request_body_filter()`
This phase is triggered after a request body is ready to send to upstream. It will be called every time a piece of request body is received.

### `proxy_upstream_filter()`
This phase determines if we should continue to the upstream to serve a response. If we short-circuit, a 502 is returned by default, but a different response can be implemented.

This phase returns a boolean determining if we should continue to the upstream or error.

### `upstream_peer()`
This phase decides which upstream to connect to (e.g. with DNS lookup and hashing/round-robin), and how to connect to it.

This phase returns a `Peer` that defines the upstream to connect to. Implementing this phase is **required**.

### `connected_to_upstream()`
This phase is executed when upstream is successfully connected.

Usually this phase is for logging purposes. Connection info such as RTT and upstream TLS ciphers are reported in this phase.

### `fail_to_connect()`
The counterpart of `connected_to_upstream()`. This phase is called if an error is encountered when connecting to upstream.

In this phase users can report the error in Sentry/Prometheus/error log. Users can also decide if the error is retry-able.

If the error is retry-able, `upstream_peer()` will be called again, in which case the user can decide whether to retry the same upstream or failover to a secondary one.

If the error is not retry-able, the request will end.

### `upstream_request_filter()`
This phase is to modify requests before sending to upstream.

### `adjust_upstream_modules()` _(feature: `adjust_upstream_modules`)_
This phase is triggered when the upstream response header arrives, before upstream modules (such as `upstream_compression`) process it.

Use this to configure upstream module behavior based on the response header, e.g. setting a dictionary for dictionary-based content encoding. The response header is provided as an immutable reference; to modify the response header itself, use `upstream_response_filter()` instead.

### `upstream_response_filter()/upstream_response_body_filter()/upstream_response_trailer_filter()`
This phase is triggered after an upstream response header/body/trailer is received.

This phase is to modify or process response headers, body, or trailers before sending to downstream. Note that this phase is called _prior_ to HTTP caching and therefore any changes made here will affect the response stored in the HTTP cache.

### `response_filter()/response_body_filter()/response_trailer_filter()`
This phase is triggered after a response header/body/trailer is ready to send to downstream.

This phase is to modify them before sending to downstream.

### `error_while_proxy()`
This phase is triggered during proxy errors to upstream, this is after the connection is established.

This phase may decide to retry a request if the connection was re-used and the HTTP method is idempotent.

### `fail_to_proxy()`
This phase is called whenever an error is encounter during any of the phases above.

This phase is usually for error logging and error reporting to downstream.

### `logging()`
This is the last phase that runs after the request is finished (or errors) and before any of its resources are released. Every request will end up in this final phase.

This phase is usually for logging and post request cleanup.

### `request_summary()`
This is not a phase, but a commonly used callback.

Errors that reach `fail_to_proxy()` are logged in the error log unless `suppress_error_log()` suppresses them. `request_summary()` will be called to dump the info regarding the request when logging the error.

This callback returns a string which allows users to customize what info to dump in the error log to help track and debug the failures.

### `suppress_error_log()`
This is also not a phase, but another callback.

`fail_to_proxy()` errors are automatically logged in the error log, but users may not be interested in every error. For example, downstream errors are logged if the client disconnects early, but these errors can become noisy if users are mainly interested in observing upstream issues. This callback can inspect the error and returns true or false. If true, the error will not be written to the log.

By default, once a request that `should_handle_downstream_abandonment()` opted in has had its downstream abandoned during a cache fill, this suppresses exactly the errors `pingora_cache::fill_cancellation::is_cache_fill_cancelled()` recognizes: `ERR_CACHE_FILL_CANCELLED`, including when another error wraps it. A cache fill the application itself asked to end early is expected control flow rather than a failure. The error still reaches `logging()`, so access logs are unaffected. For any other request or error the default suppresses nothing. Overriding this callback replaces the default entirely, including that suppression.

Besides errors that reach `fail_to_proxy()`, it is offered the failure of a cache hit of an opted-in request whose client watch ended the hit because the client closed or reset its connection or stream. Every other failed cache hit is logged as it is without the opt-in.

### `downstream_abandoned_during_cache_fill()`
This is also not a phase, but another callback.

It applies only to a request that `should_handle_downstream_abandonment()` opted in. Pingora calls it when that request's downstream becomes unusable and Pingora keeps filling the cache without it. Pingora calls it at most once per request, retries included.

Pingora calls it only if the fill can still produce a cache entry:
* during a miss with an active cache writer, or
* with streaming storage, before the response's cache decision is made. The response's cacheability and its cache writer may not be known yet at that point.

With storage that does not support streaming partial writes, Pingora does not keep filling. It returns the downstream error and does not call this callback.

The callback runs inline on proxy I/O paths, so keep it cheap.

By default the fill then runs until the upstream response completes. A storage backend can let the application cancel it earlier: its miss handler returns a `CacheFillCancellationWatcher` from `HandleMiss::cancellation_watcher()`, and the application calls `cancel()` on the matching `CacheFillCancellation` once nobody is waiting for the fill, either inside this callback or later, for example when the last other reader of the same asset leaves. The proxy only honors the cancellation after the downstream was abandoned, because until then the fill is also the downstream's response, and it observes it the next time it asks for upstream data: data the upstream already handed over is processed first, so a response whose final upstream bytes have already arrived is committed normally, and only a bounded amount of further data follows, so an upstream that always has more ready cannot keep a cancelled fill running. An honored cancellation cancels the upstream read without waiting for more upstream data, drops the miss handler without `finish()`, and fails the request with `ERR_CACHE_FILL_CANCELLED`, which is never retried. One `CacheFillCancellation` covers one logical fill, including miss handlers created for later upstream attempts after abandonment; it must not be reused for an unrelated later request that happens to share the cache key.

### `suppress_proxy_warn_log()`
This is also not a phase, but another callback.

This experimental callback can suppress proxy warning logs that do not reach `fail_to_proxy()`, such as retryable proxy upstream failures and downstream errors ignored while cache fill continues. The callback receives a `ProxyWarnLogContext` so users can distinguish these warning contexts. Final proxy errors are still handled by `suppress_error_log()`.

This API may change or be removed until indicated otherwise. Suppressing retry warning logs can remove the only per-retry audit record, so users should provide alternative observability, such as metrics or logs from this hook.

### Cache filters

To be documented
