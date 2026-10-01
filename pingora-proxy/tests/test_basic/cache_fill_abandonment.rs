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
use crate::utils::server_utils::{
    downstream_abandoned_during_cache_fill_calls, downstream_abandoned_with_pending_tasks_calls,
    downstream_cache_warn_log_calls_for, forced_retry_error_hook_calls,
    forced_retry_error_hook_calls_with_retry_disabled, recorded_request_completions,
    recorded_request_error, recorded_upstream_attempts, recorded_upstream_request_filters,
    upstream_response_hook_calls,
};
use std::future::Future;

/// Opts a test request in to downstream abandonment handling in the test proxy.
const HANDLE_ABANDONMENT: (&str, &str) = ("x-handle-downstream-abandonment", "true");

/// A client whose every request opts in to downstream abandonment handling.
fn abandonment_client() -> reqwest::Client {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(HANDLE_ABANDONMENT.0, HANDLE_ABANDONMENT.1.parse().unwrap());
    reqwest::Client::builder()
        .default_headers(headers)
        .build()
        .unwrap()
}

/// A client whose requests opt in to downstream abandonment handling only if
/// `handles_abandonment` is set.
fn client_opting_in(handles_abandonment: bool) -> reqwest::Client {
    if handles_abandonment {
        abandonment_client()
    } else {
        reqwest::Client::new()
    }
}

/// Add the opt-in header to an HTTP/2 request only if `handles_abandonment` is set.
fn opt_in(request: http::request::Builder, handles_abandonment: bool) -> http::request::Builder {
    if handles_abandonment {
        request.header(HANDLE_ABANDONMENT.0, HANDLE_ABANDONMENT.1)
    } else {
        request
    }
}

/// The opt-in header line of a raw HTTP/1 request, empty unless `handles_abandonment` is set.
fn h1_opt_in_line(handles_abandonment: bool) -> &'static str {
    if handles_abandonment {
        "X-Handle-Downstream-Abandonment: true\r\n"
    } else {
        ""
    }
}

/// Fail if the request behind these response headers waited on the cache lock.
///
/// The test cache lock has a 2 s age timeout. A request that finds a lock a failed fill never
/// released waits out the rest of that age, or none of it if the lock is already older, then
/// takes the lock over and goes upstream, just as if the lock had been released. Only the wait
/// tells the two apart: any wait at all makes the test proxy report `x-cache-lock-time-ms`. Send
/// the request only after the failed request has completed, so no live writer can hold the lock.
fn assert_cache_lock_was_free(headers: &http::HeaderMap) {
    assert_eq!(
        headers.get("x-cache-lock-time-ms"),
        None,
        "the request waited on a cache lock the failed fill should have released"
    );
}

#[path = "cache_fill_abandonment/cache_states.rs"]
mod cache_states;
#[path = "cache_fill_abandonment/cache_storage_errors.rs"]
mod cache_storage_errors;
#[path = "cache_fill_abandonment/fill_cancellation.rs"]
mod fill_cancellation;
#[path = "cache_fill_abandonment/h1_early_request.rs"]
mod h1_early_request;
#[path = "cache_fill_abandonment/h1_pre_admission.rs"]
mod h1_pre_admission;
#[path = "cache_fill_abandonment/h1_request_body_errors.rs"]
mod h1_request_body_errors;
#[path = "cache_fill_abandonment/h1_upstream_errors.rs"]
mod h1_upstream_errors;
#[path = "cache_fill_abandonment/h2_admitted_fill.rs"]
mod h2_admitted_fill;
#[path = "cache_fill_abandonment/h2_blocked_request.rs"]
mod h2_blocked_request;
#[path = "cache_fill_abandonment/hook_execution.rs"]
mod hook_execution;
#[path = "cache_fill_abandonment/post_admission_errors.rs"]
mod post_admission_errors;
#[path = "cache_fill_abandonment/pre_admission.rs"]
mod pre_admission;
#[path = "cache_fill_abandonment/pre_admission_no_fill.rs"]
mod pre_admission_no_fill;
#[path = "cache_fill_abandonment/stalled_hit.rs"]
mod stalled_hit;
#[path = "cache_fill_abandonment/upstream_errors.rs"]
mod upstream_errors;
#[path = "cache_fill_abandonment/write_timeout.rs"]
mod write_timeout;
#[path = "cache_fill_abandonment/writer_demand.rs"]
mod writer_demand;

const UPSTREAM_STREAM_WINDOW_BYTES: usize = 1024;
const BLOCKING_REQUEST_BODY_BYTES: usize = 2048;
const REQUEST_BODY_PREFIX: &[u8] = b"upload..........";

