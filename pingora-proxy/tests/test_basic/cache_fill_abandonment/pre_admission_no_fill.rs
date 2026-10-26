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

use super::pre_admission::{
    start_pre_admission_reset, PreAdmissionResetConfig, PreAdmissionResetFixture,
};
use super::*;
use http::Method;
use pingora_cache::max_file_size::ERR_RESPONSE_TOO_LARGE;
use pingora_error::ErrorType;

async fn start_reset(config: PreAdmissionResetConfig) -> PreAdmissionResetFixture {
    tokio::time::timeout(
        std::time::Duration::from_secs(20),
        start_pre_admission_reset(config),
    )
    .await
    .expect("pre-admission reset setup did not finish")
}

/// A reset before the response header, where the header then shows that the response will not
/// be cached. Nothing is left for the request to do, with or without abandonment handling: the
/// proxy fails it with a downstream write error and resets the origin's stream, which never
/// ends its response.
async fn assert_pre_admission_reset_stops_response(config: PreAdmissionResetConfig) {
    let path = config.path;
    let expected_callbacks = usize::from(config.handles_abandonment);
    let PreAdmissionResetFixture {
        upstream_port: _,
        send_response,
        response_sent,
        send_body: _send_body,
        finish_body: _finish_body,
        upstream_reset,
        abandoned_calls_before,
        attempts_before,
        completed_before,
    } = start_reset(config).await;

    send_response.send(()).unwrap();
    tokio::time::timeout(TEST_TIMEOUT, response_sent)
        .await
        .expect("origin did not send the response header")
        .unwrap();

    let reason = tokio::time::timeout(TEST_TIMEOUT, upstream_reset)
        .await
        .expect("proxy drained a response it will not cache after the downstream reset")
        .expect("upstream reset watcher stopped before receiving a reset");
    assert_eq!(reason, h2::Reason::CANCEL);
    wait_for_recorded_request_completion(path, completed_before).await;
    assert_eq!(recorded_request_error(path), Some(ErrorType::WriteError));
    assert_eq!(recorded_upstream_attempts(path), attempts_before + 1);
    assert_eq!(
        downstream_abandoned_during_cache_fill_calls(path),
        abandoned_calls_before + expected_callbacks,
        "response classification repeated the abandonment callback"
    );
}

fn uncacheable_response(path: &'static str, handles_abandonment: bool) -> PreAdmissionResetConfig {
    PreAdmissionResetConfig {
        cache_control: "no-store",
        ..PreAdmissionResetConfig::post(path, handles_abandonment)
    }
}

#[tokio::test]
async fn test_h2_pre_admission_reset_stops_uncacheable_response() {
    assert_pre_admission_reset_stops_response(uncacheable_response(
        "/test_h2_pre_admission_reset_stops_uncacheable_response_gated",
        true,
    ))
    .await;
    // Without the opt-in the reset is not reported, and the proxy fails the request when it
    // writes the response header to the reset stream instead.
    assert_pre_admission_reset_stops_response(uncacheable_response(
        "/test_h2_pre_admission_reset_stops_uncacheable_response_ungated",
        false,
    ))
    .await;
}

