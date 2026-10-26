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

//! An HTTP/1 request body that fails to read while its client stays connected. A read timeout or
//! a malformed body is the proxy's verdict on a client that is still there, not the client
//! leaving, so the request is not an abandonment and the client still gets its response.

use super::*;

const CACHE_HOST: &str = "request-body-error.example";

/// How a connected client's request body fails to read.
#[derive(Clone, Copy)]
enum BodyReadFailure {
    /// The client sends 2 of its 10 body bytes and then nothing, past a 100 ms read timeout.
    TimedOut,
    /// The client sends one chunk, then a chunk-size line that is not hexadecimal.
    Malformed,
}

impl BodyReadFailure {
    /// The body framing header lines and the body bytes the client sends.
    fn framing_and_body(self) -> (&'static str, &'static [u8]) {
        match self {
            Self::TimedOut => (
                "X-Downstream-Read-Timeout-Ms: 100\r\nContent-Length: 10\r\n",
                b"up",
            ),
            Self::Malformed => ("Transfer-Encoding: chunked\r\n", b"2\r\nup\r\nzz\r\n"),
        }
    }
}

const RESPONSE_BODY: &[u8] = b"body";

/// An origin that answers the request's header, without waiting for its body, with an
/// uncacheable response once `respond` fires.
async fn uncacheable_origin(
    upstream: Upstream,
    request_seen: tokio::sync::oneshot::Sender<()>,
    respond: tokio::sync::oneshot::Receiver<()>,
) -> (u16, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let origin = tokio::spawn(async move {
        match upstream {
            Upstream::H1 => {
                let (mut socket, _) = tokio::time::timeout(TEST_TIMEOUT, listener.accept())
                    .await
                    .expect("the proxy did not connect to the origin")
                    .unwrap();
                read_http1_head(&mut socket).await;
                request_seen.send(()).unwrap();
                respond.await.unwrap();
                socket
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nCache-Control: no-store\r\n\r\n",
                    )
                    .await
                    .unwrap();
                socket.write_all(RESPONSE_BODY).await.unwrap();
                // Hold the connection until the proxy drops it.
                let _ = socket.read(&mut [0; 1024]).await;
            }
            Upstream::H2 => {
                let mut requests = accept_h2_connection(listener).await;
                let (_request, mut response) = next_h2_request(&mut requests).await;
                request_seen.send(()).unwrap();
                respond.await.unwrap();
                let header = Response::builder()
                    .status(StatusCode::OK)
                    .header(header::CONTENT_LENGTH, RESPONSE_BODY.len())
                    .header(header::CACHE_CONTROL, "no-store")
                    .body(())
                    .unwrap();
                let mut body = response.send_response(header, false).unwrap();
                body.send_data(Bytes::from_static(RESPONSE_BODY), true)
                    .unwrap();
                // Keep the streams until the proxy drops the connection.
                while requests.recv().await.is_some() {}
            }
        }
    });
    (port, origin)
}

/// A connected HTTP/1 client's request body fails to read during a cache miss, before the
/// response header. The proxy ignores the error to keep the fill going, as it always has, but
/// does not record an abandonment, and the uncacheable response the origin then sends still
/// reaches the client, whether or not the request handles downstream abandonment.
async fn assert_body_read_failure_keeps_response(
    path: &'static str,
    failure: BodyReadFailure,
    upstream: Upstream,
    handles_abandonment: bool,
) {
    init();

    let (request_seen_tx, request_seen_rx) = tokio::sync::oneshot::channel();
    let (respond_tx, respond_rx) = tokio::sync::oneshot::channel();
    let (upstream_port, origin) = uncacheable_origin(upstream, request_seen_tx, respond_rx).await;

    let abandoned_calls_before = downstream_abandoned_during_cache_fill_calls(path);
    let warn_calls_before = downstream_cache_warn_log_calls_for(path);
    let completed_before = recorded_request_completions(path);

    let (framing, body) = failure.framing_and_body();
    let mut client = TcpStream::connect("127.0.0.1:6148").await.unwrap();
    client
        .write_all(
            format!(
                "GET {path} HTTP/1.1\r\nHost: {CACHE_HOST}\r\n{}{}X-Port: {upstream_port}\r\n\
                 X-Record-Request-Lifecycle: true\r\n{framing}\r\n",
                h1_opt_in_line(handles_abandonment),
                upstream.header_line(),
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    client.write_all(body).await.unwrap();

    tokio::time::timeout(TEST_TIMEOUT, request_seen_rx)
        .await
        .expect("the origin did not get the request header")
        .unwrap();
    wait_for_ignored_downstream_error(path, warn_calls_before).await;
    assert_eq!(
        downstream_abandoned_during_cache_fill_calls(path),
        abandoned_calls_before,
        "a failed request body read from a connected client was reported as an abandonment"
    );

    respond_tx.send(()).unwrap();
    let mut response = Vec::new();
    tokio::time::timeout(TEST_TIMEOUT, client.read_to_end(&mut response))
        .await
        .unwrap_or_else(|_| {
            panic!(
                "the response did not end; received {:?}",
                String::from_utf8_lossy(&response)
            )
        })
        .unwrap();
    let response = String::from_utf8_lossy(&response);
    assert!(
        response.starts_with("HTTP/1.1 200 OK\r\n") && response.ends_with("\r\n\r\nbody"),
        "the connected client did not get its response: {response:?}"
    );

    wait_for_recorded_request_completion(path, completed_before).await;
    assert_eq!(recorded_request_error(path), None);
    assert_eq!(
        downstream_abandoned_during_cache_fill_calls(path),
        abandoned_calls_before
    );
    origin.abort();
}

#[tokio::test]
async fn test_h1_request_body_read_timeout_keeps_response() {
    assert_body_read_failure_keeps_response(
        "/test_h1_request_body_read_timeout_keeps_response",
        BodyReadFailure::TimedOut,
        Upstream::H1,
        true,
    )
    .await;
    // Without the opt-in the outcome is the same.
    assert_body_read_failure_keeps_response(
        "/test_h1_request_body_read_timeout_keeps_response_without_abandonment_handling",
        BodyReadFailure::TimedOut,
        Upstream::H1,
        false,
    )
    .await;
}

#[tokio::test]
async fn test_h1_malformed_request_body_keeps_h2_upstream_response() {
    assert_body_read_failure_keeps_response(
        "/test_h1_malformed_request_body_keeps_h2_upstream_response",
        BodyReadFailure::Malformed,
        Upstream::H2,
        true,
    )
    .await;
}