/// Bound for a wait that only a hang can exceed. It costs nothing while the suite passes, and
/// the fixed-port test group runs these tests one at a time in every CI arm, so it is generous.
const TEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Bound for a request that must find the cache lock free. Those requests wait up to 10 s for
/// the lock (`x-cache-lock-wait-timeout-ms: 10000`), so a request still blocked on it fails
/// here instead of going upstream once that wait expires. Keep it well under 10 s.
const LOCK_RELEASED_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Bound for one run of a scenario, which may wait on several [`TEST_TIMEOUT`]s.
const SCENARIO_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

#[derive(Clone, Copy)]
enum CacheStorageFailure {
    Write,
    Finish,
}

impl CacheStorageFailure {
    fn request_header(self) -> &'static str {
        match self {
            Self::Write => "x-cache-fail-write",
            Self::Finish => "x-cache-fail-finish-streaming",
        }
    }
}

/// The protocol the proxy fetches a miss with, which picks the proxy loop that serves it.
#[derive(Clone, Copy)]
enum Upstream {
    H1,
    H2,
}

impl Upstream {
    /// The header line that selects this upstream in a raw HTTP/1 request.
    fn header_line(self) -> &'static str {
        match self {
            Self::H1 => "",
            Self::H2 => "X-H2: true\r\n",
        }
    }

    /// Select this upstream for an HTTP/2 request.
    fn select(self, request: http::request::Builder) -> http::request::Builder {
        match self {
            Self::H1 => request,
            Self::H2 => request.header("x-h2", "true"),
        }
    }
}

async fn h2c_client(port: u16) -> h2::client::SendRequest<Bytes> {
    let tcp = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let (client, connection) = client::handshake(tcp).await.unwrap();
    tokio::spawn(async move {
        let _ = connection.await;
    });
    client
}

async fn read_h2_body_chunk(body: &mut h2::RecvStream) -> Bytes {
    let chunk = tokio::time::timeout(TEST_TIMEOUT, body.data())
        .await
        .expect("response body chunk did not arrive")
        .expect("response ended before the next body chunk")
        .unwrap();
    body.flow_control().release_capacity(chunk.len()).unwrap();
    chunk
}

/// Read the rest of an HTTP/2 body, which must end within [`TEST_TIMEOUT`].
async fn read_h2_body(body: &mut h2::RecvStream) -> Vec<u8> {
    tokio::time::timeout(TEST_TIMEOUT, async {
        let mut received = Vec::new();
        while let Some(chunk) = body.data().await {
            let chunk = chunk.unwrap();
            body.flow_control().release_capacity(chunk.len()).unwrap();
            received.extend_from_slice(&chunk);
        }
        received
    })
    .await
    .expect("the body did not end")
}