/// A reset before the response header of a cacheable response whose fill then stops at the
/// first body chunk, the only one the origin sends until asked to end its response: `config`
/// makes that chunk exceed the cache size limit or fail its cache write.
///
/// An opted-in request has nothing left once its fill stops, so the proxy fails it with
/// `opted_in_error` and resets the origin's stream. Without the opt-in the proxy keeps reading
/// the origin for nothing, and the request ends without an error once the origin ends its
/// response.
async fn assert_pre_admission_reset_then_fill_stops(
    config: PreAdmissionResetConfig,
    opted_in_error: ErrorType,
) {
    let path = config.path;
    let handles_abandonment = config.handles_abandonment;
    let PreAdmissionResetFixture {
        upstream_port: _,
        send_response,
        response_sent,
        send_body,
        finish_body,
        upstream_reset,
        abandoned_calls_before,
        attempts_before,
        completed_before,
    } = start_reset(config).await;

    send_response.send(()).unwrap();
    tokio::time::timeout(TEST_TIMEOUT, response_sent)
        .await
        .expect("origin did not send the cacheable response header")
        .unwrap();
    send_body.send(()).unwrap();

    if handles_abandonment {
        let reason = tokio::time::timeout(TEST_TIMEOUT, upstream_reset)
            .await
            .expect("proxy kept reading the origin after the cache fill stopped")
            .expect("upstream reset watcher stopped before receiving a reset");
        assert_eq!(reason, h2::Reason::CANCEL);
        wait_for_recorded_request_completion(path, completed_before).await;
        assert_eq!(recorded_request_error(path), Some(opted_in_error));
    } else {
        finish_body.send(()).unwrap();
        wait_for_recorded_request_completion(path, completed_before).await;
        assert_eq!(
            recorded_request_error(path),
            None,
            "the request did not read the origin's response to its end"
        );
    }
    assert_eq!(recorded_upstream_attempts(path), attempts_before + 1);
    assert_eq!(
        downstream_abandoned_during_cache_fill_calls(path),
        abandoned_calls_before + usize::from(handles_abandonment),
        "the stopped cache fill repeated the abandonment callback"
    );
}

fn response_exceeding_cache_limit(
    path: &'static str,
    handles_abandonment: bool,
) -> PreAdmissionResetConfig {
    PreAdmissionResetConfig {
        response_body: Some(Bytes::from_static(b"larger than the cache limit")),
        max_file_size: Some(8),
        ..PreAdmissionResetConfig::post(path, handles_abandonment)
    }
}

#[tokio::test]
async fn test_h2_pre_admission_reset_stops_when_response_exceeds_cache_limit() {
    assert_pre_admission_reset_then_fill_stops(
        response_exceeding_cache_limit(
            "/test_h2_pre_admission_reset_stops_when_response_exceeds_cache_limit_gated",
            true,
        ),
        ERR_RESPONSE_TOO_LARGE,
    )
    .await;
    // Without the opt-in the proxy only disables the cache when the response exceeds its limit,
    // and reads the rest of the response although nobody receives it.
    assert_pre_admission_reset_then_fill_stops(
        response_exceeding_cache_limit(
            "/test_h2_pre_admission_reset_stops_when_response_exceeds_cache_limit_ungated",
            false,
        ),
        ERR_RESPONSE_TOO_LARGE,
    )
    .await;
}

fn response_whose_cache_write_fails(
    path: &'static str,
    handles_abandonment: bool,
) -> PreAdmissionResetConfig {
    PreAdmissionResetConfig {
        response_body: Some(Bytes::from_static(b"first part of the body")),
        fail_cache_write: true,
        ..PreAdmissionResetConfig::post(path, handles_abandonment)
    }
}

// A storage write failure after abandonment ends the request with that error, like the size
// limit above. The fill is the only output left, so the proxy does not read the rest of the
// origin response for nothing. A connected client's write failure stays as harmless as without
// abandonment handling: see
// `test_connected_miss_cache_write_failure_before_cached_header_is_not_fatal` in
// cache_storage_errors.rs.
#[tokio::test]
async fn test_h2_pre_admission_reset_stops_when_cache_write_fails() {
    assert_pre_admission_reset_then_fill_stops(
        response_whose_cache_write_fails(
            "/test_h2_pre_admission_reset_stops_when_cache_write_fails_gated",
            true,
        ),
        ErrorType::FileWriteError,
    )
    .await;
    // Without the opt-in a failed cache write only disables the cache, and the proxy reads the
    // rest of the response although nobody receives it.
    assert_pre_admission_reset_then_fill_stops(
        response_whose_cache_write_fails(
            "/test_h2_pre_admission_reset_stops_when_cache_write_fails_ungated",
            false,
        ),
        ErrorType::FileWriteError,
    )
    .await;
}

