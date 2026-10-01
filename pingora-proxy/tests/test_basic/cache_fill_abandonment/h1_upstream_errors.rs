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

/// An HTTP/2 client resets a complete `GET` before the response header. The first HTTP/1 attempt
/// then fails before its header, the retry hook makes that error retryable, and the retry gets a
/// cacheable response.
async fn assert_h1_pre_admission_abandonment_retries_complete_request_and_fills_cache(
    path: &'static str,
    handles_abandonment: bool,
) {
    init();

    const RETRY_BODY: &[u8] = b"retry body";
    let uri = format!("http://127.0.0.1:6154{path}");
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_port = listener.local_addr().unwrap().port();
    let (first_request_seen_tx, first_request_seen_rx) = tokio::sync::oneshot::channel();
    let (fail_first_attempt_tx, fail_first_attempt_rx) = tokio::sync::oneshot::channel();
    let (retry_seen_tx, retry_seen_rx) = tokio::sync::oneshot::channel();

    let upstream = tokio::spawn(async move {
        let (mut first_socket, _) = tokio::time::timeout(TEST_TIMEOUT, listener.accept())
            .await
            .expect("proxy did not connect to the first HTTP/1 upstream")
            .unwrap();
        read_h1_head(&mut first_socket).await;
        first_request_seen_tx.send(()).unwrap();

        fail_first_attempt_rx.await.unwrap();
        drop(first_socket);

        let (mut retry_socket, _) = tokio::time::timeout(TEST_TIMEOUT, listener.accept())
            .await
            .expect("proxy did not retry the HTTP/1 upstream request")
            .unwrap();
        read_h1_head(&mut retry_socket).await;
        retry_seen_tx.send(()).unwrap();
        retry_socket
            .write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nCache-Control: max-age=30\r\n\r\n",
                    RETRY_BODY.len()
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        retry_socket.write_all(RETRY_BODY).await.unwrap();
    });

    let mut client = h2c_client(6154).await;
    let attempts_before = recorded_upstream_attempts(path);
    let completed_before = recorded_request_completions(path);
    let retry_hook_calls_before = forced_retry_error_hook_calls(path);
    let request = opt_in(Request::builder(), handles_abandonment)
        .method("GET")
        .uri(uri.as_str())
        .header("x-port", upstream_port.to_string())
        .header("x-lock", "true")
        .header("x-record-request-lifecycle", "true")
        .header("x-force-retry-in-error-hook", "true")
        .body(())
        .unwrap();
    let (response, mut request_body) = client.send_request(request, true).unwrap();
    first_request_seen_rx.await.unwrap();

    let abandoned_calls_before = downstream_abandoned_during_cache_fill_calls(path);
    let warn_calls_before = downstream_cache_warn_log_calls_for(path);
    request_body.send_reset(h2::Reason::CANCEL);
    wait_for_ignored_downstream_error(path, warn_calls_before).await;
    assert_eq!(
        downstream_abandoned_during_cache_fill_calls(path),
        abandoned_calls_before + usize::from(handles_abandonment)
    );

    fail_first_attempt_tx.send(()).unwrap();
    tokio::time::timeout(TEST_TIMEOUT, retry_seen_rx)
        .await
        .expect("the complete request was not retried")
        .unwrap();
    assert_eq!(
        downstream_abandoned_during_cache_fill_calls(path),
        abandoned_calls_before + usize::from(handles_abandonment),
        "starting the retry repeated the abandonment callback"
    );

    assert!(
        tokio::time::timeout(TEST_TIMEOUT, response)
            .await
            .expect("abandoned request did not finish")
            .is_err(),
        "the retry wrote a response to the abandoned downstream"
    );
    upstream.await.unwrap();
    wait_for_recorded_request_completion(path, completed_before).await;
    assert_eq!(recorded_request_error(path), None);
    assert_eq!(
        forced_retry_error_hook_calls(path),
        retry_hook_calls_before + 1
    );
    assert_eq!(recorded_upstream_attempts(path), attempts_before + 2);
    assert_eq!(
        downstream_abandoned_during_cache_fill_calls(path),
        abandoned_calls_before + usize::from(handles_abandonment),
        "finishing the retry repeated the abandonment callback"
    );

    assert_eq!(wait_for_h2_cache_hit(&uri, upstream_port).await, RETRY_BODY);
}

#[tokio::test]
async fn test_h1_pre_admission_abandonment_retries_complete_request_and_fills_cache() {
    assert_h1_pre_admission_abandonment_retries_complete_request_and_fills_cache(
        "/test_h1_pre_admission_abandonment_retries_complete_request_and_fills_cache",
        true,
    )
    .await;
    // Without the opt-in the same retry fills the cache, just without the callback.
    assert_h1_pre_admission_abandonment_retries_complete_request_and_fills_cache(
        "/test_h1_pre_admission_abandonment_retries_complete_request_and_fills_cache_without_abandonment_handling",
        false,
    )
    .await;
}

