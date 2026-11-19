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

//! A streaming miss whose cache storage fails a body write or the finish of the entry, with the
//! client connected.

use super::*;

/// A follow-up request on a new connection, answered by a new origin: nothing was cached.
async fn assert_not_cached(path: &str) {
    let origin = HttpOrigin::bind(|_request| async {
        Response::builder()
            .header(header::CONTENT_LENGTH, "9")
            .header(header::CACHE_CONTROL, "max-age=60")
            .body(Bytes::from_static(b"recovered"))
            .unwrap()
    })
    .await
    .unwrap();
    let (head, body) = fetch(path, origin.addr().port()).await;
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    assert_eq!(
        header_value(&head, "x-cache-status"),
        Some("miss"),
        "{head}"
    );
    assert_eq!(body, b"recovered");
    origin.shutdown().await;
}

/// A response that ends with its header, whose empty body write fails before the cached header
/// is served: the cache is disabled, and the request then fails because the cache it was to be
/// served from is gone.
#[tokio::test]
async fn h1_cache_write_failure_before_cached_header_fails_request() {
    init();
    let path = "/gate_off/h1_cache_write_failure_before_cached_header";
    let baseline = Baseline::new(path);
    let origin = HttpOrigin::bind(|_request| async {
        Response::builder()
            .header(header::CONTENT_LENGTH, "0")
            .header(header::CACHE_CONTROL, "max-age=60")
            .body(Bytes::new())
            .unwrap()
    })
    .await
    .unwrap();
    let mut client = connect().await;
    client
        .write_all(
            request(
                "GET",
                path,
                origin.addr().port(),
                "X-Cache-Fail-Write: true\r\nConnection: close\r\n",
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    let head = read_head(&mut client).await;
    assert!(head.starts_with("HTTP/1.1 500"), "{head}");
    assert_eq!(header_value(&head, "content-length"), Some("0"), "{head}");
    assert_eq!(read_rest(&mut client).await, b"");
    assert_eq!(
        baseline.wait_for_completions(1).await,
        Recorded {
            error: Some(ErrorType::InternalError),
            upstream_attempts: 1,
            ignored_downstream_errors: 0,
            disabled_cache: Some(NoCacheReason::StorageError),
        }
    );
    origin.shutdown().await;
    assert_not_cached(path).await;
}

/// A connected client that already has the cached header when the first body write fails: the
/// request fails with the storage error, the proxy drops the upstream connection and the client
/// gets nothing more before its connection is closed.
#[tokio::test]
async fn h1_cache_write_failure_after_cached_header_fails_request() {
    init();
    let path = "/gate_off/h1_cache_write_failure_after_cached_header";
    let baseline = Baseline::new(path);
    let mut origin = ScriptedOrigin::start().await;
    let mut client = connect().await;
    client
        .write_all(request("GET", path, origin.port, "X-Cache-Fail-Write: true\r\n").as_bytes())
        .await
        .unwrap();
    origin.wait_for_request().await;
    origin.send(CHUNKED_HEADER).await;
    let head = read_head(&mut client).await;
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    assert_eq!(
        header_value(&head, "x-cache-status"),
        Some("miss"),
        "{head}"
    );
    origin.send(HELLO_CHUNK).await;
    origin.wait_for_proxy_close().await;
    assert_eq!(
        baseline.wait_for_completions(1).await,
        Recorded {
            error: Some(ErrorType::FileWriteError),
            upstream_attempts: 1,
            ignored_downstream_errors: 0,
            disabled_cache: Some(NoCacheReason::StorageError),
        }
    );
    assert_eq!(read_rest(&mut client).await, b"");
    assert_not_cached(path).await;
}

/// A connected client that has the whole body of a fill whose finish then fails: the finish
/// disables the cache, the request fails because the cache it was served from is gone, and the
/// client's connection is closed before the last chunk of its response.
#[tokio::test]
async fn h1_cache_finish_failure_with_connected_client_fails_request() {
    init();
    let path = "/gate_off/h1_cache_finish_failure_with_connected_client";
    let baseline = Baseline::new(path);
    let mut origin = ScriptedOrigin::start().await;
    let mut client = connect().await;
    client
        .write_all(
            request(
                "GET",
                path,
                origin.port,
                "X-Cache-Fail-Finish-Streaming: true\r\n",
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    origin.wait_for_request().await;
    origin.send(CHUNKED_HEADER).await;
    let head = read_head(&mut client).await;
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    assert_eq!(
        header_value(&head, "x-cache-status"),
        Some("miss"),
        "{head}"
    );
    origin.send(HELLO_CHUNK).await;
    assert_eq!(read_chunk(&mut client).await, b"hello ");
    origin.send(b"6\r\nworld!\r\n").await;
    assert_eq!(read_chunk(&mut client).await, b"world!");
    origin.send(b"0\r\n\r\n").await;
    // The response is complete, so the proxy may close the upstream connection before or after
    // the origin does: either end is fine.
    origin.finish().await;
    assert_eq!(
        baseline.wait_for_completions(1).await,
        Recorded {
            error: Some(ErrorType::InternalError),
            upstream_attempts: 1,
            ignored_downstream_errors: 0,
            disabled_cache: Some(NoCacheReason::StorageError),
        }
    );
    assert_eq!(read_rest(&mut client).await, b"");
    assert_not_cached(path).await;
}