async fn assert_pre_admission_predictor_bypass_reset_stops_response(
    path: &'static str,
    handles_abandonment: bool,
) {
    init();
    prime_predictor_bypass(path, "127.0.0.1:6154", handles_abandonment).await;
    assert_pre_admission_reset_stops_response(PreAdmissionResetConfig {
        method: Method::GET,
        leave_predictor_bypass: true,
        ..PreAdmissionResetConfig::post(path, handles_abandonment)
    })
    .await;
}

#[tokio::test]
async fn test_h2_pre_admission_predictor_bypass_stops_when_cache_miss_leaves_bypass_unchanged() {
    assert_pre_admission_predictor_bypass_reset_stops_response(
        "/test_h2_pre_admission_predictor_bypass_stops_when_cache_miss_leaves_bypass_unchanged_gated",
        true,
    )
    .await;
    // Without the opt-in the reset is not reported, and the proxy fails the request when it
    // writes the response header to the reset stream instead.
    assert_pre_admission_predictor_bypass_reset_stops_response(
        "/test_h2_pre_admission_predictor_bypass_stops_when_cache_miss_leaves_bypass_unchanged_ungated",
        false,
    )
    .await;
}

/// How the origin answers the first attempt of a bypassed request in
/// [`assert_abandoned_bypassed_request_with_forced_retry`]. It answers every later attempt with
/// the declined cacheable response.
#[derive(Clone, Copy)]
enum FirstBypassedAttempt {
    /// A cacheable response header, whose fill `cache_miss` declines.
    DeclinedFill,
    /// A refused stream, before any response header.
    RefusedStream,
}

/// A complete GET on a key the predictor bypasses, whose client resets its stream before the
/// response header, and whose first upstream attempt then fails as `first_attempt` says. The
/// error hook makes every failure retryable.
///
/// Once opted in, the reset abandons the response while its cache decision is open, and the
/// failed attempt leaves no cache that a retry could fill: `cache_miss` keeps declining, and
/// a bypassed response is admitted only if it re-enables the cache. So the proxy does not
/// retry. A retry would only pull from the origin for nobody. Without the opt-in the reset is
/// ignored as it is today. Each retry writes its response header to the reset stream and
/// fails, until the retry limit.
///
/// Either way, declining the fill leaves the cache bypassed, as for a client that stays
/// connected.
async fn assert_abandoned_bypassed_request_with_forced_retry(
    path: &'static str,
    first_attempt: FirstBypassedAttempt,
    handles_abandonment: bool,
) {
    use crate::utils::server_utils::recorded_cache_reason;

    const BODY: &[u8] = b"declined fill";
    init();
    prime_predictor_bypass(path, "127.0.0.1:6154", handles_abandonment).await;

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_port = listener.local_addr().unwrap().port();
    let (first_request_seen_tx, first_request_seen_rx) = tokio::sync::oneshot::channel();
    let (answer_first_request_tx, answer_first_request_rx) = tokio::sync::oneshot::channel();
    let upstream = tokio::spawn(async move {
        let mut first = Some((first_request_seen_tx, answer_first_request_rx));
        loop {
            let (io, _) = listener.accept().await.unwrap();
            let mut conn = h2::server::handshake(io).await.unwrap();
            let mut first = first.take();
            tokio::spawn(async move {
                while let Some(Ok((_request, mut respond))) = conn.accept().await {
                    let first = first.take();
                    tokio::spawn(async move {
                        if let Some((seen, answer)) = first {
                            seen.send(()).unwrap();
                            answer.await.unwrap();
                            if matches!(first_attempt, FirstBypassedAttempt::RefusedStream) {
                                respond.send_reset(h2::Reason::REFUSED_STREAM);
                                return;
                            }
                        }
                        let response = Response::builder()
                            .status(StatusCode::OK)
                            .header(header::CONTENT_LENGTH, BODY.len().to_string())
                            .header(header::CACHE_CONTROL, "max-age=30")
                            .body(())
                            .unwrap();
                        if let Ok(mut body) = respond.send_response(response, false) {
                            let _ = body.send_data(Bytes::from_static(BODY), true);
                        }
                    });
                }
            });
        }
    });

    let mut client = h2c_client(6154).await;
    let attempts_before = recorded_upstream_attempts(path);
    let completed_before = recorded_request_completions(path);
    let forced_retry_calls_before = forced_retry_error_hook_calls(path);
    let request = opt_in(Request::builder(), handles_abandonment)
        .method("GET")
        .uri(format!("http://127.0.0.1:6154{path}"))
        .header("x-h2", "true")
        .header("x-port", upstream_port.to_string())
        .header("x-record-request-lifecycle", "true")
        .header("x-test-leave-predictor-bypass", "true")
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
    assert_eq!(
        downstream_abandoned_during_cache_fill_calls(path),
        abandoned_calls_before + usize::from(handles_abandonment),
        "the reset was not reported as an abandonment exactly when the request opted in"
    );

    answer_first_request_tx.send(()).unwrap();
    wait_for_recorded_request_completion(path, completed_before).await;
    let expected_attempts = if handles_abandonment {
        1
    } else {
        pingora_core::server::configuration::ServerConf::default().max_retries
    };
    assert_eq!(
        forced_retry_error_hook_calls(path),
        forced_retry_calls_before + expected_attempts,
        "the error hook did not make every failed attempt retryable"
    );
    assert_eq!(
        recorded_upstream_attempts(path),
        attempts_before + expected_attempts,
        "an abandoned request with no cache left to fill was retried"
    );
    assert!(recorded_request_error(path).is_some());
    if matches!(first_attempt, FirstBypassedAttempt::DeclinedFill) || !handles_abandonment {
        assert_eq!(recorded_request_error(path), Some(ErrorType::WriteError));
    }
    assert_eq!(
        recorded_cache_reason(path),
        None,
        "the request ended with its cache disabled instead of bypassed"
    );
    assert!(
        tokio::time::timeout(TEST_TIMEOUT, response)
            .await
            .expect("reset request did not finish")
            .is_err(),
        "a response reached the reset stream"
    );
    upstream.abort();
}

