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
use crate::utils::server_utils::{upstream_response_hook_calls, UpstreamResponseHookCalls};
use std::time::{Duration, Instant};

const CACHE_HOST: &str = "127.0.0.1:6154";
const BODY_FILTER_DELAY: Duration = Duration::from_millis(350);

async fn serve_h1_origin(
    listener: TcpListener,
    send_remainder: tokio::sync::oneshot::Receiver<()>,
) {
    let (mut socket, _) = tokio::time::timeout(TEST_TIMEOUT, listener.accept())
        .await
        .expect("proxy did not connect to the HTTP/1 origin")
        .unwrap();
    read_h1_head(&mut socket).await;
    socket
        .write_all(
            b"HTTP/1.1 200 OK\r\nCache-Control: max-age=30\r\nTransfer-Encoding: chunked\r\nTrailer: x-origin-trailer\r\n\r\nc\r\nbefore reset\r\n",
        )
        .await
        .unwrap();

    send_remainder.await.unwrap();
    socket
        .write_all(b"11\r\nafter abandonment\r\n0\r\nx-origin-trailer: done\r\n\r\n")
        .await
        .unwrap();
}

async fn serve_h2_origin(
    listener: TcpListener,
    send_remainder: tokio::sync::oneshot::Receiver<()>,
) {
    let (_request, mut response) = accept_h2_request(listener).await;
    let response_header = Response::builder()
        .status(StatusCode::OK)
        .header(header::CACHE_CONTROL, "max-age=30")
        .body(())
        .unwrap();
    let mut response_body = response.send_response(response_header, false).unwrap();
    response_body
        .send_data(Bytes::from_static(b"before reset"), false)
        .unwrap();

    send_remainder.await.unwrap();
    response_body
        .send_data(Bytes::from_static(b"after abandonment"), false)
        .unwrap();
    let mut trailers = http::HeaderMap::new();
    trailers.insert("x-origin-trailer", "done".parse().unwrap());
    response_body.send_trailers(trailers).unwrap();
}

async fn wait_for_hook_calls(path: &str, minimum: UpstreamResponseHookCalls) {
    wait_until(&format!("upstream hooks did not run for {path}"), || {
        let calls = upstream_response_hook_calls(path);
        calls.headers >= minimum.headers
            && calls.body_chunks >= minimum.body_chunks
            && calls.trailers >= minimum.trailers
    })
    .await;
}

