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

/// The origin's cacheable response body, sent in [`BODY_CHUNK`]s.
const BODY_SIZE: usize = 8 * 1024 * 1024;
const BODY_CHUNK: Bytes = Bytes::from_static(&[b'x'; 16 * 1024]);

#[derive(Clone, Copy, PartialEq, Eq)]
enum DownstreamWritePath {
    Direct,
    ProxyTasks,
}

async fn connect_downstream_with_small_receive_buffer() -> TcpStream {
    let socket = tokio::net::TcpSocket::new_v4().unwrap();
    socket.set_recv_buffer_size(1024).unwrap();
    socket
        .connect("127.0.0.1:6148".parse().unwrap())
        .await
        .unwrap()
}

/// An HTTP/1 origin that sends the response header and the first [`BODY_CHUNK`] at once, then
/// signals `response_started`. It sends all but the last chunk once `send_rest` fires, which it
/// can only do while the proxy keeps reading, and the last chunk once `finish` fires.
async fn serve_h1_origin(
    listener: TcpListener,
    response_started: tokio::sync::oneshot::Sender<()>,
    send_rest: tokio::sync::oneshot::Receiver<()>,
    finish: tokio::sync::oneshot::Receiver<()>,
) {
    let (mut socket, _) = listener.accept().await.unwrap();
    read_http1_head(&mut socket).await;
    socket
        .write_all(
            format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {BODY_SIZE}\r\n\
                 Cache-Control: max-age=30\r\n\r\n"
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    socket.write_all(&BODY_CHUNK).await.unwrap();
    response_started.send(()).unwrap();
    send_rest.await.unwrap();
    for _ in 1..BODY_SIZE / BODY_CHUNK.len() - 1 {
        socket
            .write_all(&BODY_CHUNK)
            .await
            .expect("proxy stopped reading before the cache fill completed");
    }
    finish.await.unwrap();
    socket
        .write_all(&BODY_CHUNK)
        .await
        .expect("proxy stopped reading the final cache-fill chunk");
}

/// [`serve_h1_origin`] for an HTTP/2 upstream.
async fn serve_h2_origin(
    listener: TcpListener,
    response_started: tokio::sync::oneshot::Sender<()>,
    send_rest: tokio::sync::oneshot::Receiver<()>,
    finish: tokio::sync::oneshot::Receiver<()>,
) {
    use std::future::poll_fn;

    let (request, mut response) = accept_h2_request(listener).await;
    let request_body = request.into_body();
    let response_header = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_LENGTH, BODY_SIZE.to_string())
        .header(header::CACHE_CONTROL, "max-age=30")
        .body(())
        .unwrap();
    let mut response_body = response.send_response(response_header, false).unwrap();
    response_body.send_data(BODY_CHUNK, false).unwrap();
    response_started.send(()).unwrap();

    send_rest.await.unwrap();
    let mut remaining = BODY_SIZE - BODY_CHUNK.len() * 2;
    while remaining > 0 {
        let requested = remaining.min(BODY_CHUNK.len());
        response_body.reserve_capacity(requested);
        let available = poll_fn(|cx| response_body.poll_capacity(cx))
            .await
            .expect("proxy stopped accepting the H2 response body")
            .expect("H2 response stream failed");
        let chunk_len = requested.min(available);
        response_body
            .send_data(BODY_CHUNK.slice(..chunk_len), false)
            .unwrap();
        remaining -= chunk_len;
    }
    finish.await.unwrap();
    response_body.reserve_capacity(BODY_CHUNK.len());
    let available = poll_fn(|cx| response_body.poll_capacity(cx))
        .await
        .expect("proxy stopped accepting the final H2 response chunk")
        .expect("H2 response stream failed before its final chunk");
    assert!(available >= BODY_CHUNK.len());
    response_body.send_data(BODY_CHUNK, true).unwrap();
    drop(request_body);
}