#[tokio::test]
async fn test_h2_abandoned_declined_bypass_fill_is_not_retried() {
    assert_abandoned_bypassed_request_with_forced_retry(
        "/test_h2_abandoned_declined_bypass_fill_is_not_retried_gated",
        FirstBypassedAttempt::DeclinedFill,
        true,
    )
    .await;
    // Without the opt-in every forced retry writes its header to the reset stream.
    assert_abandoned_bypassed_request_with_forced_retry(
        "/test_h2_abandoned_declined_bypass_fill_is_not_retried_ungated",
        FirstBypassedAttempt::DeclinedFill,
        false,
    )
    .await;
}

#[tokio::test]
async fn test_h2_abandoned_bypassed_request_is_not_retried_after_upstream_failure() {
    assert_abandoned_bypassed_request_with_forced_retry(
        "/test_h2_abandoned_bypassed_request_is_not_retried_after_upstream_failure_gated",
        FirstBypassedAttempt::RefusedStream,
        true,
    )
    .await;
    // Without the opt-in the proxy retries the refused stream, and every retry writes its
    // header to the reset stream.
    assert_abandoned_bypassed_request_with_forced_retry(
        "/test_h2_abandoned_bypassed_request_is_not_retried_after_upstream_failure_ungated",
        FirstBypassedAttempt::RefusedStream,
        false,
    )
    .await;
}

