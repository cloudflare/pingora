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
use std::future::poll_fn;

async fn assert_h2_downstream_rst_during_cache_fill(path: &str, handles_abandonment: bool) {
    init();

    let uri = format!("http://127.0.0.1:6154{path}");
    let reported = usize::from(handles_abandonment);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_port = listener.local_addr().unwrap().port();
    let (upstream_ready_tx, upstream_ready_rx) = tokio::sync::oneshot::channel::<()>();
    let (upstream_window_filled_tx, upstream_window_filled_rx) =
        tokio::sync::oneshot::channel::<()>();
    let (finish_tx, finish_rx) = tokio::sync::oneshot::channel::<()>();
    let (release_streams_tx, release_streams_rx) = tokio::sync::oneshot::channel::<()>();

    let upstream = tokio::spawn(async move {
        let (io, _) = listener.accept().await.unwrap();
        let mut connection = h2::server::Builder::new()
            .initial_window_size(UPSTREAM_STREAM_WINDOW_BYTES as u32)
            .handshake::<_, Bytes>(io)
            .await
            .unwrap();
        let (request, mut response) = connection.accept().await.unwrap().unwrap();
        let mut request_body = request.into_body();
        let mut ping_pong = connection.ping_pong().unwrap();
        let connection_task = tokio::spawn(async move {
            while let Some(request) = connection.accept().await {
                if request.is_err() {
                    break;
                }
            }
        });
        ping_pong.ping(h2::Ping::opaque()).await.unwrap();
        upstream_ready_tx.send(()).unwrap();

        let response_header = Response::builder()
            .status(StatusCode::OK)
            .header(header::CACHE_CONTROL, "max-age=30")
            .body(())
            .unwrap();
        let mut response_body = response.send_response(response_header, false).unwrap();
        response_body
            .send_data(Bytes::from_static(b"hello "), false)
            .unwrap();

        fill_upstream_window_without_releasing(&mut request_body, UPSTREAM_STREAM_WINDOW_BYTES)
            .await;
        upstream_window_filled_tx.send(()).unwrap();

        finish_rx.await.unwrap();
        response_body
            .send_data(Bytes::from_static(b"world!"), true)
            .unwrap();

        // Keep both handles until the test verifies the cached body. Dropping them early
        // can queue NO_ERROR before the proxy reads the response EOS.
        release_streams_rx.await.unwrap();
        drop(request_body);
        drop(response_body);
        connection_task.abort();
    });

    let mut client = h2c_client(6154).await;
    let attempts_before = recorded_upstream_attempts(path);
    let completed_before = recorded_request_completions(path);
    let abandoned_before = downstream_abandoned_during_cache_fill_calls(path);
    let warn_calls_before = downstream_cache_warn_log_calls_for(path);
    let request = opt_in(Request::builder(), handles_abandonment)
        .method("POST")
        .uri(uri.as_str())
        .header("x-h2", "true")
        .header("x-port", upstream_port.to_string())
        .header("x-record-request-lifecycle", "true")
        .body(())
        .unwrap();
    let (response, mut request_body) = client.send_request(request, false).unwrap();

    upstream_ready_rx.await.unwrap();
    request_body.reserve_capacity(REQUEST_BODY_PREFIX.len());
    let available = poll_fn(|cx| request_body.poll_capacity(cx))
        .await
        .unwrap()
        .unwrap();
    assert!(available >= REQUEST_BODY_PREFIX.len());
    request_body
        .send_data(Bytes::from_static(REQUEST_BODY_PREFIX), false)
        .unwrap();

    let response = response.await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["x-cache-status"], "miss");
    let mut response_body = response.into_body();
    let first_chunk = response_body.data().await.unwrap().unwrap();
    assert_eq!(first_chunk, Bytes::from_static(b"hello "));
    response_body
        .flow_control()
        .release_capacity(first_chunk.len())
        .unwrap();

    // Admission and response delivery must precede the blocked upstream upload.
    block_upstream_write_with_flow_control(&mut request_body, upstream_window_filled_rx).await;
    assert_eq!(recorded_request_completions(path), completed_before);
    assert_eq!(
        downstream_abandoned_during_cache_fill_calls(path),
        abandoned_before
    );
    request_body.send_reset(h2::Reason::CANCEL);
    wait_for_ignored_downstream_error(path, warn_calls_before).await;
    assert_eq!(
        downstream_abandoned_during_cache_fill_calls(path),
        abandoned_before + reported
    );
    assert_eq!(
        recorded_request_completions(path),
        completed_before,
        "the request completed while the cache fill was unfinished"
    );

    finish_tx.send(()).unwrap();
    wait_for_recorded_request_completion(path, completed_before).await;
    assert_eq!(recorded_request_error(path), None);
    assert_eq!(recorded_upstream_attempts(path), attempts_before + 1);
    assert_eq!(
        wait_for_h2_cache_hit(&uri, upstream_port).await,
        b"hello world!"
    );
    assert_eq!(
        downstream_abandoned_during_cache_fill_calls(path),
        abandoned_before + reported,
        "finishing the cache fill repeated the abandonment callback"
    );

    release_streams_tx.send(()).unwrap();
    upstream.await.unwrap();
}