/// Poll `condition` every 10 ms until it holds, failing with `message` after 10 s.
async fn wait_until(message: &str, mut condition: impl FnMut() -> bool) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !condition() {
        assert!(std::time::Instant::now() < deadline, "{message}");
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

async fn wait_for_cache_fill_abandonment(path: &str, previous: usize) {
    let deadline = std::time::Instant::now() + TEST_TIMEOUT;
    while downstream_abandoned_during_cache_fill_calls(path) == previous {
        assert!(
            std::time::Instant::now() < deadline,
            "proxy did not report the abandoned downstream for {path}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

async fn assert_failed_fill_recovers(path: &str) {
    let origin = HttpOrigin::bind(|_request| async {
        Response::builder()
            .header(header::CONTENT_LENGTH, "9")
            .header(header::CACHE_CONTROL, "max-age=30")
            .body(Bytes::from_static(b"recovered"))
            .unwrap()
    })
    .await
    .unwrap();
    let client = reqwest::Client::new();
    for expected_status in ["miss", "hit"] {
        let response = request_after_failed_fill(&client, path, origin.addr().port())
            .send()
            .await
            .expect("request after a failed fill remained blocked on the cache lock");
        assert_eq!(response.headers()["x-cache-status"], expected_status);
        assert_cache_lock_was_free(response.headers());
        assert_eq!(response.bytes().await.unwrap(), b"recovered".as_slice());
    }
    origin.shutdown().await;
}

/// A GET for `path` on the HTTP/1 proxy, under the cache key of the HTTP/2 requests, from the
/// origin on `origin_port`. It waits for the cache lock only until [`LOCK_RELEASED_TIMEOUT`].
fn request_after_failed_fill(
    client: &reqwest::Client,
    path: &str,
    origin_port: u16,
) -> reqwest::RequestBuilder {
    client
        .get(format!("http://127.0.0.1:6148{path}"))
        .header(header::HOST, "127.0.0.1:6154")
        .header("x-port", origin_port.to_string())
        .header("x-lock", "true")
        .header("x-cache-lock-wait-timeout-ms", "10000")
        .timeout(LOCK_RELEASED_TIMEOUT)
}

/// Run one scenario, failing if it takes longer than [`SCENARIO_TIMEOUT`].
async fn bounded<T>(scenario: impl Future<Output = T>) -> T {
    tokio::time::timeout(SCENARIO_TIMEOUT, scenario)
        .await
        .expect("the scenario did not finish")
}

/// Wait until the proxy has ignored a downstream error for `path` to keep filling the cache,
/// which it does whether or not the request handles downstream abandonment.
async fn wait_for_ignored_downstream_error(path: &str, warn_calls_before: usize) {
    wait_until(
        &format!("the proxy did not ignore a downstream error for {path}"),
        || downstream_cache_warn_log_calls_for(path) != warn_calls_before,
    )
    .await;
}

/// A request the proxy sends an HTTP/2 origin, and the handle to answer it.
type H2OriginRequest = (Request<h2::RecvStream>, h2::server::SendResponse<Bytes>);

/// Accept the proxy's HTTP/2 connection on `listener` and drive it in a task that sends every
/// request the proxy opens on it to the returned receiver. The receiver ends with the
/// connection. Once it is dropped, the task drops, and so resets, any further request.
async fn accept_h2_connection(
    listener: TcpListener,
) -> tokio::sync::mpsc::UnboundedReceiver<H2OriginRequest> {
    let (io, _) = tokio::time::timeout(TEST_TIMEOUT, listener.accept())
        .await
        .expect("the proxy did not connect to the HTTP/2 origin")
        .unwrap();
    let mut connection = h2::server::handshake(io).await.unwrap();
    let (requests_tx, requests) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(Ok(request)) = connection.accept().await {
            let _ = requests_tx.send(request);
        }
    });
    requests
}

/// The next request the proxy sends on an HTTP/2 origin connection.
async fn next_h2_request(
    requests: &mut tokio::sync::mpsc::UnboundedReceiver<H2OriginRequest>,
) -> H2OriginRequest {
    tokio::time::timeout(TEST_TIMEOUT, requests.recv())
        .await
        .expect("the proxy did not send the HTTP/2 origin a request")
        .expect("the HTTP/2 origin connection closed before the request")
}

/// Accept the proxy's HTTP/2 connection on `listener` and its first request. The connection
/// stays driven, and any later request on it is reset.
async fn accept_h2_request(listener: TcpListener) -> H2OriginRequest {
    next_h2_request(&mut accept_h2_connection(listener).await).await
}

async fn block_upstream_write_with_flow_control(
    request_body: &mut h2::SendStream<Bytes>,
    upstream_window_filled: tokio::sync::oneshot::Receiver<()>,
) {
    use std::future::poll_fn;

    request_body.reserve_capacity(BLOCKING_REQUEST_BODY_BYTES);
    let granted = tokio::time::timeout(TEST_TIMEOUT, poll_fn(|cx| request_body.poll_capacity(cx)))
        .await
        .expect("proxy never granted request body capacity")
        .unwrap()
        .unwrap();
    assert!(
        granted >= BLOCKING_REQUEST_BODY_BYTES,
        "only granted {granted} bytes"
    );
    request_body
        .send_data(Bytes::from(vec![0u8; BLOCKING_REQUEST_BODY_BYTES]), false)
        .unwrap();

    tokio::time::timeout(TEST_TIMEOUT, upstream_window_filled)
        .await
        .expect("upstream did not consume its stream window")
        .unwrap();
}

async fn fill_upstream_window_without_releasing(
    request_body: &mut h2::RecvStream,
    window_bytes: usize,
) {
    let mut received = 0;
    while received < window_bytes {
        let data = request_body
            .data()
            .await
            .expect("downstream request ended before filling the upstream window")
            .unwrap();
        received += data.len();
    }
    assert_eq!(received, window_bytes);
}

/// Poll with plain requests, which do not opt in, until `uri` is a cache hit, and return its body.
///
/// A poll that arrives before admission becomes its own miss and is dropped. Were it opted in,
/// dropping it could report an abandonment for the path under test.
async fn wait_for_h2_cache_hit(uri: &str, upstream_port: u16) -> Vec<u8> {
    use std::time::{Duration, Instant};

    let deadline = Instant::now() + TEST_TIMEOUT;
    loop {
        assert!(
            Instant::now() < deadline,
            "cache admission did not complete for {uri}"
        );
        let mut client = h2c_client(6154).await;
        let request = Request::builder()
            .method("GET")
            .uri(uri)
            .header("x-port", upstream_port.to_string())
            .body(())
            .unwrap();
        let (response, _) = client.send_request(request, true).unwrap();
        if let Ok(Ok(response)) = tokio::time::timeout(Duration::from_secs(1), response).await {
            if response.status() == StatusCode::OK
                && response
                    .headers()
                    .get("x-cache-status")
                    .is_some_and(|status| status == "hit")
            {
                let mut body = response.into_body();
                return read_h2_body(&mut body).await;
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn wait_for_recorded_request_completion(path: &str, completed_before: usize) {
    wait_until(
        &format!("instrumented request did not finish for {path}"),
        || recorded_request_completions(path) != completed_before,
    )
    .await;
    assert_eq!(recorded_request_completions(path), completed_before + 1);
}

async fn wait_for_upstream_response_headers(path: &str, expected: usize) {
    wait_until(
        &format!("upstream response header hook did not run for {path}"),
        || upstream_response_hook_calls(path).headers >= expected,
    )
    .await;
    assert_eq!(upstream_response_hook_calls(path).headers, expected);
}

/// A client resets its stream during a miss whose cache storage fails as `failure` says, then
/// the origin finishes the response. The request is recorded as abandoning the fill only if
/// `handles_abandonment`, and the failed fill leaves the cache lock free.
async fn assert_abandoned_storage_failure(
    path: &str,
    failure: CacheStorageFailure,
    upstream: Upstream,
    handles_abandonment: bool,
) {
    init();
    // A failed finish sends "first", which the client reads before it resets, with the header.
    // Either response ends with "last" once `finish` fires.
    let first: &'static [u8] = match failure {
        CacheStorageFailure::Write => b"",
        CacheStorageFailure::Finish => b"first",
    };
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_port = listener.local_addr().unwrap().port();
    let (finish, finish_rx) = tokio::sync::oneshot::channel::<()>();
    let origin = tokio::spawn(async move {
        match upstream {
            Upstream::H1 => {
                let (mut socket, _) = listener.accept().await.unwrap();
                read_h1_head(&mut socket).await;
                let header = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nCache-Control: max-age=30\r\n\r\n",
                    first.len() + 4
                );
                socket.write_all(header.as_bytes()).await.unwrap();
                socket.write_all(first).await.unwrap();
                finish_rx.await.unwrap();
                socket.write_all(b"last").await.unwrap();
            }
            Upstream::H2 => {
                let (_request, mut respond) = accept_h2_request(listener).await;
                let header = Response::builder()
                    .header(header::CACHE_CONTROL, "max-age=30")
                    .body(())
                    .unwrap();
                let mut body = respond.send_response(header, false).unwrap();
                if !first.is_empty() {
                    body.send_data(Bytes::from_static(first), false).unwrap();
                }
                finish_rx.await.unwrap();
                body.send_data(Bytes::from_static(b"last"), true).unwrap();
            }
        }
    });

    let mut client = h2c_client(6154).await;
    let attempts_before = recorded_upstream_attempts(path);
    let completed_before = recorded_request_completions(path);
    let request = opt_in(upstream.select(Request::builder()), handles_abandonment)
        .method("GET")
        .uri(format!("http://127.0.0.1:6154{path}"))
        .header("x-port", upstream_port.to_string())
        .header("x-lock", "true")
        .header(failure.request_header(), "true")
        .header("x-record-request-lifecycle", "true")
        .body(())
        .unwrap();
    let (response, mut request_body) = client.send_request(request, true).unwrap();
    let response = response.await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["x-cache-status"], "miss");
    let mut response_body = response.into_body();
    if !first.is_empty() {
        assert_eq!(read_h2_body_chunk(&mut response_body).await, first);
    }

    let abandoned_before = downstream_abandoned_during_cache_fill_calls(path);
    let warn_calls_before = downstream_cache_warn_log_calls_for(path);
    request_body.send_reset(h2::Reason::CANCEL);
    wait_for_ignored_downstream_error(path, warn_calls_before).await;
    assert_eq!(
        downstream_abandoned_during_cache_fill_calls(path),
        abandoned_before + usize::from(handles_abandonment)
    );
    finish.send(()).unwrap();
    wait_for_recorded_request_completion(path, completed_before).await;
    // The client already has the cached response header, so the fill is serving it a miss body,
    // and a failed write loses the cache that body comes from: it fails the request whether or
    // not the downstream was abandoned. Finishing the admission stays best effort: the cache is
    // disabled and the request still succeeds.
    let expected_error = match failure {
        CacheStorageFailure::Write => Some(pingora_error::ErrorType::FileWriteError),
        CacheStorageFailure::Finish => None,
    };
    assert_eq!(recorded_request_error(path), expected_error);
    assert_eq!(recorded_upstream_attempts(path), attempts_before + 1);
    assert_eq!(
        downstream_abandoned_during_cache_fill_calls(path),
        abandoned_before + usize::from(handles_abandonment),
        "cache storage failure must not repeat the callback"
    );
    tokio::time::timeout(TEST_TIMEOUT, origin)
        .await
        .expect("origin task did not finish")
        .unwrap();
    assert_failed_fill_recovers(path).await;
}
