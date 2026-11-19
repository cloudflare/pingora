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

//! An HTTP/1 client of a streaming cache miss that closes, or half-closes, while the fill runs.

use super::*;

/// A client that closes after the response header and the start of the body does not stop the
/// fill: the error is ignored and the whole response is admitted.
#[tokio::test]
async fn h1_miss_client_close_mid_body_keeps_filling() {
    init();
    let path = "/gate_off/h1_miss_client_close_mid_body";
    let baseline = Baseline::new(path);
    let mut origin = ScriptedOrigin::start().await;
    let mut client = connect().await;
    client
        .write_all(request("GET", path, origin.port, "").as_bytes())
        .await
        .unwrap();
    origin.wait_for_request().await;
    origin.send(CHUNKED_HEADER).await;
    origin.send(HELLO_CHUNK).await;
    let head = read_head(&mut client).await;
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    assert_eq!(header_value(&head, "x-cache-status"), Some("miss"));
    assert_eq!(read_chunk(&mut client).await, b"hello ");
    drop(client);
    baseline.wait_for_ignored_errors(1).await;
    origin.send(WORLD_CHUNK_AND_END).await;
    let port = origin.port;
    assert_eq!(origin.finish().await, OriginEnd::Released);
    assert_eq!(
        baseline.wait_for_completions(1).await,
        FILLED_AFTER_IGNORED_ERROR
    );
    assert_cached(path, port, b"hello world!").await;
}

/// A conditional `GET` whose validator matches the response the fill is receiving gets a
/// complete `304` as soon as the header arrives. The fill itself is unconditional, and a client
/// that closes after the `304` is an ignored downstream error while the fill completes.
#[tokio::test]
async fn h1_not_modified_miss_client_close_after_complete_response_keeps_filling() {
    init();
    let path = "/gate_off/h1_not_modified_miss_client_close_after_complete_response";
    let baseline = Baseline::new(path);
    let mut origin = ScriptedOrigin::start().await;
    let mut client = connect().await;
    client
        .write_all(request("GET", path, origin.port, "If-None-Match: \"v1\"\r\n").as_bytes())
        .await
        .unwrap();
    let upstream_request = origin.wait_for_request().await;
    assert!(
        upstream_request.starts_with(&format!("GET {path} ")),
        "{upstream_request}"
    );
    assert!(
        !upstream_request
            .to_ascii_lowercase()
            .contains("if-none-match"),
        "{upstream_request}"
    );
    origin
        .send(
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nCache-Control: max-age=60\r\n\
              ETag: \"v1\"\r\n\r\n",
        )
        .await;
    let head = read_head(&mut client).await;
    assert!(head.starts_with("HTTP/1.1 304"), "{head}");
    assert_eq!(header_value(&head, "x-cache-status"), Some("miss"));
    drop(client);
    baseline.wait_for_ignored_errors(1).await;
    origin.send(HELLO_CHUNK).await;
    origin.send(WORLD_CHUNK_AND_END).await;
    let port = origin.port;
    assert_eq!(origin.finish().await, OriginEnd::Released);
    assert_eq!(
        baseline.wait_for_completions(1).await,
        FILLED_AFTER_IGNORED_ERROR
    );
    assert_cached(path, port, b"hello world!").await;
}

/// With `abort_on_close` on (the default), a client that half-closes while the origin is stalled
/// is an ignored downstream error: nothing more is written to it, the fill completes and the
/// connection is closed.
#[tokio::test]
async fn h1_miss_half_close_with_abort_on_close_stops_response() {
    init();
    let path = "/gate_off/h1_miss_half_close_with_abort_on_close";
    let baseline = Baseline::new(path);
    let mut origin = ScriptedOrigin::start().await;
    let mut client = connect().await;
    client
        .write_all(request("GET", path, origin.port, "").as_bytes())
        .await
        .unwrap();
    origin.wait_for_request().await;
    client.shutdown().await.unwrap();
    baseline.wait_for_ignored_errors(1).await;
    origin.send(FULL_CHUNKED_RESPONSE).await;
    let port = origin.port;
    assert_eq!(origin.finish().await, OriginEnd::Released);
    assert_eq!(
        baseline.wait_for_completions(1).await,
        FILLED_AFTER_IGNORED_ERROR
    );
    assert_eq!(read_rest(&mut client).await, b"");
    assert_cached(path, port, b"hello world!").await;
}

/// With `abort_on_close` off, a client that half-closes while the origin is stalled still gets
/// the whole response, and then the connection is closed.
#[tokio::test]
async fn h1_miss_half_close_without_abort_on_close_delivers_response() {
    init();
    let path = "/gate_off/h1_miss_half_close_without_abort_on_close";
    let baseline = Baseline::new(path);
    let mut origin = ScriptedOrigin::start().await;
    let mut client = connect().await;
    client
        .write_all(request("GET", path, origin.port, "X-Abort-On-Close: false\r\n").as_bytes())
        .await
        .unwrap();
    origin.wait_for_request().await;
    client.shutdown().await.unwrap();
    origin.send(FULL_CHUNKED_RESPONSE).await;
    let port = origin.port;
    assert_eq!(origin.finish().await, OriginEnd::Released);
    let head = read_head(&mut client).await;
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    assert_eq!(read_chunked_body(&mut client).await, b"hello world!");
    assert_eq!(read_rest(&mut client).await, b"");
    assert_eq!(
        baseline.wait_for_completions(1).await,
        Recorded {
            error: None,
            upstream_attempts: 1,
            ignored_downstream_errors: 0,
            disabled_cache: None,
        }
    );
    assert_cached(path, port, b"hello world!").await;
}
