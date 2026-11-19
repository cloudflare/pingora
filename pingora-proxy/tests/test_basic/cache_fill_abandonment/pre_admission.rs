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
use http::Method;

pub(super) struct PreAdmissionResetFixture {
    pub(super) upstream_port: u16,
    pub(super) send_response: tokio::sync::oneshot::Sender<()>,
    pub(super) response_sent: tokio::sync::oneshot::Receiver<()>,
    pub(super) send_body: tokio::sync::oneshot::Sender<()>,
    /// Makes the origin end its response, unless its stream was reset first.
    pub(super) finish_body: tokio::sync::oneshot::Sender<()>,
    pub(super) upstream_reset: tokio::sync::oneshot::Receiver<h2::Reason>,
    pub(super) abandoned_calls_before: usize,
    pub(super) attempts_before: usize,
    pub(super) completed_before: usize,
}

pub(super) struct PreAdmissionResetConfig {
    pub(super) path: &'static str,
    pub(super) method: Method,
    pub(super) cache_control: &'static str,
    pub(super) response_body: Option<Bytes>,
    pub(super) max_file_size: Option<usize>,
    pub(super) leave_predictor_bypass: bool,
    /// Use cache storage whose body writes fail.
    pub(super) fail_cache_write: bool,
    pub(super) handles_abandonment: bool,
}

impl PreAdmissionResetConfig {
    /// A POST for `path` whose origin answers with a cacheable header and no body, on storage
    /// without a size limit whose writes succeed.
    pub(super) fn post(path: &'static str, handles_abandonment: bool) -> Self {
        Self {
            path,
            method: Method::POST,
            cache_control: "max-age=30",
            response_body: None,
            max_file_size: None,
            leave_predictor_bypass: false,
            fail_cache_write: false,
            handles_abandonment,
        }
    }
}

pub(super) async fn start_pre_admission_reset(
    config: PreAdmissionResetConfig,
) -> PreAdmissionResetFixture {
    use std::future::poll_fn;

    init();

    let PreAdmissionResetConfig {
        path,
        method,
        cache_control,
        response_body,
        max_file_size,
        leave_predictor_bypass,
        fail_cache_write,
        handles_abandonment,
    } = config;

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_port = listener.local_addr().unwrap().port();
    let (upstream_ready_tx, upstream_ready_rx) = tokio::sync::oneshot::channel::<()>();
    let (send_response_tx, send_response_rx) = tokio::sync::oneshot::channel::<()>();
    let (response_sent_tx, response_sent_rx) = tokio::sync::oneshot::channel::<()>();
    let (send_body_tx, send_body_rx) = tokio::sync::oneshot::channel::<()>();
    let (finish_body_tx, finish_body_rx) = tokio::sync::oneshot::channel::<()>();
    let (upstream_reset_tx, upstream_reset_rx) = tokio::sync::oneshot::channel::<h2::Reason>();
    let (upstream_window_filled_tx, upstream_window_filled_rx) =
        tokio::sync::oneshot::channel::<()>();

    tokio::spawn(async move {
        let (io, _) = listener.accept().await.unwrap();
        let mut conn = h2::server::Builder::new()
            .initial_window_size((UPSTREAM_STREAM_WINDOW_BYTES + REQUEST_BODY_PREFIX.len()) as u32)
            .handshake::<_, Bytes>(io)
            .await
            .unwrap();
        let (req, mut send_response) = conn.accept().await.unwrap().unwrap();
        let mut request_body = req.into_body();
        let mut ping_pong = conn.ping_pong().unwrap();

        tokio::spawn(async move {
            while let Some(result) = conn.accept().await {
                if result.is_err() {
                    break;
                }
            }
        });
        ping_pong.ping(h2::Ping::opaque()).await.unwrap();
        upstream_ready_tx.send(()).unwrap();

        fill_upstream_window_without_releasing(
            &mut request_body,
            UPSTREAM_STREAM_WINDOW_BYTES + REQUEST_BODY_PREFIX.len(),
        )
        .await;
        upstream_window_filled_tx.send(()).unwrap();

        send_response_rx.await.unwrap();
        let response = http::Response::builder()
            .status(200)
            .header("cache-control", cache_control)
            .body(())
            .unwrap();
        let mut response_body_stream = send_response.send_response(response, false).unwrap();
        response_sent_tx.send(()).unwrap();

        if let Some(response_body) = response_body {
            send_body_rx.await.unwrap();
            response_body_stream
                .send_data(response_body, false)
                .unwrap();
        }

        let reset = tokio::select! {
            reason = poll_fn(|cx| response_body_stream.poll_reset(cx)) => Some(reason),
            Ok(()) = finish_body_rx => None,
        };
        let reason = match reset {
            Some(reason) => reason,
            None => {
                response_body_stream
                    .send_data(Bytes::from_static(b" end"), true)
                    .unwrap();
                // Hold the request body until the proxy drops the stream: releasing it after
                // the response ended would reset the stream, which the proxy could read before
                // the end of the response.
                poll_fn(|cx| response_body_stream.poll_reset(cx)).await
            }
        };
        let _ = upstream_reset_tx.send(reason.unwrap());
    });

    let mut client = h2c_client(6154).await;

    let uri = format!("http://127.0.0.1:6154{path}");
    let attempts_before = recorded_upstream_attempts(path);
    let completed_before = recorded_request_completions(path);
    let mut request = opt_in(Request::builder(), handles_abandonment)
        .method(method)
        .uri(uri)
        .header("x-h2", "true")
        .header("x-port", upstream_port.to_string())
        .header("x-record-request-lifecycle", "true");
    if leave_predictor_bypass {
        request = request.header("x-test-leave-predictor-bypass", "true");
    }
    if fail_cache_write {
        request = request.header("x-cache-fail-write", "true");
    }
    if let Some(max_file_size) = max_file_size {
        request = request.header("x-cache-max-file-size-bytes", max_file_size.to_string());
    }
    let (_response, mut request_body) = client
        .send_request(request.body(()).unwrap(), false)
        .unwrap();

    upstream_ready_rx.await.unwrap();
    request_body.reserve_capacity(REQUEST_BODY_PREFIX.len());
    let granted = tokio::time::timeout(TEST_TIMEOUT, poll_fn(|cx| request_body.poll_capacity(cx)))
        .await
        .expect("proxy never granted the initial request body capacity")
        .unwrap()
        .unwrap();
    assert!(granted >= REQUEST_BODY_PREFIX.len());
    request_body
        .send_data(Bytes::from_static(REQUEST_BODY_PREFIX), false)
        .unwrap();
    block_upstream_write_with_flow_control(&mut request_body, upstream_window_filled_rx).await;

    let abandoned_calls_before = downstream_abandoned_during_cache_fill_calls(path);
    let warn_calls_before = downstream_cache_warn_log_calls_for(path);
    request_body.send_reset(h2::Reason::CANCEL);
    wait_for_ignored_downstream_error(path, warn_calls_before).await;
    // Storage with streaming partial writes lets a fill outlive its client, so the proxy ignores
    // the reset either way. Only an opted-in request reports it, and at once.
    assert_eq!(
        downstream_abandoned_during_cache_fill_calls(path),
        abandoned_calls_before + usize::from(handles_abandonment),
        "the reset was not reported as an abandonment exactly when the request opted in"
    );

    PreAdmissionResetFixture {
        upstream_port,
        send_response: send_response_tx,
        response_sent: response_sent_rx,
        send_body: send_body_tx,
        finish_body: finish_body_tx,
        upstream_reset: upstream_reset_rx,
        abandoned_calls_before,
        attempts_before,
        completed_before,
    }
}

