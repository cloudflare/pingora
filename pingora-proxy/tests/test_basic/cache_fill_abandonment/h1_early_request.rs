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

//! An HTTP/1 client that sends its next request before the proxy has finished the current one,
//! with pipelining off. The client is still connected, so opting in to downstream abandonment
//! handling must change nothing it can observe.

use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

const CACHE_HOST: &str = "early-request.example";

/// The origin's cacheable 12-byte response, sent as [`BODY_START`] with the header and
/// [`BODY_REST`] once the test releases it.
const BODY_START: &[u8] = b"hello ";
const BODY_REST: &[u8] = b"world!";

/// The request whose response is still being produced when the client sends its next one.
#[derive(Clone, Copy)]
enum FirstRequest {
    /// A `GET`, whose next request arrives after [`BODY_START`] and before [`BODY_REST`].
    Get,
    /// A `HEAD`, whose next request arrives after its complete response while the fill still
    /// waits for [`BODY_REST`].
    Head,
}

/// What the client and the proxy observed for one run.
#[derive(Debug, PartialEq)]
struct EarlyRequestOutcome {
    /// The first response's status line.
    status_line: String,
    /// The first response's `x-cache-status`, empty if it had none.
    cache_status: String,
    /// The first response's body as received before the connection ended or the body was
    /// complete.
    body: Vec<u8>,
    /// The next request's status line, or `None` if the connection closed without serving it.
    next_status_line: Option<String>,
    /// The first request's final proxy error.
    request_error: Option<pingora_error::ErrorType>,
    /// Abandonment hook calls for the first request.
    abandonments: usize,
    /// Requests the origin behind the next request received.
    next_origin_requests: usize,
    /// The body a later request got from the cache, or `None` if it was not a cache hit.
    cached_body: Option<Vec<u8>>,
}

/// Read one HTTP/1 response head byte by byte, so none of its body is consumed. Returns the head
/// received so far if the connection ends first.
async fn read_response_head(stream: &mut TcpStream) -> String {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        match tokio::time::timeout(TEST_TIMEOUT, stream.read(&mut byte))
            .await
            .expect("response head did not arrive")
        {
            Ok(1) => head.push(byte[0]),
            _ => break,
        }
    }
    String::from_utf8_lossy(&head).into_owned()
}

/// The value of the `name` field in a response head, empty if the head has no such field.
fn header_value(head: &str, name: &str) -> String {
    head.lines()
        .filter_map(|line| line.split_once(':'))
        .find(|(field, _)| field.trim().eq_ignore_ascii_case(name))
        .map(|(_, value)| value.trim().to_owned())
        .unwrap_or_default()
}

/// Read body bytes until `limit` arrived or the connection ended.
async fn read_body_up_to(stream: &mut TcpStream, body: &mut Vec<u8>, limit: usize) {
    let mut buf = [0u8; 64];
    while body.len() < limit {
        let want = (limit - body.len()).min(buf.len());
        match tokio::time::timeout(TEST_TIMEOUT, stream.read(&mut buf[..want]))
            .await
            .expect("the connection neither sent the body nor closed")
        {
            Ok(0) | Err(_) => break,
            Ok(read) => body.extend_from_slice(&buf[..read]),
        }
    }
}

