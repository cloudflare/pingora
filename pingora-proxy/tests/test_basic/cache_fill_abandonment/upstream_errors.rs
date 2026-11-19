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

/// A PUT whose client resets the stream while its body is still incomplete, abandoning a fill
/// that has not been admitted yet. The upstream then refuses the stream and the error hook makes
/// that failure retryable. A retry could never send the rest of the body, so the proxy fails the
/// request instead (any retried stream would be reset at once) and releases the cache lock for
/// the next request. The connected twin,
/// `test_h2_pre_admission_connected_request_with_incomplete_body_retries`, retries.
#[tokio::test]
async fn test_h2_pre_admission_abandonment_with_incomplete_body_stops_retry_and_releases_lock() {
    init();

    const PATH: &str =
        "/test_h2_pre_admission_abandonment_with_incomplete_body_stops_retry_and_releases_lock";
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_port = listener.local_addr().unwrap().port();
    let (request_seen_tx, request_seen_rx) = tokio::sync::oneshot::channel();
    let (refuse_stream_tx, refuse_stream_rx) = tokio::sync::oneshot::channel();

    let upstream = tokio::spawn(async move {
        let (request, mut response) = accept_h2_request(listener).await;
        let mut request_body = request.into_body();
        let first_chunk = tokio::time::timeout(TEST_TIMEOUT, request_body.data())
            .await
            .expect("proxy did not forward the request body prefix")
            .expect("first upstream request ended before its body prefix")
            .unwrap();
        assert_eq!(first_chunk, Bytes::from_static(b"body prefix"));
        request_seen_tx.send(()).unwrap();

        refuse_stream_rx.await.unwrap();
        response.send_reset(h2::Reason::REFUSED_STREAM);
    });

    let mut client = h2c_client(6154).await;

    let attempts_before = recorded_upstream_attempts(PATH);
    let completed_before = recorded_request_completions(PATH);
    let forced_retry_calls_before = forced_retry_error_hook_calls(PATH);
    let nonretryable_hook_calls_before = forced_retry_error_hook_calls_with_retry_disabled(PATH);
    let request = Request::builder()
        .header(HANDLE_ABANDONMENT.0, HANDLE_ABANDONMENT.1)
        .method("PUT")
        .uri(format!("http://127.0.0.1:6154{PATH}"))
        .header("x-h2", "true")
        .header("x-port", upstream_port.to_string())
        .header("x-lock", "true")
        .header("x-record-request-lifecycle", "true")
        .header("x-force-retry-in-error-hook", "true")
        .body(())
        .unwrap();
    let (first_response, mut request_body) = client.send_request(request, false).unwrap();
    send_body_chunk(&mut request_body, b"body prefix", false).await;
    tokio::time::timeout(TEST_TIMEOUT, request_seen_rx)
        .await
        .expect("first upstream did not receive the request")
        .unwrap();

    let abandoned_calls_before = downstream_abandoned_during_cache_fill_calls(PATH);
    let warn_calls_before = downstream_cache_warn_log_calls_for(PATH);
    request_body.send_reset(h2::Reason::CANCEL);
    wait_for_ignored_downstream_error(PATH, warn_calls_before).await;
    assert_eq!(
        downstream_abandoned_during_cache_fill_calls(PATH),
        abandoned_calls_before + 1,
        "downstream abandonment did not notify the application immediately"
    );

    refuse_stream_tx.send(()).unwrap();

    assert!(
        tokio::time::timeout(TEST_TIMEOUT, first_response)
            .await
            .expect("abandoned request did not finish")
            .is_err(),
        "abandoned request unexpectedly received a response"
    );
    wait_for_recorded_request_completion(PATH, completed_before).await;
    assert_eq!(
        downstream_abandoned_during_cache_fill_calls(PATH),
        abandoned_calls_before + 1,
        "the upstream failure repeated the abandonment callback"
    );
    assert_eq!(
        recorded_upstream_attempts(PATH),
        attempts_before + 1,
        "proxy retried an abandoned request whose body was incomplete"
    );
    assert_eq!(
        forced_retry_error_hook_calls(PATH),
        forced_retry_calls_before + 1,
        "the refused stream did not reach the retry hook"
    );
    assert_eq!(
        forced_retry_error_hook_calls_with_retry_disabled(PATH),
        nonretryable_hook_calls_before,
        "the refused stream reached the retry hook already non-retryable"
    );
    tokio::time::timeout(TEST_TIMEOUT, upstream)
        .await
        .expect("first upstream task did not finish")
        .unwrap();
    assert_failed_fill_recovers(PATH).await;
}