/// Prime the cache predictor so the next request for `path` bypasses the cache.
async fn prime_predictor_bypass(path: &str, cache_host: &str, handles_abandonment: bool) {
    const PRIME_BODY: &[u8] = b"not cached";
    let prime_origin = HttpOrigin::bind(|_request| async move {
        Response::builder()
            .header(header::CONTENT_LENGTH, PRIME_BODY.len().to_string())
            .header(header::CACHE_CONTROL, "no-store")
            .body(Bytes::from_static(PRIME_BODY))
            .unwrap()
    })
    .await
    .unwrap();
    let prime = client_opting_in(handles_abandonment)
        .get(format!("http://127.0.0.1:6148{path}"))
        .header(header::HOST, cache_host)
        .header("x-port", prime_origin.addr().port().to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(prime.status(), StatusCode::OK);
    assert_eq!(prime.bytes().await.unwrap(), PRIME_BODY);
    prime_origin.shutdown().await;
}

/// A bypassed request whose `cache_miss` callback declined to fill its cacheable response, and
/// whose client resets the stream once it has the response header. That response will never be
/// cached, so the reset is not an abandonment: as without abandonment handling, the proxy fails
/// the request at its next write to the client and stops reading the origin. A reset before the
/// response header is covered by
/// `test_h2_pre_admission_predictor_bypass_stops_when_cache_miss_leaves_bypass_unchanged`.
async fn assert_declined_fill_reset_after_header(path: &'static str, handles_abandonment: bool) {
    use std::future::poll_fn;

    init();

    const CACHE_HOST: &str = "127.0.0.1:6154";
    prime_predictor_bypass(path, CACHE_HOST, handles_abandonment).await;

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_port = listener.local_addr().unwrap().port();
    let (send_body_tx, send_body_rx) = tokio::sync::oneshot::channel::<()>();
    let (upstream_reset_tx, upstream_reset_rx) = tokio::sync::oneshot::channel::<h2::Reason>();
    let upstream = tokio::spawn(async move {
        let (_request, mut response) = accept_h2_request(listener).await;
        let response_header = Response::builder()
            .status(StatusCode::OK)
            .header(header::CACHE_CONTROL, "max-age=30")
            .body(())
            .unwrap();
        let mut response_body = response.send_response(response_header, false).unwrap();
        send_body_rx.await.unwrap();
        // Only the start of the body: the request can finish only by resetting this stream.
        response_body
            .send_data(Bytes::from_static(b"first"), false)
            .unwrap();
        let reason = poll_fn(|cx| response_body.poll_reset(cx)).await.unwrap();
        upstream_reset_tx.send(reason).unwrap();
    });

    let mut client = h2c_client(6154).await;
    let attempts_before = recorded_upstream_attempts(path);
    let completed_before = recorded_request_completions(path);
    let abandoned_before = downstream_abandoned_during_cache_fill_calls(path);
    let warn_calls_before = downstream_cache_warn_log_calls_for(path);
    let request = opt_in(Request::builder(), handles_abandonment)
        .method("GET")
        .uri(format!("http://{CACHE_HOST}{path}"))
        .header("x-h2", "true")
        .header("x-port", upstream_port.to_string())
        .header("x-test-leave-predictor-bypass", "true")
        .header("x-record-request-lifecycle", "true")
        .body(())
        .unwrap();
    let (response, mut request_stream) = client.send_request(request, true).unwrap();
    let response = tokio::time::timeout(TEST_TIMEOUT, response)
        .await
        .expect("proxy did not send the response header")
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["x-cache-status"], "no-cache");

    request_stream.send_reset(h2::Reason::CANCEL);
    wait_for_ignored_downstream_error(path, warn_calls_before).await;
    assert_eq!(
        downstream_abandoned_during_cache_fill_calls(path),
        abandoned_before,
        "a reset after the cache_miss callback declined the fill was reported as abandoning it"
    );

    send_body_tx.send(()).unwrap();
    let reason = tokio::time::timeout(TEST_TIMEOUT, upstream_reset_rx)
        .await
        .expect("proxy kept reading an origin response it will neither deliver nor cache")
        .unwrap();
    assert_eq!(reason, h2::Reason::CANCEL);
    wait_for_recorded_request_completion(path, completed_before).await;
    assert!(
        recorded_request_error(path).is_some(),
        "the request succeeded although its client left before the response was delivered"
    );
    assert_eq!(recorded_upstream_attempts(path), attempts_before + 1);
    assert_eq!(
        downstream_abandoned_during_cache_fill_calls(path),
        abandoned_before
    );
    tokio::time::timeout(TEST_TIMEOUT, upstream)
        .await
        .expect("upstream task did not finish")
        .unwrap();
}