async fn run_early_next_request(
    path: &'static str,
    first: FirstRequest,
    handles_abandonment: bool,
) -> EarlyRequestOutcome {
    init();

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_port = listener.local_addr().unwrap().port();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
    let upstream = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let request = read_h1_head(&mut socket).await;
        // Whatever the client asked for, the fill is a GET.
        assert!(
            request.starts_with(&format!("GET {path} HTTP/1.1")),
            "{request:?}"
        );
        socket
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Length: 12\r\nCache-Control: max-age=30\r\n\r\n",
            )
            .await
            .unwrap();
        socket.write_all(BODY_START).await.unwrap();
        release_rx.await.unwrap();
        socket.write_all(BODY_REST).await.unwrap();
        socket
    });
    let next_origin_requests = Arc::new(AtomicUsize::new(0));
    let counted = next_origin_requests.clone();
    let next_origin = HttpOrigin::bind(move |_request| {
        counted.fetch_add(1, Ordering::Relaxed);
        async {
            Response::builder()
                .header(header::CONTENT_LENGTH, "4")
                .body(Bytes::from_static(b"next"))
                .unwrap()
        }
    })
    .await
    .unwrap();

    let completed_before = recorded_request_completions(path);
    let abandoned_before = downstream_abandoned_during_cache_fill_calls(path);
    let warn_logs_before = downstream_cache_warn_log_calls_for(path);
    let method = match first {
        FirstRequest::Get => "GET",
        FirstRequest::Head => "HEAD",
    };
    let opt_in = h1_opt_in_line(handles_abandonment);
    let mut downstream = TcpStream::connect("127.0.0.1:6148").await.unwrap();
    downstream
        .write_all(
            format!(
                "{method} {path} HTTP/1.1\r\nHost: {CACHE_HOST}\r\nX-Port: {upstream_port}\r\n\
                 X-Record-Request-Lifecycle: true\r\nX-Test-Upstream-Response-Hooks: true\r\n{opt_in}\r\n"
            )
            .as_bytes(),
        )
        .await
        .unwrap();

    // A HEAD response is complete with its header, so the client reads it first. A GET's
    // response may still be on its way to the client, so wait for the proxy to have the origin's
    // header and the start of its body.
    let mut head = String::new();
    if matches!(first, FirstRequest::Head) {
        head = read_response_head(&mut downstream).await;
    } else {
        wait_until(
            &format!("the proxy did not receive the start of the response for {path}"),
            || upstream_response_hook_calls(path).body_chunks > 0,
        )
        .await;
    }

    downstream
        .write_all(
            format!(
                "GET {path}/next HTTP/1.1\r\nHost: {CACHE_HOST}\r\nX-Port: {}\r\n\
                 X-Bypass-Cache: true\r\n\r\n",
                next_origin.addr().port()
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    // Both runs read that request start at the same point and ignore the resulting error for the
    // fill, which the test proxy counts.
    wait_until(
        &format!("the proxy did not read the early next request for {path}"),
        || downstream_cache_warn_log_calls_for(path) != warn_logs_before,
    )
    .await;
    release_tx.send(()).unwrap();

    let mut body = Vec::new();
    if matches!(first, FirstRequest::Get) {
        head = read_response_head(&mut downstream).await;
        read_body_up_to(
            &mut downstream,
            &mut body,
            BODY_START.len() + BODY_REST.len(),
        )
        .await;
    }
    let status_line = head.lines().next().unwrap_or_default().to_owned();
    let cache_status = header_value(&head, "x-cache-status");
    let next_head = read_response_head(&mut downstream).await;
    let next_status_line = next_head.lines().next().map(str::to_owned);
    wait_for_recorded_request_completion(path, completed_before).await;
    drop(upstream.await.unwrap());
    next_origin.shutdown().await;

    // The origin no longer accepts connections, so only a cache hit can succeed.
    let later = reqwest::Client::new()
        .get(format!("http://127.0.0.1:6148{path}"))
        .header(header::HOST, CACHE_HOST)
        .header("x-port", upstream_port.to_string())
        .timeout(TEST_TIMEOUT)
        .send()
        .await
        .unwrap();
    let cached_body = if later
        .headers()
        .get("x-cache-status")
        .is_some_and(|status| status == "hit")
    {
        Some(later.bytes().await.unwrap().to_vec())
    } else {
        None
    };

    EarlyRequestOutcome {
        status_line,
        cache_status,
        body,
        next_status_line,
        request_error: recorded_request_error(path),
        abandonments: downstream_abandoned_during_cache_fill_calls(path) - abandoned_before,
        next_origin_requests: next_origin_requests.load(Ordering::Relaxed),
        cached_body,
    }
}

async fn assert_early_next_request_matches_opt_out(
    opted_in_path: &'static str,
    opted_out_path: &'static str,
    first: FirstRequest,
) -> EarlyRequestOutcome {
    let run = async {
        let opted_out = run_early_next_request(opted_out_path, first, false).await;
        let opted_in = run_early_next_request(opted_in_path, first, true).await;
        assert_eq!(
            opted_in, opted_out,
            "the early next request changed what the opted-in client observed"
        );
        opted_out
    };
    tokio::time::timeout(std::time::Duration::from_secs(60), run)
        .await
        .expect("early next request scenario did not finish")
}

// The early request ends delivery to the client as it does without the opt-in: the client keeps
// the part of the body the proxy had sent, the connection closes without serving the next
// request, and the fill completes.
#[tokio::test]
async fn test_h1_early_next_request_during_miss_body_matches_opt_out() {
    let outcome = assert_early_next_request_matches_opt_out(
        "/test_h1_early_next_request_during_miss_body_opted_in",
        "/test_h1_early_next_request_during_miss_body_opted_out",
        FirstRequest::Get,
    )
    .await;
    assert_eq!(outcome.status_line, "HTTP/1.1 200 OK");
    assert_eq!(outcome.cache_status, "miss");
    assert_eq!(outcome.body, BODY_START);
    assert_eq!(outcome.next_status_line, None);
    assert_eq!(outcome.request_error, None);
    assert_eq!(outcome.abandonments, 0);
    assert_eq!(outcome.next_origin_requests, 0);
    assert_eq!(
        outcome.cached_body.as_deref(),
        Some(b"hello world!".as_slice())
    );
}

// A next request sent after the complete response of a HEAD miss whose fill is still running is
// read, and handled, as it is without the opt-in: the connection closes without serving it, and
// the fill completes without being abandoned.
#[tokio::test]
async fn test_h1_early_next_request_after_complete_head_response_matches_opt_out() {
    let outcome = assert_early_next_request_matches_opt_out(
        "/test_h1_early_next_request_after_complete_head_response_opted_in",
        "/test_h1_early_next_request_after_complete_head_response_opted_out",
        FirstRequest::Head,
    )
    .await;
    assert_eq!(outcome.status_line, "HTTP/1.1 200 OK");
    assert_eq!(outcome.cache_status, "miss");
    assert_eq!(outcome.next_status_line, None);
    assert_eq!(outcome.abandonments, 0);
    assert_eq!(outcome.next_origin_requests, 0);
    assert_eq!(
        outcome.cached_body.as_deref(),
        Some(b"hello world!".as_slice())
    );
}