async fn send_body_chunk(
    request_body: &mut h2::SendStream<Bytes>,
    chunk: &'static [u8],
    end_of_stream: bool,
) {
    request_body.reserve_capacity(chunk.len());
    tokio::time::timeout(
        TEST_TIMEOUT,
        std::future::poll_fn(|cx| request_body.poll_capacity(cx)),
    )
    .await
    .expect("proxy did not grant request body capacity")
    .unwrap()
    .unwrap();
    request_body
        .send_data(Bytes::from_static(chunk), end_of_stream)
        .unwrap();
}

/// The same PUT and refused stream as
/// `test_h2_pre_admission_abandonment_with_incomplete_body_stops_retry_and_releases_lock`, but
/// the client stays connected and sends the rest of its body once the retry has started. The
/// proxy retries it: the retry replays the buffered body prefix and forwards the rest.
async fn assert_h2_connected_incomplete_body_retries(path: &str, handles_abandonment: bool) {
    init();

    const BODY_PREFIX: &[u8] = b"body prefix";
    const BODY_SUFFIX: &[u8] = b" body suffix";
    const RETRY_RESPONSE: &[u8] = b"retried";
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_port = listener.local_addr().unwrap().port();
    let (first_request_seen_tx, first_request_seen_rx) = tokio::sync::oneshot::channel();
    let (refuse_stream_tx, refuse_stream_rx) = tokio::sync::oneshot::channel();
    let (retry_prefix_seen_tx, retry_prefix_seen_rx) = tokio::sync::oneshot::channel();

    let upstream = tokio::spawn(async move {
        let mut requests = accept_h2_connection(listener).await;
        let (request, mut response) = next_h2_request(&mut requests).await;
        let mut request_body = request.into_body();
        let first_chunk = read_h2_body_chunk(&mut request_body).await;
        assert_eq!(first_chunk, BODY_PREFIX);
        first_request_seen_tx.send(()).unwrap();

        refuse_stream_rx.await.unwrap();
        response.send_reset(h2::Reason::REFUSED_STREAM);

        let (retry_request, mut retry_response) = next_h2_request(&mut requests).await;
        let mut retry_body = retry_request.into_body();
        let mut received = Vec::new();
        while received.len() < BODY_PREFIX.len() {
            received.extend_from_slice(&read_h2_body_chunk(&mut retry_body).await);
        }
        assert_eq!(
            received, BODY_PREFIX,
            "the retry did not replay the body prefix"
        );
        retry_prefix_seen_tx.send(()).unwrap();
        while let Some(chunk) = tokio::time::timeout(TEST_TIMEOUT, retry_body.data())
            .await
            .expect("the retry did not forward the rest of the request body")
        {
            let chunk = chunk.unwrap();
            retry_body
                .flow_control()
                .release_capacity(chunk.len())
                .unwrap();
            received.extend_from_slice(&chunk);
        }
        assert_eq!(received, [BODY_PREFIX, BODY_SUFFIX].concat());

        let response_header = Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_LENGTH, RETRY_RESPONSE.len().to_string())
            .body(())
            .unwrap();
        let mut response_body = retry_response
            .send_response(response_header, false)
            .unwrap();
        response_body
            .send_data(Bytes::from_static(RETRY_RESPONSE), true)
            .unwrap();
    });

    let mut client = h2c_client(6154).await;
    let attempts_before = recorded_upstream_attempts(path);
    let completed_before = recorded_request_completions(path);
    let forced_retry_calls_before = forced_retry_error_hook_calls(path);
    let abandoned_calls_before = downstream_abandoned_during_cache_fill_calls(path);
    let request = opt_in(Request::builder(), handles_abandonment)
        .method("PUT")
        .uri(format!("http://127.0.0.1:6154{path}"))
        .header("x-h2", "true")
        .header("x-port", upstream_port.to_string())
        .header("x-lock", "true")
        .header("x-record-request-lifecycle", "true")
        .header("x-force-retry-in-error-hook", "true")
        .body(())
        .unwrap();
    let (response, mut request_body) = client.send_request(request, false).unwrap();
    send_body_chunk(&mut request_body, BODY_PREFIX, false).await;
    tokio::time::timeout(TEST_TIMEOUT, first_request_seen_rx)
        .await
        .expect("the upstream did not receive the body prefix")
        .unwrap();
    refuse_stream_tx.send(()).unwrap();
    tokio::time::timeout(TEST_TIMEOUT, retry_prefix_seen_rx)
        .await
        .expect("the retry did not reach the upstream")
        .unwrap();
    send_body_chunk(&mut request_body, BODY_SUFFIX, true).await;
    let response = tokio::time::timeout(TEST_TIMEOUT, response)
        .await
        .expect("proxy did not return the retried response")
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        read_h2_body(&mut response.into_body()).await,
        RETRY_RESPONSE
    );

    tokio::time::timeout(TEST_TIMEOUT, upstream)
        .await
        .expect("upstream task did not finish")
        .unwrap();
    wait_for_recorded_request_completion(path, completed_before).await;
    assert_eq!(
        forced_retry_error_hook_calls(path),
        forced_retry_calls_before + 1,
        "the refused stream did not reach the retry hook"
    );
    assert_eq!(recorded_upstream_attempts(path), attempts_before + 2);
    assert_eq!(
        downstream_abandoned_during_cache_fill_calls(path),
        abandoned_calls_before,
        "a connected request was reported as abandoned"
    );
}