#[tokio::test]
async fn test_h2_reset_after_header_of_declined_bypass_fill_is_not_abandonment() {
    bounded(assert_declined_fill_reset_after_header(
        "/test_h2_reset_after_header_of_declined_bypass_fill_gated",
        true,
    ))
    .await;
    // Without the opt-in the reset is handled the same.
    bounded(assert_declined_fill_reset_after_header(
        "/test_h2_reset_after_header_of_declined_bypass_fill_ungated",
        false,
    ))
    .await;
}

/// A miss on storage without streaming partial writes goes straight to the client, whose
/// stream is reset after the first body chunk. Such a fill cannot outlive its client, so with
/// or without abandonment handling the proxy fails the request at once, resets the upstream
/// stream, reports no abandonment and releases the cache lock.
async fn assert_downstream_reset_aborts_non_streaming_cache_admission(
    path: &'static str,
    handles_abandonment: bool,
) {
    use std::future::poll_fn;

    init();

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_port = listener.local_addr().unwrap().port();
    let (response_started_tx, response_started_rx) = tokio::sync::oneshot::channel();
    let (upstream_reset_tx, upstream_reset_rx) = tokio::sync::oneshot::channel();
    let upstream = tokio::spawn(async move {
        let (_request, mut response) = accept_h2_request(listener).await;
        let response_header = Response::builder()
            .status(StatusCode::OK)
            .header(header::CACHE_CONTROL, "max-age=30")
            .body(())
            .unwrap();
        let mut response_body = response.send_response(response_header, false).unwrap();
        response_body
            .send_data(Bytes::from_static(b"partial"), false)
            .unwrap();
        response_started_tx.send(()).unwrap();

        let reason = poll_fn(|cx| response_body.poll_reset(cx)).await.unwrap();
        upstream_reset_tx.send(reason).unwrap();
    });

    let mut client = h2c_client(6154).await;

    let attempts_before = recorded_upstream_attempts(path);
    let completed_before = recorded_request_completions(path);
    let abandoned_calls_before = downstream_abandoned_during_cache_fill_calls(path);
    let request = opt_in(Request::builder(), handles_abandonment)
        .method("GET")
        .uri(format!("http://127.0.0.1:6154{path}"))
        .header("x-h2", "true")
        .header("x-port", upstream_port.to_string())
        .header("x-lock", "true")
        .header("x-cache-fail-finish", "true")
        .header("x-record-request-lifecycle", "true")
        .body(())
        .unwrap();
    let (response, mut request_body) = client.send_request(request, true).unwrap();
    tokio::time::timeout(TEST_TIMEOUT, response_started_rx)
        .await
        .expect("upstream response did not start")
        .unwrap();
    let response = tokio::time::timeout(TEST_TIMEOUT, response)
        .await
        .expect("proxy did not send response headers")
        .unwrap();
    let mut response_body = response.into_body();
    let first_chunk = read_h2_body_chunk(&mut response_body).await;
    assert_eq!(first_chunk, Bytes::from_static(b"partial"));

    request_body.send_reset(h2::Reason::CANCEL);
    let reason = tokio::time::timeout(TEST_TIMEOUT, upstream_reset_rx)
        .await
        .expect("proxy did not stop the coupled upstream response")
        .unwrap();
    assert_eq!(reason, h2::Reason::CANCEL);
    tokio::time::timeout(TEST_TIMEOUT, upstream)
        .await
        .expect("upstream task did not finish")
        .unwrap();
    wait_for_recorded_request_completion(path, completed_before).await;
    assert!(
        recorded_request_error(path).is_some(),
        "the downstream reset was not propagated as the request error"
    );
    assert_eq!(recorded_upstream_attempts(path), attempts_before + 1);
    assert_eq!(
        downstream_abandoned_during_cache_fill_calls(path),
        abandoned_calls_before,
        "a coupled cache admission must not run the detached-fill callback"
    );

    assert_failed_fill_recovers(path).await;
}

