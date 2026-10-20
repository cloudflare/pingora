// Copyright 2026 Cloudflare, Inc.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use super::*;
use crate::utils::server_utils::piped_subrequest;

/// A client resets its stream after the first body chunk of an admitted miss whose origin ends
/// the response with trailers. The fill completes and is served as a hit.
async fn assert_h2_downstream_rst_after_admission_completes_fill_with_trailers(
    path: &'static str,
    handles_abandonment: bool,
) {
    init();

    let reported = usize::from(handles_abandonment);
    let uri = format!("http://127.0.0.1:6154{path}");
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_port = listener.local_addr().unwrap().port();
    let (finish_tx, finish_rx) = tokio::sync::oneshot::channel();
    let (done_tx, done_rx) = tokio::sync::oneshot::channel();

    let upstream = tokio::spawn(async move {
        let (request, mut response) = accept_h2_request(listener).await;
        let request_body = request.into_body();
        let response_header = Response::builder()
            .status(200)
            .header("cache-control", "max-age=30")
            .body(())
            .unwrap();
        let mut response_body = response.send_response(response_header, false).unwrap();
        response_body
            .send_data(Bytes::from_static(b"hello "), false)
            .unwrap();

        finish_rx.await.unwrap();
        response_body
            .send_data(Bytes::from_static(b"world!"), false)
            .unwrap();
        let mut trailers = http::HeaderMap::new();
        trailers.insert("x-origin-trailer", "done".parse().unwrap());
        response_body.send_trailers(trailers).unwrap();

        let _ = done_rx.await;
        drop(request_body);
        drop(response_body);
    });

    let mut client = h2c_client(6154).await;

    let attempts_before = recorded_upstream_attempts(path);
    let completed_before = recorded_request_completions(path);
    let request = opt_in(Request::builder(), handles_abandonment)
        .method("GET")
        .uri(uri.as_str())
        .header("x-h2", "true")
        .header("x-port", upstream_port.to_string())
        .header("x-record-request-lifecycle", "true")
        .body(())
        .unwrap();
    let (response, mut request_body) = client.send_request(request, true).unwrap();
    let response = tokio::time::timeout(TEST_TIMEOUT, response)
        .await
        .expect("proxy did not send response headers")
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers().get("x-cache-status").unwrap(), "miss");
    let mut response_body = response.into_body();
    let first_chunk = read_h2_body_chunk(&mut response_body).await;
    assert_eq!(first_chunk, Bytes::from_static(b"hello "));

    let abandoned_calls_before = downstream_abandoned_during_cache_fill_calls(path);
    let warn_calls_before = downstream_cache_warn_log_calls_for(path);
    request_body.send_reset(h2::Reason::CANCEL);
    wait_for_ignored_downstream_error(path, warn_calls_before).await;
    assert_eq!(
        downstream_abandoned_during_cache_fill_calls(path),
        abandoned_calls_before + reported,
    );

    finish_tx.send(()).unwrap();
    wait_for_recorded_request_completion(path, completed_before).await;
    assert_eq!(recorded_request_error(path), None);
    assert_eq!(recorded_upstream_attempts(path), attempts_before + 1);
    let body = wait_for_h2_cache_hit(&uri, upstream_port).await;
    assert_eq!(body, b"hello world!");
    assert_eq!(
        downstream_abandoned_during_cache_fill_calls(path),
        abandoned_calls_before + reported,
        "finishing the cache fill must not repeat the callback"
    );

    let _ = done_tx.send(());
    tokio::time::timeout(TEST_TIMEOUT, upstream)
        .await
        .expect("upstream task did not finish")
        .unwrap();
}

#[tokio::test]
async fn test_h2_downstream_rst_after_admission_completes_fill_with_trailers() {
    assert_h2_downstream_rst_after_admission_completes_fill_with_trailers(
        "/test_h2_downstream_rst_after_admission_with_trailers",
        true,
    )
    .await;
    // Without the opt-in the reset is ignored for the fill but not reported.
    assert_h2_downstream_rst_after_admission_completes_fill_with_trailers(
        "/test_h2_downstream_rst_after_admission_with_trailers_ungated",
        false,
    )
    .await;
}