#[tokio::test]
async fn test_h2_pre_admission_connected_request_with_incomplete_body_retries() {
    assert_h2_connected_incomplete_body_retries(
        "/test_h2_pre_admission_connected_request_with_incomplete_body_retries_gated",
        true,
    )
    .await;
    // Without the opt-in the connected request is retried just the same.
    assert_h2_connected_incomplete_body_retries(
        "/test_h2_pre_admission_connected_request_with_incomplete_body_retries_ungated",
        false,
    )
    .await;
}

/// A GET whose client resets the stream before the response header, on storage with streaming
/// partial writes, so the proxy ignores the reset for the fill. The upstream then refuses the
/// stream and the error hook makes that failure retryable. The request body is complete, so the
/// proxy retries, and the retried response fills the cache.
async fn assert_h2_pre_admission_reset_retries_complete_request_and_fills_cache(
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
    let (finish_retry_tx, finish_retry_rx) = tokio::sync::oneshot::channel();
    let (release_retry_streams_tx, release_retry_streams_rx) = tokio::sync::oneshot::channel();

    let upstream = tokio::spawn(async move {
        let mut requests = accept_h2_connection(listener).await;
        let (_request, mut first_response) = next_h2_request(&mut requests).await;
        first_request_seen_tx.send(()).unwrap();

        fail_first_attempt_rx.await.unwrap();
        first_response.send_reset(h2::Reason::REFUSED_STREAM);

        // The retry reuses the upstream connection.
        let (retry_request, mut retry_response) = next_h2_request(&mut requests).await;
        drop(requests);
        retry_seen_tx.send(()).unwrap();
        finish_retry_rx.await.unwrap();

        let response_header = Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_LENGTH, RETRY_BODY.len().to_string())
            .header(header::CACHE_CONTROL, "max-age=30")
            .body(())
            .unwrap();
        let mut response_body = retry_response
            .send_response(response_header, false)
            .unwrap();
        response_body
            .send_data(Bytes::from_static(RETRY_BODY), true)
            .unwrap();

        let _ = release_retry_streams_rx.await;
        drop(retry_request);
        drop(response_body);
    });

    let mut client = h2c_client(6154).await;

    let attempts_before = recorded_upstream_attempts(path);
    let completed_before = recorded_request_completions(path);
    let forced_retry_calls_before = forced_retry_error_hook_calls(path);
    let nonretryable_hook_calls_before = forced_retry_error_hook_calls_with_retry_disabled(path);
    let request = opt_in(Request::builder(), handles_abandonment)
        .method("GET")
        .uri(uri.as_str())
        .header("x-h2", "true")
        .header("x-port", upstream_port.to_string())
        .header("x-lock", "true")
        .header("x-record-request-lifecycle", "true")
        .header("x-force-retry-in-error-hook", "true")
        .body(())
        .unwrap();
    let (response, mut request_body) = client.send_request(request, true).unwrap();
    tokio::time::timeout(TEST_TIMEOUT, first_request_seen_rx)
        .await
        .expect("first upstream request was not observed")
        .unwrap();

    let abandoned_calls_before = downstream_abandoned_during_cache_fill_calls(path);
    let warn_calls_before = downstream_cache_warn_log_calls_for(path);
    request_body.send_reset(h2::Reason::CANCEL);
    wait_for_ignored_downstream_error(path, warn_calls_before).await;
    // Only an opted-in request reports the ignored reset as an abandonment, and at once.
    let expected_callbacks = usize::from(handles_abandonment);
    assert_eq!(
        downstream_abandoned_during_cache_fill_calls(path),
        abandoned_calls_before + expected_callbacks,
        "the reset was not reported as an abandonment exactly when the request opted in"
    );

    fail_first_attempt_tx.send(()).unwrap();
    tokio::time::timeout(TEST_TIMEOUT, retry_seen_rx)
        .await
        .expect("a complete request was not retried after downstream abandonment")
        .unwrap();
    assert_eq!(
        downstream_abandoned_during_cache_fill_calls(path),
        abandoned_calls_before + expected_callbacks,
        "starting a retry repeated the abandonment callback"
    );

    finish_retry_tx.send(()).unwrap();
    assert!(
        tokio::time::timeout(TEST_TIMEOUT, response)
            .await
            .expect("abandoned request did not finish")
            .is_err(),
        "the retry wrote a response to the abandoned downstream"
    );
    wait_for_recorded_request_completion(path, completed_before).await;
    assert_eq!(recorded_request_error(path), None);
    assert_eq!(
        forced_retry_error_hook_calls(path),
        forced_retry_calls_before + 1,
        "the first upstream failure did not reach the retry hook"
    );
    assert_eq!(
        forced_retry_error_hook_calls_with_retry_disabled(path),
        nonretryable_hook_calls_before,
        "downstream abandonment disabled an otherwise valid retry"
    );
    assert_eq!(recorded_upstream_attempts(path), attempts_before + 2);
    assert_eq!(
        downstream_abandoned_during_cache_fill_calls(path),
        abandoned_calls_before + expected_callbacks,
        "the successful retry repeated the abandonment callback"
    );
    // A retry of an abandoned request never reads its client again. Without the opt-in the
    // retry reads the gone client like the first attempt did, and ignores its reset again.
    let expected_ignored_resets = if handles_abandonment { 1 } else { 2 };
    assert_eq!(
        downstream_cache_warn_log_calls_for(path),
        warn_calls_before + expected_ignored_resets
    );

    let cached_body = wait_for_h2_cache_hit(&uri, upstream_port).await;
    assert_eq!(cached_body, RETRY_BODY);
    let _ = release_retry_streams_tx.send(());
    tokio::time::timeout(TEST_TIMEOUT, upstream)
        .await
        .expect("upstream retry task did not finish")
        .unwrap();
}

#[tokio::test]
async fn test_h2_pre_admission_abandonment_retries_complete_request_and_fills_cache() {
    assert_h2_pre_admission_reset_retries_complete_request_and_fills_cache(
        "/test_h2_pre_admission_abandonment_retries_complete_request_and_fills_cache_gated",
        true,
    )
    .await;
    // Without the opt-in the proxy retries and fills the cache just the same, but the retry reads
    // the gone client again, and the abandonment callback is missing.
    assert_h2_pre_admission_reset_retries_complete_request_and_fills_cache(
        "/test_h2_pre_admission_abandonment_retries_complete_request_and_fills_cache_ungated",
        false,
    )
    .await;
}