/// A client resets its stream during a miss once it has the first body chunk. The fill goes on
/// through the upstream hooks, whose body filter rewrites the rest of the body to the data the
/// downstream body filter rejects: that filter must not run for the reset client.
async fn assert_upstream_hooks_continue_after_downstream_reset(path: &str, upstream: Upstream) {
    init();

    assert_eq!(
        upstream_response_hook_calls(path),
        UpstreamResponseHookCalls::default(),
        "test path was already used"
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_port = listener.local_addr().unwrap().port();
    let (send_remainder_tx, send_remainder_rx) = tokio::sync::oneshot::channel();
    let origin = tokio::spawn(async move {
        match upstream {
            Upstream::H1 => serve_h1_origin(listener, send_remainder_rx).await,
            Upstream::H2 => serve_h2_origin(listener, send_remainder_rx).await,
        }
    });

    let mut client = h2c_client(6154).await;

    let attempts_before = recorded_upstream_attempts(path);
    let completed_before = recorded_request_completions(path);
    let abandoned_before = downstream_abandoned_during_cache_fill_calls(path);
    let warn_calls_before = downstream_cache_warn_log_calls_for(path);
    let request = upstream
        .select(Request::builder())
        .header(HANDLE_ABANDONMENT.0, HANDLE_ABANDONMENT.1)
        .method("GET")
        .uri(format!("http://{CACHE_HOST}{path}"))
        .header(header::HOST, CACHE_HOST)
        .header("x-port", upstream_port.to_string())
        .header("x-lock", "true")
        .header("x-record-request-lifecycle", "true")
        .header("x-test-upstream-response-hooks", "true")
        .header("x-fail-downstream-filter-after-abandonment", "true")
        .header(
            "x-upstream-hook-delay-ms",
            BODY_FILTER_DELAY.as_millis().to_string(),
        );
    let (response, mut request_body) = client
        .send_request(request.body(()).unwrap(), true)
        .unwrap();
    let response = tokio::time::timeout(TEST_TIMEOUT, response)
        .await
        .expect("proxy did not send response headers")
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers().get("x-cache-status").unwrap(), "miss");
    assert_eq!(
        response.headers().get("x-upstream-hook-applied").unwrap(),
        "true"
    );

    let mut response_body = response.into_body();
    let first_chunk = read_h2_body_chunk(&mut response_body).await;
    assert_eq!(first_chunk, Bytes::from_static(b"before reset"));
    wait_for_hook_calls(
        path,
        UpstreamResponseHookCalls {
            headers: 1,
            body_chunks: 1,
            trailers: 0,
        },
    )
    .await;

    request_body.send_reset(h2::Reason::CANCEL);
    wait_for_ignored_downstream_error(path, warn_calls_before).await;
    assert_eq!(
        downstream_abandoned_during_cache_fill_calls(path),
        abandoned_before + 1
    );

    let delay_started = Instant::now();
    send_remainder_tx.send(()).unwrap();
    let expected_trailer_calls = usize::from(matches!(upstream, Upstream::H2));
    wait_for_hook_calls(
        path,
        UpstreamResponseHookCalls {
            headers: 1,
            body_chunks: 2,
            trailers: expected_trailer_calls,
        },
    )
    .await;
    wait_for_recorded_request_completion(path, completed_before).await;
    assert!(
        delay_started.elapsed() >= BODY_FILTER_DELAY - Duration::from_millis(50),
        "upstream body filter delay was skipped for {path}"
    );
    // The downstream body filter fails the request if it sees the rewritten data.
    assert_eq!(
        recorded_request_error(path),
        None,
        "a downstream filter ran for the reset client, or the fill failed"
    );
    assert_eq!(recorded_upstream_attempts(path), attempts_before + 1);
    assert_eq!(
        downstream_cache_warn_log_calls_for(path),
        warn_calls_before + 1
    );
    assert_eq!(
        downstream_abandoned_during_cache_fill_calls(path),
        abandoned_before + 1,
        "finishing the cache fill repeated the abandonment callback"
    );
    let calls_after_fill = upstream_response_hook_calls(path);
    tokio::time::timeout(TEST_TIMEOUT, origin)
        .await
        .expect("origin task did not finish")
        .unwrap();

    let cached_response = reqwest::Client::new()
        .get(format!("http://127.0.0.1:6148{path}"))
        .header(header::HOST, CACHE_HOST)
        .send()
        .await
        .unwrap();
    assert_eq!(cached_response.status(), StatusCode::OK);
    assert_eq!(
        cached_response.headers().get("x-cache-status").unwrap(),
        "hit"
    );
    assert_eq!(
        cached_response
            .headers()
            .get("x-upstream-hook-applied")
            .unwrap(),
        "true"
    );
    assert_eq!(
        cached_response.bytes().await.unwrap(),
        b"before resetafter filter data".as_slice()
    );
    assert_eq!(upstream_response_hook_calls(path), calls_after_fill);
}

#[tokio::test]
async fn test_http1_abandoned_cache_fill_runs_upstream_hooks_without_downstream_filters() {
    bounded(assert_upstream_hooks_continue_after_downstream_reset(
        "/test_h1_upstream_hooks_after_downstream_reset_gated",
        Upstream::H1,
    ))
    .await;
}

#[tokio::test]
async fn test_http2_abandoned_cache_fill_runs_upstream_hooks_without_downstream_filters() {
    bounded(assert_upstream_hooks_continue_after_downstream_reset(
        "/test_h2_upstream_hooks_after_downstream_reset_gated",
        Upstream::H2,
    ))
    .await;
}