// The reset must interrupt a blocked upstream upload while the admitted cache fill completes.
#[tokio::test]
async fn test_h2_downstream_rst_during_cache_fill() {
    bounded(assert_h2_downstream_rst_during_cache_fill(
        "/test_h2_downstream_rst_during_gated_cache_fill",
        true,
    ))
    .await;
    // Without the opt-in the reset is ignored for the fill but not reported.
    bounded(assert_h2_downstream_rst_during_cache_fill(
        "/test_h2_downstream_rst_during_ungated_cache_fill",
        false,
    ))
    .await;
}

/// How much of the response body the downstream client's stream window admits. The proxy's
/// write of the rest of the cached body then blocks until the client releases capacity.
const DOWNSTREAM_STREAM_WINDOW_BYTES: usize = 5;
const FINISHED_FILL_BODY: &[u8] = b"hello world!";

/// An opted-in client resets its stream while the proxy is still serving it the cached body of a
/// miss whose cache fill has already finished. With nothing left to fill, the reset is handled as
/// without abandonment handling: ignored for the finished fill, reported only as an ignored
/// downstream error, and the request still succeeds.
async fn assert_h2_downstream_rst_after_cache_fill_finished(path: &str) {
    init();

    let uri = format!("http://127.0.0.1:6154{path}");
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_port = listener.local_addr().unwrap().port();
    let (release_streams_tx, release_streams_rx) = tokio::sync::oneshot::channel::<()>();
    let upstream = tokio::spawn(async move {
        let (_request, mut response) = accept_h2_request(listener).await;
        let response_header = Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_LENGTH, FINISHED_FILL_BODY.len().to_string())
            .header(header::CACHE_CONTROL, "max-age=30")
            .body(())
            .unwrap();
        let mut response_body = response.send_response(response_header, false).unwrap();
        // A single DATA frame that ends the stream: the proxy writes the whole body to the
        // cache and finishes the fill in one step, before any of it is served downstream.
        response_body
            .send_data(Bytes::from_static(FINISHED_FILL_BODY), true)
            .unwrap();
        release_streams_rx.await.unwrap();
        drop(response_body);
    });

    let tcp = TcpStream::connect("127.0.0.1:6154").await.unwrap();
    let (mut client, connection) = client::Builder::new()
        .initial_window_size(DOWNSTREAM_STREAM_WINDOW_BYTES as u32)
        .handshake::<_, Bytes>(tcp)
        .await
        .unwrap();
    let downstream_connection = tokio::spawn(async move {
        let _ = connection.await;
    });
    let attempts_before = recorded_upstream_attempts(path);
    let completed_before = recorded_request_completions(path);
    let abandoned_before = downstream_abandoned_during_cache_fill_calls(path);
    let warn_calls_before = downstream_cache_warn_log_calls_for(path);
    let request = Request::builder()
        .header(HANDLE_ABANDONMENT.0, HANDLE_ABANDONMENT.1)
        .method("GET")
        .uri(uri.as_str())
        .header("x-h2", "true")
        .header("x-port", upstream_port.to_string())
        .header("x-record-request-lifecycle", "true")
        .body(())
        .unwrap();
    let (response, mut request_stream) = client.send_request(request, true).unwrap();
    let response = tokio::time::timeout(TEST_TIMEOUT, response)
        .await
        .expect("proxy did not send the response header")
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["x-cache-status"], "miss");

    // Take what the stream window admits without releasing it, so the proxy's write of the
    // rest of the cached body stays blocked.
    let mut response_body = response.into_body();
    let mut received = Vec::new();
    while received.len() < DOWNSTREAM_STREAM_WINDOW_BYTES {
        let chunk = tokio::time::timeout(TEST_TIMEOUT, response_body.data())
            .await
            .expect("cached response body did not start")
            .expect("cached response body ended early")
            .unwrap();
        received.extend_from_slice(&chunk);
    }
    assert_eq!(
        received,
        &FINISHED_FILL_BODY[..DOWNSTREAM_STREAM_WINDOW_BYTES]
    );

    request_stream.send_reset(h2::Reason::CANCEL);
    wait_for_recorded_request_completion(path, completed_before).await;
    assert_eq!(
        recorded_request_error(path),
        None,
        "a reset after the cache fill finished failed the request"
    );
    assert_eq!(
        downstream_cache_warn_log_calls_for(path),
        warn_calls_before + 1,
        "the ignored downstream error was not reported"
    );
    assert_eq!(
        downstream_abandoned_during_cache_fill_calls(path),
        abandoned_before,
        "a reset after the cache fill finished was reported as abandoning it"
    );
    assert_eq!(recorded_upstream_attempts(path), attempts_before + 1);
    assert_eq!(
        wait_for_h2_cache_hit(&uri, upstream_port).await,
        FINISHED_FILL_BODY
    );

    release_streams_tx.send(()).unwrap();
    upstream.await.unwrap();
    downstream_connection.abort();
}

#[tokio::test]
async fn test_h2_downstream_rst_after_cache_fill_finished_is_not_abandonment() {
    bounded(assert_h2_downstream_rst_after_cache_fill_finished(
        "/test_h2_downstream_rst_after_cache_fill_finished_gated",
    ))
    .await;
}