/// An HTTP/2 client resets its stream after the header and the start of the body of a cacheable
/// HTTP/1 response, which the origin then truncates. The retry hook makes every error retryable,
/// and the origin refuses any retried attempt.
async fn assert_h1_abandoned_cache_fill_propagates_truncated_upstream_response(
    path: &'static str,
    handles_abandonment: bool,
) {
    init();

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_port = listener.local_addr().unwrap().port();
    let (truncate_tx, truncate_rx) = tokio::sync::oneshot::channel();

    let upstream = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        drop(listener);
        read_h1_head(&mut socket).await;
        socket
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Length: 12\r\nCache-Control: max-age=30\r\n\r\nhello",
            )
            .await
            .unwrap();
        truncate_rx.await.unwrap();
    });

    let mut client = h2c_client(6154).await;
    let attempts_before = recorded_upstream_attempts(path);
    let completed_before = recorded_request_completions(path);
    let forced_retry_calls_before = forced_retry_error_hook_calls(path);
    let request = opt_in(Request::builder(), handles_abandonment)
        .method("GET")
        .uri(format!("http://127.0.0.1:6154{path}"))
        .header("x-port", upstream_port.to_string())
        .header("x-lock", "true")
        .header("x-cache-lock-wait-timeout-ms", "10000")
        .header("x-record-request-lifecycle", "true")
        .header("x-force-retry-in-error-hook", "true")
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
    assert_eq!(
        read_h2_body_chunk(&mut response_body).await,
        b"hello".as_slice()
    );

    let abandoned_calls_before = downstream_abandoned_during_cache_fill_calls(path);
    let warn_calls_before = downstream_cache_warn_log_calls_for(path);
    request_body.send_reset(h2::Reason::CANCEL);
    wait_for_ignored_downstream_error(path, warn_calls_before).await;

    truncate_tx.send(()).unwrap();
    upstream.await.unwrap();
    wait_for_recorded_request_completion(path, completed_before).await;
    assert_eq!(
        downstream_abandoned_during_cache_fill_calls(path),
        abandoned_calls_before + usize::from(handles_abandonment)
    );
    // After abandonment the admitted fill's error is final. Without the opt-in the retry hook's
    // retry is allowed and fails to connect.
    let (expected_error, expected_attempts) = if handles_abandonment {
        (pingora_error::ErrorType::ConnectionClosed, 1)
    } else {
        (pingora_error::ErrorType::ConnectRefused, 2)
    };
    assert_eq!(recorded_request_error(path), Some(expected_error));
    assert_eq!(
        forced_retry_error_hook_calls(path),
        forced_retry_calls_before + 1
    );
    assert_eq!(
        recorded_upstream_attempts(path),
        attempts_before + expected_attempts
    );

    assert_failed_fill_recovers(path).await;
}

#[tokio::test]
async fn test_h1_abandoned_cache_fill_propagates_truncated_upstream_response() {
    bounded(
        assert_h1_abandoned_cache_fill_propagates_truncated_upstream_response(
            "/test_h1_abandoned_cache_fill_propagates_truncated_upstream_response",
            true,
        ),
    )
    .await;
    // Without the opt-in the retry is allowed despite the admission; it fails to connect.
    bounded(
        assert_h1_abandoned_cache_fill_propagates_truncated_upstream_response(
            "/test_h1_abandoned_cache_fill_propagates_truncated_upstream_response_without_abandonment_handling",
            false,
        ),
    )
    .await;
}

#[tokio::test]
async fn test_h1_abandoned_cache_fill_finish_error_stays_best_effort() {
    bounded(assert_abandoned_storage_failure(
        "/test_h1_abandoned_cache_fill_finish_error_stays_best_effort",
        CacheStorageFailure::Finish,
        Upstream::H1,
        true,
    ))
    .await;
    // Without the opt-in a failed finish is equally best effort.
    bounded(assert_abandoned_storage_failure(
        "/test_h1_abandoned_cache_fill_finish_error_stays_best_effort_without_abandonment_handling",
        CacheStorageFailure::Finish,
        Upstream::H1,
        false,
    ))
    .await;
}

#[tokio::test]
async fn test_h1_abandoned_cache_fill_propagates_write_error() {
    bounded(assert_abandoned_storage_failure(
        "/test_h1_abandoned_cache_fill_propagates_write_error",
        CacheStorageFailure::Write,
        Upstream::H1,
        true,
    ))
    .await;
    // Without the opt-in the failed write is fatal too, as it fed the served miss body.
    bounded(assert_abandoned_storage_failure(
        "/test_h1_abandoned_cache_fill_propagates_write_error_without_abandonment_handling",
        CacheStorageFailure::Write,
        Upstream::H1,
        false,
    ))
    .await;
}