/// A client stops reading its miss response body, so the proxy's downstream write times out
/// after `X-Write-Timeout-Ms`. The fill goes on and the response is served as a hit.
///
/// The client never sends more or closes before the fill completes, no other timeout is set on
/// the proxy and the origin keeps sending, so the downstream write timeout is the only error the
/// proxy can ignore for the fill.
async fn assert_downstream_write_timeout_completes_cache_fill(
    path: &'static str,
    upstream: Upstream,
    downstream_write_path: DownstreamWritePath,
    handles_abandonment: bool,
) {
    init();

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_port = listener.local_addr().unwrap().port();
    let (response_started_tx, response_started_rx) = tokio::sync::oneshot::channel();
    let (send_rest_tx, send_rest_rx) = tokio::sync::oneshot::channel();
    let (finish_tx, finish_rx) = tokio::sync::oneshot::channel();
    let origin = match upstream {
        Upstream::H1 => tokio::spawn(serve_h1_origin(
            listener,
            response_started_tx,
            send_rest_rx,
            finish_rx,
        )),
        Upstream::H2 => tokio::spawn(serve_h2_origin(
            listener,
            response_started_tx,
            send_rest_rx,
            finish_rx,
        )),
    };

    let reported = usize::from(handles_abandonment);
    let attempts_before = recorded_upstream_attempts(path);
    let completed_before = recorded_request_completions(path);
    let warn_calls_before = downstream_cache_warn_log_calls_for(path);
    let abandoned_calls_before = downstream_abandoned_during_cache_fill_calls(path);
    let pending_task_calls_before = downstream_abandoned_with_pending_tasks_calls(path);
    let mut downstream = connect_downstream_with_small_receive_buffer().await;
    let proxy_tasks_header = match downstream_write_path {
        DownstreamWritePath::Direct => "",
        DownstreamWritePath::ProxyTasks => "X-Proxy-Tasks-Enabled: true\r\n",
    };
    downstream
        .write_all(
            format!(
                "GET {path} HTTP/1.1\r\nHost: example.com\r\n{}{}X-Port: {upstream_port}\r\n\
                 X-Write-Timeout-Ms: 1000\r\nX-Record-Request-Lifecycle: true\r\n\
                 {proxy_tasks_header}\r\n",
                h1_opt_in_line(handles_abandonment),
                upstream.header_line(),
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    tokio::time::timeout(TEST_TIMEOUT, response_started_rx)
        .await
        .expect("the origin did not send the first response chunk")
        .unwrap();
    let response_head = read_http1_head(&mut downstream).await;
    assert!(
        response_head.lines().next().unwrap().contains(" 200 "),
        "{response_head}"
    );
    assert!(
        response_head
            .to_ascii_lowercase()
            .contains("x-cache-status: miss"),
        "{response_head}"
    );
    send_rest_tx.send(()).unwrap();

    wait_for_ignored_downstream_error(path, warn_calls_before).await;
    assert_eq!(
        downstream_abandoned_during_cache_fill_calls(path),
        abandoned_calls_before + reported,
        "one downstream request must produce one cache-fill callback"
    );
    assert_eq!(
        downstream_abandoned_with_pending_tasks_calls(path),
        pending_task_calls_before
            + reported * usize::from(downstream_write_path == DownstreamWritePath::ProxyTasks),
        "the callback did not run from the selected downstream write path"
    );
    drop(downstream);
    finish_tx.send(()).unwrap();

    tokio::time::timeout(TEST_TIMEOUT, origin)
        .await
        .expect("cache fill did not finish")
        .unwrap();
    wait_for_recorded_request_completion(path, completed_before).await;
    assert_eq!(recorded_request_error(path), None);
    assert_eq!(recorded_upstream_attempts(path), attempts_before + 1);
    assert_eq!(
        downstream_cache_warn_log_calls_for(path),
        warn_calls_before + 1
    );

    let body = wait_for_h2_cache_hit(&format!("http://example.com{path}"), upstream_port).await;
    assert_eq!(body.len(), BODY_SIZE);
    assert!(body.iter().all(|byte| *byte == b'x'));
    assert_eq!(
        downstream_abandoned_during_cache_fill_calls(path),
        abandoned_calls_before + reported,
        "finishing the cache fill must not repeat the callback"
    );
}

#[tokio::test]
async fn test_h1_downstream_write_timeout_during_cache_fill_notifies_application() {
    bounded(assert_downstream_write_timeout_completes_cache_fill(
        "/test_h1_downstream_write_timeout_during_cache_fill",
        Upstream::H1,
        DownstreamWritePath::Direct,
        true,
    ))
    .await;
    // Without the opt-in the write timeout is ignored for the fill but not reported.
    bounded(assert_downstream_write_timeout_completes_cache_fill(
        "/test_h1_downstream_write_timeout_during_cache_fill_ungated",
        Upstream::H1,
        DownstreamWritePath::Direct,
        false,
    ))
    .await;
}

#[tokio::test]
async fn test_h2_upstream_proxy_tasks_downstream_write_timeout_completes_cache_fill() {
    bounded(assert_downstream_write_timeout_completes_cache_fill(
        "/test_h2_upstream_proxy_tasks_downstream_write_timeout",
        Upstream::H2,
        DownstreamWritePath::ProxyTasks,
        true,
    ))
    .await;
}