/// A POST whose client resets the stream while the proxy's upstream request body write is
/// blocked, before the upstream reveals whether its response is cacheable. Streaming partial
/// cache writes are supported, so the proxy ignores the reset, and the later cacheable response
/// still completes the fill.
async fn assert_downstream_rst_before_cache_admission_completes_fill(
    path: &'static str,
    handles_abandonment: bool,
) {
    let fixture = start_pre_admission_reset(PreAdmissionResetConfig {
        response_body: Some(Bytes::from_static(b"hello")),
        ..PreAdmissionResetConfig::post(path, handles_abandonment)
    })
    .await;
    // Only the response and its end are left: the fill runs without its client from here on.
    fixture.send_response.send(()).unwrap();
    fixture.send_body.send(()).unwrap();
    fixture.finish_body.send(()).unwrap();
    wait_for_recorded_request_completion(path, fixture.completed_before).await;
    assert_eq!(recorded_request_error(path), None);
    assert_eq!(
        recorded_upstream_attempts(path),
        fixture.attempts_before + 1
    );
    assert_eq!(
        downstream_abandoned_during_cache_fill_calls(path),
        fixture.abandoned_calls_before + usize::from(handles_abandonment),
        "cache admission or finishing the fill repeated the abandonment callback"
    );

    let body = wait_for_h2_cache_hit(
        &format!("http://127.0.0.1:6154{path}"),
        fixture.upstream_port,
    )
    .await;
    assert_eq!(body, b"hello end");
}

#[tokio::test]
async fn test_h2_downstream_rst_before_cache_admission() {
    assert_downstream_rst_before_cache_admission_completes_fill(
        "/test_h2_downstream_rst_before_cache_admission_gated",
        true,
    )
    .await;
    // Without the opt-in the same reset is ignored for the fill just the same, and only the
    // abandonment callback is missing.
    assert_downstream_rst_before_cache_admission_completes_fill(
        "/test_h2_downstream_rst_before_cache_admission_ungated",
        false,
    )
    .await;
}
