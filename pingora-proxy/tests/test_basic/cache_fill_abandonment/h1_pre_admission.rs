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

/// A client can close before its declared request body is complete. An upstream failure after
/// that close must not cause the proxy to retry with the truncated body as a finished request.
#[tokio::test]
async fn test_h1_incomplete_abandoned_request_is_not_retried() {
    bounded(async {
        init();
        const PATH: &str = "/test_h1_incomplete_abandoned_request_is_not_retried";
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_port = listener.local_addr().unwrap().port();
        let (request_seen_tx, request_seen_rx) = tokio::sync::oneshot::channel();
        let (fail_upstream_tx, fail_upstream_rx) = tokio::sync::oneshot::channel();
        let upstream = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            drop(listener);
            read_h1_head(&mut socket).await;
            request_seen_tx.send(()).unwrap();
            fail_upstream_rx.await.unwrap();
            drop(socket);
        });

        let attempts_before = recorded_upstream_attempts(PATH);
        let completed_before = recorded_request_completions(PATH);
        let abandoned_before = downstream_abandoned_during_cache_fill_calls(PATH);
        let ignored_error_before = downstream_cache_warn_log_calls_for(PATH);
        let retry_hook_before = forced_retry_error_hook_calls(PATH);

        let mut downstream = TcpStream::connect("127.0.0.1:6148").await.unwrap();
        downstream
            .write_all(
                format!(
                    "GET {PATH} HTTP/1.1\r\nHost: 127.0.0.1:6154\r\n\
                     {}X-Port: {upstream_port}\r\nX-Lock: true\r\n\
                     X-Record-Request-Lifecycle: true\r\n\
                     X-Force-Retry-In-Error-Hook: true\r\n\
                     Content-Length: 1024\r\n\r\nbody prefix",
                    h1_opt_in_line(true)
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        request_seen_rx.await.unwrap();
        drop(downstream);
        wait_for_ignored_downstream_error(PATH, ignored_error_before).await;
        assert_eq!(
            downstream_abandoned_during_cache_fill_calls(PATH),
            abandoned_before + 1
        );

        fail_upstream_tx.send(()).unwrap();
        upstream.await.unwrap();
        wait_for_recorded_request_completion(PATH, completed_before).await;
        assert_eq!(
            forced_retry_error_hook_calls(PATH),
            retry_hook_before + 1,
            "the first upstream failure must reach the retry hook"
        );
        assert_eq!(
            recorded_upstream_attempts(PATH),
            attempts_before + 1,
            "an incomplete abandoned request must not be replayed"
        );
    })
    .await;
}

