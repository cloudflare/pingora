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
use crate::utils::server_utils::recorded_fail_to_proxy_code;

#[derive(Clone, Copy)]
enum PostAbandonmentUpstreamFailure {
    BodyFilter,
    Reset,
}

/// A GET whose client resets the stream after receiving the start of an admitted miss. The
/// proxy ignores the reset so that the fill can continue, until `upstream_failure` fails the
/// upstream response, which the error hook makes retryable. The partial fill is discarded and
/// the cache lock released either way.
///
/// An opted-in request is not retried: the retry would start a second cache admission on top of
/// the abandoned one. Without the opt-in the proxy retries until its retry limit, every retried
/// stream being reset by the upstream before a response header.
async fn assert_h2_post_admission_failure_after_reset(
    upstream_failure: PostAbandonmentUpstreamFailure,
    path: &'static str,
    handles_abandonment: bool,
) {
    init();

    let uri = format!("http://127.0.0.1:6154{path}");
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_port = listener.local_addr().unwrap().port();
    let (send_after_reset_tx, send_after_reset_rx) = tokio::sync::oneshot::channel();
    let (trigger_upstream_failure_tx, trigger_upstream_failure_rx) =
        tokio::sync::oneshot::channel();

    let upstream = tokio::spawn(async move {
        let (_request, mut response) = accept_h2_request(listener).await;
        let response_header = Response::builder()
            .status(200)
            .header("cache-control", "max-age=30")
            .body(())
            .unwrap();
        let mut response_body = response.send_response(response_header, false).unwrap();
        response_body
            .send_data(Bytes::from_static(b"hello"), false)
            .unwrap();

        send_after_reset_rx.await.unwrap();
        response_body
            .send_data(Bytes::from_static(b"after reset"), false)
            .unwrap();

        trigger_upstream_failure_rx.await.unwrap();
        match upstream_failure {
            PostAbandonmentUpstreamFailure::BodyFilter => response_body
                .send_data(
                    Bytes::from_static(b"trigger retryable body filter error"),
                    false,
                )
                .unwrap(),
            PostAbandonmentUpstreamFailure::Reset => {
                response_body.send_reset(h2::Reason::INTERNAL_ERROR)
            }
        }
    });

    let mut client = h2c_client(6154).await;
    let attempts_before = recorded_upstream_attempts(path);
    let completed_before = recorded_request_completions(path);
    let forced_retry_calls_before = forced_retry_error_hook_calls(path);
    let nonretryable_hook_calls_before = forced_retry_error_hook_calls_with_retry_disabled(path);
    let mut request = opt_in(Request::builder(), handles_abandonment)
        .method("GET")
        .uri(uri.as_str())
        .header("x-h2", "true")
        .header("x-port", upstream_port.to_string())
        .header("x-lock", "true")
        .header("x-record-request-lifecycle", "true")
        .header("x-force-retry-in-error-hook", "true");
    if matches!(upstream_failure, PostAbandonmentUpstreamFailure::BodyFilter) {
        request = request.header("x-retryable-upstream-body-filter-error", "true");
    }
    let request = request.body(()).unwrap();
    let (response, mut request_body) = client.send_request(request, true).unwrap();
    let response = tokio::time::timeout(TEST_TIMEOUT, response)
        .await
        .expect("proxy did not send response headers")
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers().get("x-cache-status").unwrap(), "miss");
    let mut response_body = response.into_body();
    let first_chunk = read_h2_body_chunk(&mut response_body).await;
    assert_eq!(first_chunk, Bytes::from_static(b"hello"));

    let abandoned_calls_before = downstream_abandoned_during_cache_fill_calls(path);
    let warn_calls_before = downstream_cache_warn_log_calls_for(path);
    request_body.send_reset(h2::Reason::CANCEL);
    send_after_reset_tx.send(()).unwrap();
    wait_for_ignored_downstream_error(path, warn_calls_before).await;
    // Only an opted-in request reports the ignored reset as an abandonment, once.
    let expected_callbacks = usize::from(handles_abandonment);
    assert_eq!(
        downstream_abandoned_during_cache_fill_calls(path),
        abandoned_calls_before + expected_callbacks,
        "the reset was not reported as an abandonment exactly when the request opted in"
    );

    trigger_upstream_failure_tx.send(()).unwrap();
    tokio::time::timeout(TEST_TIMEOUT, upstream)
        .await
        .expect("first upstream task did not finish")
        .unwrap();

    tokio::time::timeout(TEST_TIMEOUT, async {
        while let Some(chunk) = response_body.data().await {
            match chunk {
                Ok(chunk) => {
                    response_body
                        .flow_control()
                        .release_capacity(chunk.len())
                        .unwrap();
                }
                Err(_) => break,
            }
        }
    })
    .await
    .expect("abandoned downstream response did not finish");
    wait_for_recorded_request_completion(path, completed_before).await;
    let attempts = recorded_upstream_attempts(path) - attempts_before;
    let forced_retry_calls = forced_retry_error_hook_calls(path) - forced_retry_calls_before;
    if handles_abandonment {
        assert_eq!(
            recorded_fail_to_proxy_code(path),
            Some(0),
            "an abandoned request reports 0, as with the default fail_to_proxy: its downstream is gone"
        );
        assert_eq!(
            forced_retry_calls, 1,
            "the upstream failure did not pass through the retry override"
        );
        assert_eq!(
            forced_retry_error_hook_calls_with_retry_disabled(path),
            nonretryable_hook_calls_before + 1,
            "the retry override did not receive the expected non-retryable error"
        );
        assert_eq!(attempts, 1, "proxy retried after cache-fill abandonment");
    } else {
        assert_eq!(
            (attempts, forced_retry_calls),
            (retry_limit(), retry_limit()),
            "without the opt-in every failed attempt is retried until the retry limit"
        );
    }

    assert_failed_fill_recovers(path).await;
    assert_eq!(
        downstream_abandoned_during_cache_fill_calls(path),
        abandoned_calls_before + expected_callbacks,
        "failure cleanup must not repeat the callback"
    );
}

/// The test proxy's retry limit: its configuration file leaves `max_retries` at the default.
fn retry_limit() -> usize {
    pingora_core::server::configuration::ServerConf::default().max_retries
}

#[tokio::test]
async fn test_h2_post_admission_body_filter_error_stops_retry_and_discards_partial_fill() {
    bounded(assert_h2_post_admission_failure_after_reset(
        PostAbandonmentUpstreamFailure::BodyFilter,
        "/test_h2_post_admission_body_filter_error_gated",
        true,
    ))
    .await;
    // Without the opt-in nothing vetoes the retry the error hook allowed.
    bounded(assert_h2_post_admission_failure_after_reset(
        PostAbandonmentUpstreamFailure::BodyFilter,
        "/test_h2_post_admission_body_filter_error_ungated",
        false,
    ))
    .await;
}

#[tokio::test]
async fn test_h2_post_admission_upstream_reset_stops_retry_and_discards_partial_fill() {
    bounded(assert_h2_post_admission_failure_after_reset(
        PostAbandonmentUpstreamFailure::Reset,
        "/test_h2_post_admission_upstream_reset_gated",
        true,
    ))
    .await;
    // Without the opt-in nothing vetoes the retry the error hook allowed.
    bounded(assert_h2_post_admission_failure_after_reset(
        PostAbandonmentUpstreamFailure::Reset,
        "/test_h2_post_admission_upstream_reset_ungated",
        false,
    ))
    .await;
}
