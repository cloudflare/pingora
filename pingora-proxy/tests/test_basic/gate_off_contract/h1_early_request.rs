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

//! An HTTP/1 client that sends its next request before the proxy is done with the current one.
//! Pipelining is off, so the proxy reads the first byte of that request as data sent after the
//! end of the request body.

use super::*;

/// Without a cache fill to protect, an early request fails the request: the proxy drops the
/// upstream connection and closes the client's connection without serving the next request.
async fn assert_early_next_request_fails_proxied_response(path: &str, bypass: bool) {
    init();
    let baseline = Baseline::new(path);
    let next = NextOrigin::start().await;
    let mut origin = ScriptedOrigin::start().await;
    let mut client = connect().await;
    client
        .write_all(
            request(
                "GET",
                path,
                origin.port,
                if bypass {
                    "X-Bypass-Cache: true\r\n"
                } else {
                    ""
                },
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    origin.wait_for_request().await;
    let header: &[u8] = if bypass {
        CHUNKED_HEADER
    } else {
        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nCache-Control: no-store\r\n\r\n"
    };
    origin.send(header).await;
    origin.send(HELLO_CHUNK).await;
    let head = read_head(&mut client).await;
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    assert_eq!(header_value(&head, "x-cache-status"), Some("no-cache"));
    assert_eq!(read_chunk(&mut client).await, b"hello ");
    client
        .write_all(next.next_request(path).as_bytes())
        .await
        .unwrap();
    origin.wait_for_proxy_close().await;
    assert_eq!(
        baseline.wait_for_completions(1).await,
        Recorded {
            error: Some(ErrorType::ConnectError),
            upstream_attempts: 1,
            ignored_downstream_errors: 0,
            disabled_cache: Some(if bypass {
                NoCacheReason::NeverEnabled
            } else {
                NoCacheReason::OriginNotCache
            }),
        }
    );
    assert_eq!(read_rest(&mut client).await, b"");
    assert_eq!(next.requests(), 0);
    next.shutdown().await;
}

#[tokio::test]
async fn h1_early_next_request_during_bypassed_response_fails_request() {
    assert_early_next_request_fails_proxied_response(
        "/gate_off/h1_early_next_request_during_bypassed_response",
        true,
    )
    .await;
}

#[tokio::test]
async fn h1_early_next_request_during_uncacheable_response_fails_request() {
    assert_early_next_request_fails_proxied_response(
        "/gate_off/h1_early_next_request_during_uncacheable_response",
        false,
    )
    .await;
}
