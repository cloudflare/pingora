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

async fn prime_cache_entry(
    path: &str,
    cache_host: &str,
    cache_control: &'static str,
    handles_abandonment: bool,
) {
    let prime_origin = HttpOrigin::bind(move |_request| async move {
        Response::builder()
            .header(header::CONTENT_LENGTH, "11")
            .header(header::CACHE_CONTROL, cache_control)
            .header(header::ETAG, "\"v1\"")
            .body(Bytes::from_static(b"cached body"))
            .unwrap()
    })
    .await
    .unwrap();
    let prime_origin_port = prime_origin.addr().port();

    let primed = client_opting_in(handles_abandonment)
        .get(format!("http://127.0.0.1:6148{path}"))
        .header(header::HOST, cache_host)
        .header("x-port", prime_origin_port.to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(primed.status(), StatusCode::OK);
    assert_eq!(primed.headers().get("x-cache-status").unwrap(), "miss");
    assert_eq!(primed.bytes().await.unwrap(), b"cached body".as_slice());
    prime_origin.shutdown().await;
}

/// How the origin answers a revalidation whose client has reset its stream.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Revalidation {
    /// A 304, which refreshes the entry.
    NotModified,
    /// A new 200 response, sent in two parts, which replaces the entry.
    Replaced,
    /// A 503 for an entry that may be served stale on error.
    StaleIfError,
}

/// A GET revalidating an expired entry whose client resets the stream before the upstream
/// answers. Streaming partial cache writes are supported, so the proxy ignores the reset, and
/// the upstream's answer is applied as for a client that stayed: a 304 refreshes the entry, a
/// 200 replaces it, and a 503 keeps the entry that may be served stale on that error, without a
/// retry, and releases the cache lock for the next revalidation.
async fn assert_h2_reset_during_revalidation(
    path: &'static str,
    revalidation: Revalidation,
    handles_abandonment: bool,
) {
    init();

    const CACHE_HOST: &str = "127.0.0.1:6154";
    let uri = format!("http://{CACHE_HOST}{path}");
    let cache_control = match revalidation {
        Revalidation::StaleIfError => "max-age=0, stale-if-error=30",
        Revalidation::NotModified | Revalidation::Replaced => "max-age=0",
    };
    prime_cache_entry(path, CACHE_HOST, cache_control, handles_abandonment).await;

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_port = listener.local_addr().unwrap().port();
    let (request_seen_tx, request_seen_rx) = tokio::sync::oneshot::channel();
    let (respond_tx, respond_rx) = tokio::sync::oneshot::channel();
    let (response_started_tx, response_started_rx) = tokio::sync::oneshot::channel();
    let (finish_response_tx, finish_response_rx) = tokio::sync::oneshot::channel();
    let upstream = tokio::spawn(async move {
        let (request, mut response) = accept_h2_request(listener).await;
        assert_eq!(
            request.headers().get(header::IF_NONE_MATCH).unwrap(),
            "\"v1\""
        );
        request_seen_tx.send(()).unwrap();

        respond_rx.await.unwrap();
        let response_header = match revalidation {
            Revalidation::NotModified => Response::builder()
                .status(StatusCode::NOT_MODIFIED)
                .header(header::CACHE_CONTROL, "max-age=30")
                .header(header::ETAG, "\"v1\""),
            Revalidation::Replaced => Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_LENGTH, "10")
                .header(header::CACHE_CONTROL, "max-age=30")
                .header(header::ETAG, "\"v2\""),
            Revalidation::StaleIfError => {
                Response::builder().status(StatusCode::SERVICE_UNAVAILABLE)
            }
        }
        .body(())
        .unwrap();
        if revalidation != Revalidation::Replaced {
            response.send_response(response_header, true).unwrap();
            return;
        }
        let mut response_body = response.send_response(response_header, false).unwrap();
        response_body
            .send_data(Bytes::from_static(b"fresh "), false)
            .unwrap();
        response_started_tx.send(()).unwrap();
        finish_response_rx.await.unwrap();
        response_body
            .send_data(Bytes::from_static(b"body"), true)
            .unwrap();
    });

    let mut client = h2c_client(6154).await;
    let mut request = opt_in(Request::builder(), handles_abandonment)
        .method("GET")
        .uri(uri.as_str())
        .header("x-h2", "true")
        .header("x-port", upstream_port.to_string())
        .header("x-lock", "true")
        .header("x-record-request-lifecycle", "true");
    if revalidation != Revalidation::StaleIfError {
        request = request.header("x-force-expire", "true");
    }
    let attempts_before = recorded_upstream_attempts(path);
    let completed_before = recorded_request_completions(path);
    let (response, mut request_body) = client
        .send_request(request.body(()).unwrap(), true)
        .unwrap();
    tokio::time::timeout(TEST_TIMEOUT, request_seen_rx)
        .await
        .expect("origin did not receive the revalidation request")
        .unwrap();

    let abandoned_calls_before = downstream_abandoned_during_cache_fill_calls(path);
    let warn_calls_before = downstream_cache_warn_log_calls_for(path);
    request_body.send_reset(h2::Reason::CANCEL);
    wait_for_ignored_downstream_error(path, warn_calls_before).await;
    // The proxy ignores the reset either way; only an opted-in request reports it, at once.
    let expected_callbacks = usize::from(handles_abandonment);
    assert_eq!(
        downstream_abandoned_during_cache_fill_calls(path),
        abandoned_calls_before + expected_callbacks,
        "the reset was not reported as an abandonment exactly when the request opted in"
    );

    respond_tx.send(()).unwrap();
    if revalidation == Revalidation::Replaced {
        response_started_rx.await.unwrap();
        assert_eq!(
            downstream_abandoned_during_cache_fill_calls(path),
            abandoned_calls_before + expected_callbacks,
            "replacement cache admission repeated the abandonment callback"
        );
        finish_response_tx.send(()).unwrap();
    }
    assert!(
        tokio::time::timeout(TEST_TIMEOUT, response)
            .await
            .expect("abandoned revalidation did not finish")
            .is_err(),
        "abandoned revalidation unexpectedly returned a response"
    );
    tokio::time::timeout(TEST_TIMEOUT, upstream)
        .await
        .expect("revalidation origin task did not finish")
        .unwrap();
    wait_for_recorded_request_completion(path, completed_before).await;
    assert_eq!(recorded_request_error(path), None);
    assert_eq!(
        recorded_upstream_attempts(path),
        attempts_before + 1,
        "proxy retried the abandoned revalidation"
    );

    let cached_body = match revalidation {
        Revalidation::NotModified => b"cached body".as_slice(),
        Revalidation::Replaced => b"fresh body".as_slice(),
        Revalidation::StaleIfError => {
            // The failed revalidation completed, so nothing may hold the cache lock any more.
            let recovery_origin = HttpOrigin::bind(|_request| async {
                Response::builder()
                    .header(header::CONTENT_LENGTH, "10")
                    .header(header::CACHE_CONTROL, "max-age=30")
                    .body(Bytes::from_static(b"fresh body"))
                    .unwrap()
            })
            .await
            .unwrap();
            let client = reqwest::Client::new();
            let recovery_port = recovery_origin.addr().port();
            let recovery_response = request_after_failed_fill(&client, path, recovery_port)
                .header("x-force-expire", "true")
                .send()
                .await
                .expect("the next revalidation remained blocked on the cache lock");
            assert_eq!(recovery_response.status(), StatusCode::OK);
            assert_cache_lock_was_free(recovery_response.headers());
            assert_eq!(
                recovery_response.bytes().await.unwrap(),
                b"fresh body".as_slice()
            );
            let cached = request_after_failed_fill(&client, path, recovery_port)
                .send()
                .await
                .unwrap();
            assert_eq!(cached.headers().get("x-cache-status").unwrap(), "hit");
            assert_eq!(cached.bytes().await.unwrap(), b"fresh body".as_slice());
            recovery_origin.shutdown().await;
            b"fresh body".as_slice()
        }
    };
    assert_eq!(
        wait_for_h2_cache_hit(&uri, upstream_port).await,
        cached_body
    );
    assert_eq!(
        downstream_abandoned_during_cache_fill_calls(path),
        abandoned_calls_before + expected_callbacks,
        "finishing the revalidation repeated the abandonment callback"
    );
}