#[tokio::test]
async fn test_h2_abandoned_cache_fill_finish_error_stays_best_effort() {
    bounded(assert_abandoned_storage_failure(
        "/test_h2_abandoned_cache_fill_finish_error_stays_best_effort",
        CacheStorageFailure::Finish,
        Upstream::H2,
        true,
    ))
    .await;
    // Without the opt-in the reset is not reported; the finish failure is the same.
    bounded(assert_abandoned_storage_failure(
        "/test_h2_cache_fill_finish_error_after_reset_stays_best_effort_ungated",
        CacheStorageFailure::Finish,
        Upstream::H2,
        false,
    ))
    .await;
}

#[tokio::test]
async fn test_h2_abandoned_cache_fill_propagates_write_error() {
    bounded(assert_abandoned_storage_failure(
        "/test_h2_abandoned_cache_fill_propagates_write_error",
        CacheStorageFailure::Write,
        Upstream::H2,
        true,
    ))
    .await;
    // Without the opt-in the reset is not reported, and the write failure still fails the
    // request because the reset client was being served the miss body from the failing cache.
    bounded(assert_abandoned_storage_failure(
        "/test_h2_cache_fill_write_error_after_reset_ungated",
        CacheStorageFailure::Write,
        Upstream::H2,
        false,
    ))
    .await;
}

/// A request whose client closes its write side once it has read the response header.
#[derive(Clone, Copy)]
enum ClosedRequest {
    /// `HEAD`, whose response is complete with its header.
    Head,
    /// A `GET` whose `If-None-Match` matches, answered by a complete `304 Not Modified`.
    NotModified,
    /// A plain `GET`, whose response body is still to come.
    Get,
}

impl ClosedRequest {
    /// The request line and any header that selects this request.
    fn start(self, path: &str) -> String {
        match self {
            Self::Head => format!("HEAD {path} HTTP/1.1\r\n"),
            Self::NotModified => {
                format!("GET {path} HTTP/1.1\r\nIf-None-Match: \"cache-fill\"\r\n")
            }
            Self::Get => format!("GET {path} HTTP/1.1\r\n"),
        }
    }

    /// The status line of the response.
    fn status_line(self) -> &'static str {
        match self {
            Self::Head | Self::Get => "HTTP/1.1 200",
            Self::NotModified => "HTTP/1.1 304",
        }
    }
}

/// The body [`GatedOrigin`] sends in two parts: this start when the test asks for it, and
/// [`BODY_REST`] on release.
const BODY_START: &[u8] = b"bo";
const BODY_REST: &[u8] = b"dy";

/// An origin for one request that sends a cacheable response's header at once, then
/// [`BODY_START`] when `send_body_start` fires and [`BODY_REST`] when `release` fires, so the
/// fill outlives a response that ends with its header. The response has no `Content-Length`
/// (it is chunked over HTTP/1), so the proxy flushes its response header to an HTTP/1 client at
/// once even when a body is to follow.
struct GatedOrigin {
    port: u16,
    send_body_start: tokio::sync::oneshot::Sender<()>,
    release: tokio::sync::oneshot::Sender<()>,
    task: tokio::task::JoinHandle<()>,
}