/// An HTTP/1 client sends part of a `GET` body and leaves before the response header. The origin
/// then sends a 1xx and the start of a cacheable response before closing early. The retry hook
/// makes every error retryable, and the origin refuses any retried attempt.
async fn assert_h1_pre_admission_abandonment_propagates_truncated_fill(
    path: &'static str,
    handles_abandonment: bool,
) {
    init();

    // The cache key of the HTTP/2 requests, which `assert_failed_fill_recovers` uses.
    const CACHE_HOST: &str = "127.0.0.1:6154";

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_port = listener.local_addr().unwrap().port();
    let (request_seen_tx, request_seen_rx) = tokio::sync::oneshot::channel();
    let (send_early_hints_tx, send_early_hints_rx) = tokio::sync::oneshot::channel();
    let (send_final_response_tx, send_final_response_rx) = tokio::sync::oneshot::channel();
    let (final_response_started_tx, final_response_started_rx) = tokio::sync::oneshot::channel();
    let (truncate_response_tx, truncate_response_rx) = tokio::sync::oneshot::channel();

    let upstream = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        drop(listener);
        read_h1_head(&mut socket).await;
        request_seen_tx.send(()).unwrap();

        send_early_hints_rx.await.unwrap();
        socket
            .write_all(b"HTTP/1.1 103 Early Hints\r\nLink: </asset.css>; rel=preload\r\n\r\n")
            .await
            .unwrap();

        // Without the opt-in the proxy has dropped this connection by now.
        send_final_response_rx.await.unwrap();
        let _ = socket
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Length: 12\r\nCache-Control: max-age=30\r\n\r\nhello",
            )
            .await;
        final_response_started_tx.send(()).unwrap();

        truncate_response_rx.await.unwrap();
        let _ = socket.shutdown().await;
    });

    let abandoned_calls_before = downstream_abandoned_during_cache_fill_calls(path);
    let warn_calls_before = downstream_cache_warn_log_calls_for(path);
    let attempts_before = recorded_upstream_attempts(path);
    let completed_before = recorded_request_completions(path);
    let upstream_header_calls_before = upstream_response_hook_calls(path).headers;
    let forced_retry_calls_before = forced_retry_error_hook_calls(path);
    let nonretryable_hook_calls_before = forced_retry_error_hook_calls_with_retry_disabled(path);

    let mut downstream = TcpStream::connect("127.0.0.1:6148").await.unwrap();
    downstream
        .write_all(
            format!(
                "GET {path} HTTP/1.1\r\nHost: {CACHE_HOST}\r\n{}X-Port: {upstream_port}\r\n\
                 X-Lock: true\r\nX-Cache-Lock-Wait-Timeout-Ms: 10000\r\n\
                 X-Record-Request-Lifecycle: true\r\n\
                 X-Test-Upstream-Response-Hooks: true\r\n\
                 X-Fail-Downstream-Informational-Filter-After-Abandonment: true\r\n\
                 X-Force-Retry-In-Error-Hook: true\r\nContent-Length: 1024\r\n\r\n\
                 body prefix",
                h1_opt_in_line(handles_abandonment)
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    request_seen_rx.await.unwrap();
    drop(downstream);

    wait_for_ignored_downstream_error(path, warn_calls_before).await;
    assert_eq!(
        downstream_abandoned_during_cache_fill_calls(path),
        abandoned_calls_before + usize::from(handles_abandonment),
        "the downstream failure did not run the callback immediately"
    );

    send_early_hints_tx.send(()).unwrap();
    wait_for_upstream_response_headers(path, upstream_header_calls_before + 1).await;
    if !handles_abandonment {
        // The 1xx went on to the downstream response filter, whose failure ended the attempt, and
        // the retry the hook forced could not connect.
        wait_for_recorded_request_completion(path, completed_before).await;
    }
    send_final_response_tx.send(()).unwrap();
    final_response_started_rx.await.unwrap();
    if handles_abandonment {
        wait_for_upstream_response_headers(path, upstream_header_calls_before + 2).await;
        assert_eq!(
            downstream_abandoned_during_cache_fill_calls(path),
            abandoned_calls_before + 1,
            "cache admission repeated the cache-fill callback"
        );
        assert_eq!(
            recorded_request_completions(path),
            completed_before,
            "the abandoned request completed before the upstream body failed"
        );
        assert_eq!(
            forced_retry_error_hook_calls(path),
            forced_retry_calls_before,
            "the retry hook ran before the upstream body failed"
        );
    }

    truncate_response_tx.send(()).unwrap();
    upstream.await.unwrap();
    if handles_abandonment {
        wait_for_recorded_request_completion(path, completed_before).await;
    }
    // After abandonment the truncated fill is final. Without the opt-in the retry hook's retry of
    // the failed 1xx filter is allowed and fails to connect.
    let (expected_error, expected_attempts) = if handles_abandonment {
        (pingora_error::ErrorType::ConnectionClosed, 1)
    } else {
        (pingora_error::ErrorType::ConnectRefused, 2)
    };
    assert_eq!(recorded_request_error(path), Some(expected_error));
    assert_eq!(
        forced_retry_error_hook_calls(path),
        forced_retry_calls_before + 1,
        "the failed attempt did not reach the retry hook"
    );
    assert_eq!(
        forced_retry_error_hook_calls_with_retry_disabled(path),
        nonretryable_hook_calls_before + 1,
        "the failed attempt's error was already retryable when the hook ran"
    );
    assert_eq!(
        recorded_upstream_attempts(path),
        attempts_before + expected_attempts,
        "unexpected number of upstream attempts"
    );
    assert_eq!(
        downstream_abandoned_during_cache_fill_calls(path),
        abandoned_calls_before + usize::from(handles_abandonment),
        "the upstream failure repeated the cache-fill callback"
    );

    assert_failed_fill_recovers(path).await;
}

#[tokio::test]
async fn test_h1_pre_admission_abandonment_propagates_truncated_fill() {
    bounded(
        assert_h1_pre_admission_abandonment_propagates_truncated_fill(
            "/test_h1_pre_admission_abandonment_propagates_truncated_fill",
            true,
        ),
    )
    .await;
    // Without the opt-in the 1xx reaches the failing filter and the retry is allowed.
    bounded(assert_h1_pre_admission_abandonment_propagates_truncated_fill(
        "/test_h1_pre_admission_abandonment_propagates_truncated_fill_without_abandonment_handling",
        false,
    ))
    .await;
}

/// An HTTP/1 client sends a complete cacheable `GET` and leaves before the response header.
async fn assert_h1_complete_get_abandonment_notifies_before_upstream_response(
    path: &'static str,
    handles_abandonment: bool,
) {
    init();

    const CACHE_HOST: &str = "complete-get.example";

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_port = listener.local_addr().unwrap().port();
    let (request_seen_tx, request_seen_rx) = tokio::sync::oneshot::channel();
    let (send_response_tx, send_response_rx) = tokio::sync::oneshot::channel();

    let upstream = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        read_h1_head(&mut socket).await;
        request_seen_tx.send(()).unwrap();

        send_response_rx.await.unwrap();
        socket
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nCache-Control: max-age=30\r\n\r\nbody",
            )
            .await
            .unwrap();
    });

    let abandoned_calls_before = downstream_abandoned_during_cache_fill_calls(path);
    let warn_calls_before = downstream_cache_warn_log_calls_for(path);
    let attempts_before = recorded_upstream_attempts(path);
    let completed_before = recorded_request_completions(path);

    let mut downstream = TcpStream::connect("127.0.0.1:6148").await.unwrap();
    downstream
        .write_all(
            format!(
                "GET {path} HTTP/1.1\r\nHost: {CACHE_HOST}\r\n{}X-Port: {upstream_port}\r\n\
                 X-Record-Request-Lifecycle: true\r\n\r\n",
                h1_opt_in_line(handles_abandonment)
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    request_seen_rx.await.unwrap();
    drop(downstream);

    wait_for_ignored_downstream_error(path, warn_calls_before).await;
    assert_eq!(
        downstream_abandoned_during_cache_fill_calls(path),
        abandoned_calls_before + usize::from(handles_abandonment),
        "the abandonment callback did not run exactly once before the upstream response"
    );
    assert_eq!(
        recorded_request_completions(path),
        completed_before,
        "the proxy completed while the upstream response was withheld"
    );

    send_response_tx.send(()).unwrap();
    upstream.await.unwrap();
    wait_for_recorded_request_completion(path, completed_before).await;
    assert_eq!(recorded_request_error(path), None);
    assert_eq!(
        downstream_abandoned_during_cache_fill_calls(path),
        abandoned_calls_before + usize::from(handles_abandonment),
        "cache admission repeated the abandonment callback"
    );
    assert_eq!(
        recorded_upstream_attempts(path),
        attempts_before + 1,
        "the complete request unexpectedly retried"
    );

    let cached = reqwest::Client::new()
        .get(format!("http://127.0.0.1:6148{path}"))
        .header(header::HOST, CACHE_HOST)
        .header("x-port", upstream_port.to_string())
        .timeout(TEST_TIMEOUT)
        .send()
        .await
        .unwrap();
    assert_eq!(cached.status(), StatusCode::OK);
    assert_eq!(cached.headers().get("x-cache-status").unwrap(), "hit");
    assert_eq!(cached.bytes().await.unwrap(), b"body".as_slice());
}

#[tokio::test]
async fn test_h1_complete_get_abandonment_notifies_before_upstream_response() {
    assert_h1_complete_get_abandonment_notifies_before_upstream_response(
        "/test_h1_complete_get_abandonment_notifies_before_upstream_response",
        true,
    )
    .await;
    // Without the opt-in the fill completes the same, just without the callback.
    assert_h1_complete_get_abandonment_notifies_before_upstream_response(
        "/test_h1_complete_get_abandonment_notifies_before_upstream_response_without_abandonment_handling",
        false,
    )
    .await;
}

/// An HTTP/1 client sends a complete `GET` and leaves before the header of a response the origin
/// then marks uncacheable, withholding its body until the test releases it.
async fn assert_h1_pre_admission_abandonment_stops_uncacheable_response(
    path: &'static str,
    handles_abandonment: bool,
) {
    init();

    const CACHE_HOST: &str = "uncacheable-after-abandonment.example";

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_port = listener.local_addr().unwrap().port();
    let (request_seen_tx, request_seen_rx) = tokio::sync::oneshot::channel();
    let (send_response_tx, send_response_rx) = tokio::sync::oneshot::channel();
    let (response_sent_tx, response_sent_rx) = tokio::sync::oneshot::channel();
    let (send_body_tx, send_body_rx) = tokio::sync::oneshot::channel();

    let upstream = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        read_h1_head(&mut socket).await;
        request_seen_tx.send(()).unwrap();

        send_response_rx.await.unwrap();
        socket
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nCache-Control: no-store\r\n\r\n")
            .await
            .unwrap();
        response_sent_tx.send(()).unwrap();

        send_body_rx.await.unwrap();
        let _ = socket.write_all(b"body").await;
    });

    let abandoned_calls_before = downstream_abandoned_during_cache_fill_calls(path);
    let warn_calls_before = downstream_cache_warn_log_calls_for(path);
    let attempts_before = recorded_upstream_attempts(path);
    let completed_before = recorded_request_completions(path);

    let mut downstream = TcpStream::connect("127.0.0.1:6148").await.unwrap();
    downstream
        .write_all(
            format!(
                "GET {path} HTTP/1.1\r\nHost: {CACHE_HOST}\r\n{}X-Port: {upstream_port}\r\n\
                 X-Record-Request-Lifecycle: true\r\n\r\n",
                h1_opt_in_line(handles_abandonment)
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    request_seen_rx.await.unwrap();
    drop(downstream);

    wait_for_ignored_downstream_error(path, warn_calls_before).await;
    assert_eq!(
        downstream_abandoned_during_cache_fill_calls(path),
        abandoned_calls_before + usize::from(handles_abandonment),
        "the complete H1 request did not report downstream abandonment"
    );

    send_response_tx.send(()).unwrap();
    response_sent_rx.await.unwrap();
    if handles_abandonment {
        // The response can no longer reach anyone, so the request ends without its body.
        wait_for_recorded_request_completion(path, completed_before).await;
    }
    send_body_tx.send(()).unwrap();
    upstream.await.unwrap();
    if !handles_abandonment {
        // The response goes on to the closed connection, so the request only ends with its body.
        wait_for_recorded_request_completion(path, completed_before).await;
    }
    assert_eq!(
        recorded_request_error(path),
        handles_abandonment.then_some(pingora_error::ErrorType::WriteError)
    );
    assert_eq!(recorded_upstream_attempts(path), attempts_before + 1);
    assert_eq!(
        downstream_abandoned_during_cache_fill_calls(path),
        abandoned_calls_before + usize::from(handles_abandonment),
        "classifying the response repeated the abandonment callback"
    );
}

#[tokio::test]
async fn test_h1_pre_admission_abandonment_stops_uncacheable_response() {
    assert_h1_pre_admission_abandonment_stops_uncacheable_response(
        "/test_h1_pre_admission_abandonment_stops_uncacheable_response",
        true,
    )
    .await;
    // Without the opt-in the response still goes to the closed connection, body and all.
    assert_h1_pre_admission_abandonment_stops_uncacheable_response(
        "/test_h1_pre_admission_abandonment_stops_uncacheable_response_without_abandonment_handling",
        false,
    )
    .await;
}

/// An HTTP/2 client sends part of a `POST` body to an HTTP/1 origin and resets its stream before
/// the response header. The origin then sends a 1xx and a cacheable response in two parts.
async fn assert_h1_downstream_abandonment_before_cache_admission_completes_fill(
    path: &'static str,
    handles_abandonment: bool,
) {
    use std::future::poll_fn;

    init();

    let uri = format!("http://127.0.0.1:6154{path}");
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_port = listener.local_addr().unwrap().port();
    let (request_seen_tx, request_seen_rx) = tokio::sync::oneshot::channel();
    let (send_early_hints_tx, send_early_hints_rx) = tokio::sync::oneshot::channel();
    let (send_final_response_tx, send_final_response_rx) = tokio::sync::oneshot::channel();
    let (response_started_tx, response_started_rx) = tokio::sync::oneshot::channel();
    let (finish_response_tx, finish_response_rx) = tokio::sync::oneshot::channel();
    let (done_tx, done_rx) = tokio::sync::oneshot::channel();

    let upstream = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        read_h1_head(&mut socket).await;
        request_seen_tx.send(()).unwrap();
        send_early_hints_rx.await.unwrap();

        socket
            .write_all(b"HTTP/1.1 103 Early Hints\r\nLink: </asset.css>; rel=preload\r\n\r\n")
            .await
            .unwrap();
        send_final_response_rx.await.unwrap();

        // Without the opt-in the proxy has dropped this connection by now.
        let _ = socket
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Length: 12\r\nCache-Control: max-age=30\r\n\r\nhello ",
            )
            .await;
        response_started_tx.send(()).unwrap();
        finish_response_rx.await.unwrap();
        let _ = socket.write_all(b"world!").await;
        let _ = done_rx.await;
    });

    let mut client = h2c_client(6154).await;
    let attempts_before = recorded_upstream_attempts(path);
    let completed_before = recorded_request_completions(path);
    let upstream_header_calls_before = upstream_response_hook_calls(path).headers;
    let request = opt_in(Request::builder(), handles_abandonment)
        .method("POST")
        .uri(uri.as_str())
        .header("x-port", upstream_port.to_string())
        .header("x-lock", "true")
        .header("x-record-request-lifecycle", "true")
        .header("x-test-upstream-response-hooks", "true")
        .header(
            "x-fail-downstream-informational-filter-after-abandonment",
            "true",
        )
        .body(())
        .unwrap();
    let (response, mut request_body) = client.send_request(request, false).unwrap();
    request_body.reserve_capacity(11);
    poll_fn(|cx| request_body.poll_capacity(cx))
        .await
        .unwrap()
        .unwrap();
    request_body
        .send_data(Bytes::from_static(b"body prefix"), false)
        .unwrap();
    request_seen_rx.await.unwrap();

    let abandoned_calls_before = downstream_abandoned_during_cache_fill_calls(path);
    let warn_calls_before = downstream_cache_warn_log_calls_for(path);
    request_body.send_reset(h2::Reason::CANCEL);
    wait_for_ignored_downstream_error(path, warn_calls_before).await;
    assert_eq!(
        downstream_abandoned_during_cache_fill_calls(path),
        abandoned_calls_before + usize::from(handles_abandonment),
        "the H1 path did not notify immediately"
    );

    send_early_hints_tx.send(()).unwrap();
    wait_for_upstream_response_headers(path, upstream_header_calls_before + 1).await;
    if !handles_abandonment {
        // The 1xx went on to the downstream response filter, whose failure ended the request.
        wait_for_recorded_request_completion(path, completed_before).await;
        assert_eq!(
            recorded_request_error(path),
            Some(pingora_error::ErrorType::InternalError)
        );
    }
    send_final_response_tx.send(()).unwrap();
    response_started_rx.await.unwrap();
    if handles_abandonment {
        wait_for_upstream_response_headers(path, upstream_header_calls_before + 2).await;
        assert_eq!(
            downstream_abandoned_during_cache_fill_calls(path),
            abandoned_calls_before + 1,
            "the final response repeated the cache-fill callback"
        );
    }
    finish_response_tx.send(()).unwrap();
    let _ = done_tx.send(());
    assert!(response.await.is_err());
    if handles_abandonment {
        wait_for_recorded_request_completion(path, completed_before).await;
    }
    assert_eq!(recorded_upstream_attempts(path), attempts_before + 1);

    if handles_abandonment {
        let cached_body = wait_for_h2_cache_hit(&uri, upstream_port).await;
        assert_eq!(cached_body, b"hello world!");
    } else {
        assert_failed_fill_recovers(path).await;
    }
    assert_eq!(
        downstream_abandoned_during_cache_fill_calls(path),
        abandoned_calls_before + usize::from(handles_abandonment),
        "finishing the H1 cache fill must not repeat the callback"
    );

    upstream.await.unwrap();
}

#[tokio::test]
async fn test_h1_pre_admission_abandonment_skips_early_hints_and_completes_fill() {
    bounded(
        assert_h1_downstream_abandonment_before_cache_admission_completes_fill(
            "/test_h1_downstream_abandonment_before_cache_admission",
            true,
        ),
    )
    .await;
    // Without the opt-in the 1xx reaches the failing filter, ending the unretried POST.
    bounded(
        assert_h1_downstream_abandonment_before_cache_admission_completes_fill(
            "/test_h1_downstream_abandonment_before_cache_admission_without_abandonment_handling",
            false,
        ),
    )
    .await;
}