#[tokio::test]
async fn test_h2_abandoned_revalidation_keeps_cached_response() {
    assert_h2_reset_during_revalidation(
        "/test_h2_abandoned_revalidation_keeps_cached_response_gated",
        Revalidation::NotModified,
        true,
    )
    .await;
    // Without the opt-in the proxy ignores the same reset and applies the 304 just the same, and
    // only the abandonment callback is missing.
    assert_h2_reset_during_revalidation(
        "/test_h2_abandoned_revalidation_keeps_cached_response_ungated",
        Revalidation::NotModified,
        false,
    )
    .await;
}

#[tokio::test]
async fn test_h2_abandoned_expired_entry_replacement_completes_fill() {
    bounded(assert_h2_reset_during_revalidation(
        "/test_h2_abandoned_expired_entry_replacement_gated",
        Revalidation::Replaced,
        true,
    ))
    .await;
    // Without the opt-in the proxy ignores the same reset for the fill just the same, and only
    // the abandonment callback is missing.
    bounded(assert_h2_reset_during_revalidation(
        "/test_h2_abandoned_expired_entry_replacement_ungated",
        Revalidation::Replaced,
        false,
    ))
    .await;
}

#[tokio::test]
async fn test_h2_abandoned_stale_if_error_releases_cache_lock() {
    assert_h2_reset_during_revalidation(
        "/test_h2_abandoned_stale_if_error_releases_cache_lock_gated",
        Revalidation::StaleIfError,
        true,
    )
    .await;
    // Without the opt-in the proxy ignores the same reset and serves the failed revalidation
    // stale just the same, and only the abandonment callback is missing.
    assert_h2_reset_during_revalidation(
        "/test_h2_abandoned_stale_if_error_releases_cache_lock_ungated",
        Revalidation::StaleIfError,
        false,
    )
    .await;
}