#[tokio::test]
async fn test_h2_downstream_reset_aborts_non_streaming_cache_admission() {
    assert_downstream_reset_aborts_non_streaming_cache_admission(
        "/test_h2_downstream_reset_aborts_non_streaming_cache_admission_gated",
        true,
    )
    .await;
    // Nothing differs without the opt-in, since a fill that cannot outlive its client is never
    // abandoned.
    assert_downstream_reset_aborts_non_streaming_cache_admission(
        "/test_h2_downstream_reset_aborts_non_streaming_cache_admission_ungated",
        false,
    )
    .await;
}

/// The same loss of the client as
/// `assert_downstream_reset_aborts_non_streaming_cache_admission`, with an HTTP/1 upstream: the
/// proxy closes the upstream connection instead of reading the rest of its response.
async fn assert_h1_upstream_stops_when_non_streaming_cache_admission_loses_downstream(
    path: &'static str,
    handles_abandonment: bool,
) {
    init();

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_port = listener.local_addr().unwrap().port();
    let (response_started_tx, response_started_rx) = tokio::sync::oneshot::channel();

    let upstream = tokio::spawn(async move {
        let (mut socket, _) = tokio::time::timeout(TEST_TIMEOUT, listener.accept())
            .await
            .expect("proxy did not connect to the HTTP/1 upstream")
            .unwrap();
        read_http1_head(&mut socket).await;
        socket
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Length: 14\r\nCache-Control: max-age=30\r\n\r\npartial",
            )
            .await
            .unwrap();
        response_started_tx.send(()).unwrap();

        let mut byte = [0];
        let read = tokio::time::timeout(TEST_TIMEOUT, socket.read(&mut byte))
            .await
            .expect("proxy kept reading a non-streaming fill after downstream loss")
            .unwrap();
        assert_eq!(read, 0, "proxy sent unexpected data to the upstream");
    });

    let attempts_before = recorded_upstream_attempts(path);
    let completed_before = recorded_request_completions(path);
    let abandoned_calls_before = downstream_abandoned_during_cache_fill_calls(path);
    let mut client = h2c_client(6154).await;
    let request = opt_in(Request::builder(), handles_abandonment)
        .method("GET")
        .uri(format!("http://127.0.0.1:6154{path}"))
        .header("x-port", upstream_port.to_string())
        .header("x-lock", "true")
        .header("x-cache-fail-finish", "true")
        .header("x-record-request-lifecycle", "true")
        .body(())
        .unwrap();
    let (response, mut request_body) = client.send_request(request, true).unwrap();
    response_started_rx.await.unwrap();
    let response = tokio::time::timeout(TEST_TIMEOUT, response)
        .await
        .expect("proxy did not send response headers")
        .unwrap();
    let mut response_body = response.into_body();
    let first_chunk = read_h2_body_chunk(&mut response_body).await;
    assert_eq!(first_chunk, Bytes::from_static(b"partial"));

    request_body.send_reset(h2::Reason::CANCEL);
    tokio::time::timeout(TEST_TIMEOUT, upstream)
        .await
        .expect("HTTP/1 upstream task did not stop")
        .unwrap();
    wait_for_recorded_request_completion(path, completed_before).await;
    assert!(
        recorded_request_error(path).is_some(),
        "the downstream reset was not recorded as the request error"
    );
    assert_eq!(recorded_upstream_attempts(path), attempts_before + 1);
    assert_eq!(
        downstream_abandoned_during_cache_fill_calls(path),
        abandoned_calls_before,
        "a coupled cache admission must not run the detached-fill callback"
    );
}

#[tokio::test]
async fn test_h1_upstream_stops_when_non_streaming_cache_admission_loses_downstream() {
    assert_h1_upstream_stops_when_non_streaming_cache_admission_loses_downstream(
        "/test_h1_upstream_stops_when_non_streaming_cache_admission_loses_downstream_gated",
        true,
    )
    .await;
    // Nothing differs without the opt-in, since a fill that cannot outlive its client is never
    // abandoned.
    assert_h1_upstream_stops_when_non_streaming_cache_admission_loses_downstream(
        "/test_h1_upstream_stops_when_non_streaming_cache_admission_loses_downstream_ungated",
        false,
    )
    .await;
}
