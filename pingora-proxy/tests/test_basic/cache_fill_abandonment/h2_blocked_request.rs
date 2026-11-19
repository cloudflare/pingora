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

struct QueuedStreamPaths {
    /// The request that holds the upstream connection's only stream.
    held: &'static str,
    /// The request queued behind it, whose client resets its stream.
    reset: &'static str,
    /// A later request on the same upstream connection.
    recovery: &'static str,
}

/// A GET whose upstream stream is still queued behind another request when its client resets
/// the stream. Streaming partial cache writes are supported, so the proxy ignores the reset:
/// once a stream is free the request goes out, its cacheable response fills the cache, and the
/// upstream connection stays usable.
async fn assert_pre_admission_reset_of_queued_upstream_stream_fills_cache(
    paths: QueuedStreamPaths,
    handles_abandonment: bool,
) {
    let QueuedStreamPaths {
        held: held_path,
        reset: reset_path,
        recovery: recovery_path,
    } = paths;
    const RESET_BODY: &[u8] = b"detached fill";
    const RECOVERY_BODY: &[u8] = b"same connection";

    init();

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_port = listener.local_addr().unwrap().port();
    let (held_request_tx, held_request_rx) = tokio::sync::oneshot::channel();
    let (release_held_tx, release_held_rx) = tokio::sync::oneshot::channel();
    let (recovery_received_tx, recovery_received_rx) = tokio::sync::oneshot::channel();

    let upstream = tokio::spawn(async move {
        let (io, _) = tokio::time::timeout(TEST_TIMEOUT, listener.accept())
            .await
            .expect("proxy did not open the upstream connection")
            .unwrap();
        let mut connection = h2::server::Builder::new()
            .max_concurrent_streams(1)
            .handshake::<_, Bytes>(io)
            .await
            .unwrap();
        let mut ping_pong = connection.ping_pong().unwrap();

        let (held_request, mut held_response) =
            tokio::time::timeout(TEST_TIMEOUT, connection.accept())
                .await
                .expect("proxy did not send the request that holds the upstream stream")
                .expect("upstream connection closed before the first request")
                .unwrap();
        assert_eq!(held_request.uri().path(), held_path);

        let ping = ping_pong.ping(h2::Ping::opaque());
        tokio::pin!(ping);
        tokio::time::timeout(TEST_TIMEOUT, async {
            tokio::select! {
                result = &mut ping => result.unwrap(),
                request = connection.accept() => {
                    panic!("upstream received another request before the SETTINGS round trip: {request:?}")
                }
            }
        })
        .await
        .expect("proxy did not acknowledge the upstream SETTINGS and PING");
        held_request_tx.send(()).unwrap();

        release_held_rx.await.unwrap();
        held_response
            .send_response(
                Response::builder()
                    .status(StatusCode::NO_CONTENT)
                    .body(())
                    .unwrap(),
                true,
            )
            .unwrap();

        let (reset_request, mut reset_response) =
            tokio::time::timeout(TEST_TIMEOUT, connection.accept())
                .await
                .expect("queued request headers were not flushed after stream capacity returned")
                .expect("the original upstream connection closed")
                .unwrap();
        assert_eq!(reset_request.uri().path(), reset_path);
        let response = Response::builder()
            .status(StatusCode::OK)
            .header(header::CACHE_CONTROL, "max-age=30")
            .header(header::CONTENT_LENGTH, RESET_BODY.len().to_string())
            .body(())
            .unwrap();
        let mut body = reset_response.send_response(response, false).unwrap();
        body.send_data(Bytes::from_static(RESET_BODY), true)
            .unwrap();

        let (recovery_request, mut recovery_response) =
            tokio::time::timeout(TEST_TIMEOUT, connection.accept())
                .await
                .expect("the original upstream connection did not receive the recovery request")
                .expect("the original upstream connection closed")
                .unwrap();
        assert_eq!(recovery_request.uri().path(), recovery_path);
        let response = Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_LENGTH, RECOVERY_BODY.len().to_string())
            .body(())
            .unwrap();
        let mut body = recovery_response.send_response(response, false).unwrap();
        body.send_data(Bytes::from_static(RECOVERY_BODY), true)
            .unwrap();

        let recovery_received = recovery_received_rx;
        tokio::pin!(recovery_received);
        tokio::time::timeout(TEST_TIMEOUT, async {
            tokio::select! {
                result = &mut recovery_received => {
                    result.expect("recovery client stopped before reading the response");
                }
                request = connection.accept() => {
                    match request {
                        Some(Ok((request, _))) => {
                            panic!("upstream received unexpected request {}", request.uri())
                        }
                        Some(Err(error)) => panic!("upstream connection failed: {error}"),
                        None => panic!("upstream connection closed before recovery completed"),
                    }
                }
            }
        })
        .await
        .expect("recovery response did not reach the client");
    });

    let mut client = h2c_client(6154).await;
    let request = |path: &'static str| {
        opt_in(Request::builder(), handles_abandonment)
            .method("GET")
            .uri(format!("http://127.0.0.1:6154{path}"))
            .header("x-h2", "true")
            .header("x-h2-max-streams", "2")
            .header("x-port", upstream_port.to_string())
            .header("x-record-request-lifecycle", "true")
            .body(())
            .unwrap()
    };

    let (held_response, _) = client.send_request(request(held_path), true).unwrap();
    tokio::time::timeout(TEST_TIMEOUT, held_request_rx)
        .await
        .expect("upstream did not receive the request that holds its only stream")
        .unwrap();

    let reset_filters_before = recorded_upstream_request_filters(reset_path);
    let reset_completions_before = recorded_request_completions(reset_path);
    let reset_callbacks_before = downstream_abandoned_during_cache_fill_calls(reset_path);
    let reset_warn_calls_before = downstream_cache_warn_log_calls_for(reset_path);
    let (_reset_response, mut reset_stream) =
        client.send_request(request(reset_path), true).unwrap();

    wait_until(
        "reset request did not reach the upstream request filter",
        || recorded_upstream_request_filters(reset_path) != reset_filters_before,
    )
    .await;
    assert_eq!(
        recorded_upstream_request_filters(reset_path),
        reset_filters_before + 1
    );

    reset_stream.send_reset(h2::Reason::CANCEL);
    wait_for_ignored_downstream_error(reset_path, reset_warn_calls_before).await;
    // Only an opted-in request reports the ignored reset as an abandonment.
    let expected_callbacks = usize::from(handles_abandonment);
    assert_eq!(
        downstream_abandoned_during_cache_fill_calls(reset_path),
        reset_callbacks_before + expected_callbacks
    );

    release_held_tx.send(()).unwrap();
    let held_response = tokio::time::timeout(TEST_TIMEOUT, held_response)
        .await
        .expect("held request did not finish after its upstream stream was released")
        .unwrap();
    assert_eq!(held_response.status(), StatusCode::NO_CONTENT);
    wait_for_recorded_request_completion(reset_path, reset_completions_before).await;
    assert_eq!(recorded_request_error(reset_path), None);
    assert_eq!(
        downstream_abandoned_during_cache_fill_calls(reset_path),
        reset_callbacks_before + expected_callbacks
    );

    let (cached_response, _) = client.send_request(request(reset_path), true).unwrap();
    let cached_response = tokio::time::timeout(TEST_TIMEOUT, cached_response)
        .await
        .expect("detached cache fill did not publish an entry")
        .unwrap();
    assert_eq!(cached_response.status(), StatusCode::OK);
    assert_eq!(cached_response.headers()["x-cache-status"], "hit");
    assert_eq!(
        read_h2_body(&mut cached_response.into_body()).await,
        RESET_BODY
    );

    let (recovery_response, _) = client.send_request(request(recovery_path), true).unwrap();
    let recovery_response = tokio::time::timeout(TEST_TIMEOUT, recovery_response)
        .await
        .expect("follow-up request did not use the original upstream connection")
        .unwrap();
    assert_eq!(recovery_response.status(), StatusCode::OK);
    assert_eq!(
        read_h2_body(&mut recovery_response.into_body()).await,
        RECOVERY_BODY
    );
    recovery_received_tx.send(()).unwrap();

    tokio::time::timeout(TEST_TIMEOUT, upstream)
        .await
        .expect("upstream task did not finish")
        .unwrap();
}

#[tokio::test]
async fn test_h2_pre_admission_reset_of_queued_upstream_stream_fills_cache() {
    assert_pre_admission_reset_of_queued_upstream_stream_fills_cache(
        QueuedStreamPaths {
            held: "/test_h2_upstream_stream_held_gated",
            reset: "/test_h2_reset_while_waiting_for_upstream_stream_gated",
            recovery: "/test_h2_upstream_connection_remains_usable_gated",
        },
        true,
    )
    .await;
    // Without the opt-in the proxy ignores the same reset for the fill just the same, and only
    // the abandonment callback is missing.
    assert_pre_admission_reset_of_queued_upstream_stream_fills_cache(
        QueuedStreamPaths {
            held: "/test_h2_upstream_stream_held_ungated",
            reset: "/test_h2_reset_while_waiting_for_upstream_stream_ungated",
            recovery: "/test_h2_upstream_connection_remains_usable_ungated",
        },
        false,
    )
    .await;
}