impl GatedOrigin {
    async fn start(path: &'static str, upstream: Upstream) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (send_body_start, body_start) = tokio::sync::oneshot::channel::<()>();
        let (release, released) = tokio::sync::oneshot::channel::<()>();
        let task = tokio::spawn(async move {
            // Whatever the client asked for, the fill is an unconditional GET.
            match upstream {
                Upstream::H1 => {
                    let (mut socket, _) = listener.accept().await.unwrap();
                    let request = read_h1_head(&mut socket).await;
                    assert_eq!(
                        request.lines().next().unwrap(),
                        format!("GET {path} HTTP/1.1")
                    );
                    assert!(!request.to_ascii_lowercase().contains("if-none-match:"));
                    socket
                        .write_all(
                            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\
                              Cache-Control: max-age=30\r\nETag: \"cache-fill\"\r\n\r\n",
                        )
                        .await
                        .unwrap();
                    body_start.await.unwrap();
                    // A cancelled fill may already have closed the connection.
                    let _ = socket
                        .write_all(&[b"2\r\n".as_slice(), BODY_START, b"\r\n"].concat())
                        .await;
                    released.await.unwrap();
                    let _ = socket
                        .write_all(&[b"2\r\n".as_slice(), BODY_REST, b"\r\n0\r\n\r\n"].concat())
                        .await;
                }
                Upstream::H2 => {
                    let (request, mut respond) = accept_h2_request(listener).await;
                    assert_eq!(request.method(), http::Method::GET);
                    assert_eq!(request.uri().path(), path);
                    assert!(!request.headers().contains_key(header::IF_NONE_MATCH));
                    let response = Response::builder()
                        .header(header::CACHE_CONTROL, "max-age=30")
                        .header(header::ETAG, "\"cache-fill\"")
                        .body(())
                        .unwrap();
                    let mut body = respond.send_response(response, false).unwrap();
                    body_start.await.unwrap();
                    // A cancelled fill may already have reset the stream.
                    let _ = body.send_data(Bytes::from_static(BODY_START), false);
                    released.await.unwrap();
                    let _ = body.send_data(Bytes::from_static(BODY_REST), true);
                }
            }
        });
        Self {
            port,
            send_body_start,
            release,
            task,
        }
    }
}

/// What the proxy recorded for a request whose client closed after the response header.
struct ClosedClient {
    abandonments: usize,
    request_error: Option<pingora_error::ErrorType>,
}

/// The counters for a path, sampled before a close scenario sends its request.
struct Before {
    completed: usize,
    abandoned: usize,
    warn_calls: usize,
}

impl Before {
    fn sample(path: &str) -> Self {
        Self {
            completed: recorded_request_completions(path),
            abandoned: downstream_abandoned_during_cache_fill_calls(path),
            warn_calls: downstream_cache_warn_log_calls_for(path),
        }
    }
}

/// Once the proxy has ignored the client's close for `path`, let `origin` send the body and
/// finish the fill, wait for a request recorded under `path` to complete after
/// `completed_before`, and report what the proxy recorded. The close must be the only
/// downstream error the proxy ignores for `path`.
async fn finish_fill_after_ignored_close(
    path: &'static str,
    origin: GatedOrigin,
    before: &Before,
    completed_before: usize,
) -> ClosedClient {
    wait_until(
        &format!("the proxy did not ignore the client's close for {path}"),
        || downstream_cache_warn_log_calls_for(path) != before.warn_calls,
    )
    .await;
    origin.send_body_start.send(()).unwrap();
    origin.release.send(()).unwrap();
    origin.task.await.unwrap();
    wait_for_recorded_request_completion(path, completed_before).await;
    assert_eq!(
        downstream_cache_warn_log_calls_for(path),
        before.warn_calls + 1
    );
    ClosedClient {
        abandonments: downstream_abandoned_during_cache_fill_calls(path) - before.abandoned,
        request_error: recorded_request_error(path),
    }
}

/// Send `request` for `path` with a failing cache write, read the response header, then close
/// the connection's write side: a TCP half-close, which leaves the client able to read. Only
/// once the proxy has taken that half-close as the client leaving does the origin send the body
/// and finish the fill.
async fn half_close_after_response_header(
    path: &'static str,
    request: ClosedRequest,
    upstream: Upstream,
    handles_abandonment: bool,
) -> ClosedClient {
    init_proxy().await;
    let before = Before::sample(path);
    let origin = GatedOrigin::start(path, upstream).await;

    let mut downstream = TcpStream::connect("127.0.0.1:6148").await.unwrap();
    downstream
        .write_all(
            format!(
                "{}Host: close-after-response.example\r\nX-Port: {}\r\nX-Lock: true\r\n\
                 X-Record-Request-Lifecycle: true\r\nX-Cache-Fail-Write: true\r\n\
                 {}{}\r\n",
                request.start(path),
                origin.port,
                h1_opt_in_line(handles_abandonment),
                upstream.header_line(),
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    let head = read_h1_head(&mut downstream).await;
    assert!(head.starts_with(request.status_line()), "{head:?}");
    assert!(
        head.to_ascii_lowercase().contains("x-cache-status: miss"),
        "{head:?}"
    );

    downstream.shutdown().await.unwrap();
    // The proxy keeps an idle read on the client after the request, and HTTP/1's default
    // `abort_on_close` makes the half-close that read sees a `ConnectionClosed` error, which the
    // proxy ignores for the fill. The client never sends anything else, and the response header
    // it read was the last write that could fail before the body arrives, so that read is the
    // only downstream error to ignore.
    finish_fill_after_ignored_close(path, origin, &before, before.completed).await
}

/// Send an HTTP/2 `HEAD` for `path` with a failing cache write, `Content-Length: 0` and no
/// END_STREAM, and read the response, which the proxy ends right after its header while the fill
/// still runs, then reset the request. The proxy takes the empty request body as done and waits
/// on the stream, and that wait ends with the reset only after the response is complete.
async fn reset_h2_head_after_complete_response(
    path: &'static str,
    handles_abandonment: bool,
) -> ClosedClient {
    init_proxy().await;
    let before = Before::sample(path);
    let origin = GatedOrigin::start(path, Upstream::H1).await;

    let mut client = h2c_client(6154).await;
    let request = opt_in(Request::builder(), handles_abandonment)
        .method("HEAD")
        .uri(format!("http://127.0.0.1:6154{path}"))
        .header("x-port", origin.port.to_string())
        .header("x-lock", "true")
        .header("x-record-request-lifecycle", "true")
        .header("x-cache-fail-write", "true")
        .header("content-length", "0")
        .body(())
        .unwrap();
    let (response, mut request_body) = client.send_request(request, false).unwrap();
    let response = tokio::time::timeout(TEST_TIMEOUT, response)
        .await
        .expect("proxy did not send the response header")
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers().get("x-cache-status").unwrap(), "miss");
    // The proxy ends the stream in an empty DATA frame after the header.
    let mut response_body = response.into_body();
    while let Some(chunk) = tokio::time::timeout(TEST_TIMEOUT, response_body.data())
        .await
        .expect("the HEAD response stream did not end")
    {
        assert_eq!(chunk.unwrap(), Bytes::new(), "the HEAD response had a body");
    }

    request_body.send_reset(h2::Reason::CANCEL);
    finish_fill_after_ignored_close(path, origin, &before, before.completed).await
}

/// Send an HTTP/1 `HEAD` for `path` with a failing cache write, which the proxy serves by piping
/// a subrequest, and read the response header. The pipe ends once it has written the
/// subrequest's complete response, which closes the subrequest's input while the subrequest's
/// fill still runs, so the subrequest's idle read fails.
async fn pipe_head_miss(path: &'static str, handles_abandonment: bool) -> ClosedClient {
    init_proxy().await;
    let before = Before::sample(path);
    let origin = GatedOrigin::start(path, Upstream::H1).await;

    let mut downstream = TcpStream::connect("127.0.0.1:6148").await.unwrap();
    downstream
        .write_all(
            format!(
                "HEAD {path} HTTP/1.1\r\nHost: close-after-response.example\r\nX-Port: {}\r\n\
                 X-Lock: true\r\nX-Record-Request-Lifecycle: true\r\nX-Cache-Fail-Write: true\r\n\
                 X-Pipe-Subrequest: no-body\r\n{}\r\n",
                origin.port,
                h1_opt_in_line(handles_abandonment),
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    let head = read_h1_head(&mut downstream).await;
    assert!(head.starts_with("HTTP/1.1 200"), "{head:?}");
    assert!(
        head.to_ascii_lowercase().contains("x-cache-status: miss"),
        "{head:?}"
    );

    // The subrequest copies the request's headers, so both requests are recorded under `path`.
    // The parent, which only runs the pipe, completes first: the subrequest cannot finish before
    // the origin sends the body. The subrequest's record then replaces the parent's.
    wait_for_recorded_request_completion(path, before.completed).await;
    assert_eq!(
        piped_subrequest(path).and_then(|piped| piped.outcome),
        Some(Ok(()))
    );
    finish_fill_after_ignored_close(path, origin, &before, before.completed + 1).await
}

/// A client that closes after its complete response has everything it asked for, so the fill
/// still running for that response does not treat it as abandoned, and the fill's failing cache
/// writes stay as harmless to the request as they are without abandonment handling: the
/// response is served from the cache header alone, not from the failing cache body.
async fn assert_close_after_complete_response_keeps_fill(
    scenario: impl Future<Output = ClosedClient>,
) {
    let closed = bounded(scenario).await;
    assert_eq!(
        closed.abandonments, 0,
        "a client that closed after its complete response was reported as abandoning the fill \
         (request error: {:?})",
        closed.request_error
    );
    assert_eq!(closed.request_error, None);
}

/// The control for the tests above: a client that half-closes before its response body arrives
/// is reported as abandoning the fill when it opted in. Its failing cache writes fail the request
/// either way: its body was to be served from them.
async fn assert_close_before_response_body_makes_write_failure_fatal(
    path: &'static str,
    upstream: Upstream,
    handles_abandonment: bool,
) {
    let closed = bounded(half_close_after_response_header(
        path,
        ClosedRequest::Get,
        upstream,
        handles_abandonment,
    ))
    .await;
    assert_eq!(closed.abandonments, usize::from(handles_abandonment));
    assert_eq!(
        closed.request_error,
        Some(pingora_error::ErrorType::FileWriteError)
    );
}

#[tokio::test]
async fn test_client_close_after_head_miss_keeps_cache_write_failure_nonfatal() {
    assert_close_after_complete_response_keeps_fill(half_close_after_response_header(
        "/test_client_close_after_head_miss_keeps_cache_write_failure_nonfatal",
        ClosedRequest::Head,
        Upstream::H1,
        true,
    ))
    .await;
    // Without the opt-in the close is never reported, so nothing differs.
    assert_close_after_complete_response_keeps_fill(half_close_after_response_header(
        "/test_client_close_after_head_miss_keeps_cache_write_failure_nonfatal_ungated",
        ClosedRequest::Head,
        Upstream::H1,
        false,
    ))
    .await;
}

#[tokio::test]
async fn test_client_close_after_304_miss_keeps_cache_write_failure_nonfatal() {
    assert_close_after_complete_response_keeps_fill(half_close_after_response_header(
        "/test_client_close_after_304_miss_keeps_cache_write_failure_nonfatal",
        ClosedRequest::NotModified,
        Upstream::H1,
        true,
    ))
    .await;
    // Without the opt-in the close is never reported, so nothing differs.
    assert_close_after_complete_response_keeps_fill(half_close_after_response_header(
        "/test_client_close_after_304_miss_keeps_cache_write_failure_nonfatal_ungated",
        ClosedRequest::NotModified,
        Upstream::H1,
        false,
    ))
    .await;
}

#[tokio::test]
async fn test_client_close_after_head_miss_keeps_h2_upstream_cache_write_failure_nonfatal() {
    assert_close_after_complete_response_keeps_fill(half_close_after_response_header(
        "/test_client_close_after_head_miss_keeps_h2_upstream_cache_write_failure_nonfatal",
        ClosedRequest::Head,
        Upstream::H2,
        true,
    ))
    .await;
    // Without the opt-in the close is never reported, so nothing differs.
    assert_close_after_complete_response_keeps_fill(half_close_after_response_header(
        "/test_client_close_after_head_miss_keeps_h2_upstream_cache_write_failure_nonfatal_ungated",
        ClosedRequest::Head,
        Upstream::H2,
        false,
    ))
    .await;
}

#[tokio::test]
async fn test_h2_reset_after_complete_head_miss_response_keeps_cache_write_failure_nonfatal() {
    assert_close_after_complete_response_keeps_fill(reset_h2_head_after_complete_response(
        "/test_h2_reset_after_complete_head_miss_response_keeps_cache_write_failure_nonfatal",
        true,
    ))
    .await;
    // Without the opt-in the reset is never reported, so nothing differs.
    assert_close_after_complete_response_keeps_fill(reset_h2_head_after_complete_response(
        "/test_h2_reset_after_complete_head_miss_response_keeps_cache_write_failure_nonfatal_ungated",
        false,
    ))
    .await;
}

#[tokio::test]
async fn test_piped_head_miss_keeps_cache_write_failure_nonfatal() {
    assert_close_after_complete_response_keeps_fill(pipe_head_miss(
        "/test_piped_head_miss_keeps_cache_write_failure_nonfatal",
        true,
    ))
    .await;
    // Without the opt-in the pipe's end is never reported, so nothing differs.
    assert_close_after_complete_response_keeps_fill(pipe_head_miss(
        "/test_piped_head_miss_keeps_cache_write_failure_nonfatal_ungated",
        false,
    ))
    .await;
}

#[tokio::test]
async fn test_client_close_before_miss_body_makes_cache_write_failure_fatal() {
    assert_close_before_response_body_makes_write_failure_fatal(
        "/test_client_close_before_miss_body_makes_cache_write_failure_fatal",
        Upstream::H1,
        true,
    )
    .await;
    // Without the opt-in the close is not reported; the miss body's failure is fatal.
    assert_close_before_response_body_makes_write_failure_fatal(
        "/test_client_close_before_miss_body_makes_cache_write_failure_fatal_ungated",
        Upstream::H1,
        false,
    )
    .await;
}

#[tokio::test]
async fn test_client_close_before_miss_body_makes_h2_upstream_cache_write_failure_fatal() {
    assert_close_before_response_body_makes_write_failure_fatal(
        "/test_client_close_before_miss_body_makes_h2_upstream_cache_write_failure_fatal",
        Upstream::H2,
        true,
    )
    .await;
    // Without the opt-in the close is not reported; the miss body's failure is fatal.
    assert_close_before_response_body_makes_write_failure_fatal(
        "/test_client_close_before_miss_body_makes_h2_upstream_cache_write_failure_fatal_ungated",
        Upstream::H2,
        false,
    )
    .await;
}

/// An opted-in, connected client's miss whose cache body write fails before the cached response
/// header is served. As without abandonment handling, the failure only disables the cache: the
/// request then fails because the cache it was to be served from is gone, not with the storage
/// error. After abandonment the storage error ends the request instead; see
/// `test_h2_pre_admission_reset_stops_when_cache_write_fails` in pre_admission_no_fill.rs.
#[tokio::test]
async fn test_connected_miss_cache_write_failure_before_cached_header_is_not_fatal() {
    const PATH: &str = "/test_connected_miss_cache_write_failure_before_cached_header_is_not_fatal";
    init();
    // A response that ends with its header: the proxy writes its empty cache body right after
    // it starts the miss, before the cached header can be served.
    let origin = HttpOrigin::bind(|_request| async {
        Response::builder()
            .header(header::CONTENT_LENGTH, "0")
            .header(header::CACHE_CONTROL, "max-age=30")
            .body(Bytes::new())
            .unwrap()
    })
    .await
    .unwrap();
    let completed_before = recorded_request_completions(PATH);
    let abandoned_before = downstream_abandoned_during_cache_fill_calls(PATH);
    let response = abandonment_client()
        .get(format!("http://127.0.0.1:6148{PATH}"))
        .header("x-port", origin.addr().port().to_string())
        .header("x-cache-fail-write", "true")
        .header("x-record-request-lifecycle", "true")
        .timeout(TEST_TIMEOUT)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    wait_for_recorded_request_completion(PATH, completed_before).await;
    assert_eq!(
        recorded_request_error(PATH),
        Some(pingora_error::ErrorType::InternalError),
        "the cache write failure itself failed the request"
    );
    assert_eq!(
        downstream_abandoned_during_cache_fill_calls(PATH),
        abandoned_before
    );
    origin.shutdown().await;
}
