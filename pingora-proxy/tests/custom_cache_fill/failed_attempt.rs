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

//! A failed custom upstream attempt of a request that opted in to downstream abandonment
//! handling is abandoned only once its downstream was abandoned or its fill was cancelled. Any
//! other failed attempt, before or after it wrote its request, is released exactly as without
//! the opt-in.

use super::*;
use std::sync::LazyLock;

/// Nothing is admitted into it: each attempt fails before its request is written.
static PRE_WRITE_FAILURE_CACHE: LazyLock<MemCache> = LazyLock::new(MemCache::new);

/// Nothing is admitted into it either: each fill fails before its response is complete.
static POST_WRITE_FAILURE_CACHE: LazyLock<MemCache> = LazyLock::new(MemCache::new);

async fn assert_pre_write_failure_does_not_abandon(failure: PreWriteFailure) {
    let observed = Arc::new(Observations {
        pre_write_failure: Some(failure),
        expected_error: Some(ErrorType::InternalError),
        ..Observations::default()
    });
    let upstream = CustomUpstream::new();
    let abandon_calls = Arc::clone(&upstream.abandon_calls);
    let request_headers_written = Arc::clone(&upstream.request_headers_written);
    let server = RunningProxy::start(
        CacheFillProxy {
            cache: &*PRE_WRITE_FAILURE_CACHE,
            observed: Arc::clone(&observed),
            handles_abandonment: true,
        },
        upstream.connector,
    );
    let mut client = within(
        "HTTP client did not connect",
        TcpStream::connect(server.address),
    )
    .await
    .unwrap();
    within(
        "HTTP request was not written",
        client.write_all(
            b"GET /custom-pre-write-error HTTP/1.1\r\nHost: example.com\r\nConnection: close\r\n\r\n",
        ),
    )
    .await
    .unwrap();

    let mut response_line = String::new();
    within(
        "HTTP error response did not arrive",
        BufReader::new(client).read_line(&mut response_line),
    )
    .await
    .unwrap();
    assert!(response_line.starts_with("HTTP/1.1 500"), "{response_line}");
    within(
        "failed request did not finish",
        observed.finished.notified(),
    )
    .await;
    assert_eq!(request_headers_written.load(Ordering::Relaxed), 0);
    assert!(
        abandon_calls.lock().unwrap().is_empty(),
        "a pre-write failure must not abandon a custom upstream session"
    );
    server.stop().await;
}

#[tokio::test]
async fn connection_hook_failure_does_not_abandon_custom_session() {
    assert_pre_write_failure_does_not_abandon(PreWriteFailure::ConnectedHook).await;
}

#[tokio::test]
async fn request_filter_failure_does_not_abandon_custom_session() {
    assert_pre_write_failure_does_not_abandon(PreWriteFailure::RequestFilter).await;
}

enum Client {
    /// Reads the response until the upstream fails.
    Connected,
    /// Leaves after the first body chunk, before the upstream fails.
    Leaves,
}

/// The upstream fails after writing the request and sending its first body chunk.
async fn assert_post_write_failure(path: &'static str, client_event: Client, opted_in: bool) {
    let observed = Arc::new(Observations {
        expected_error: Some(ErrorType::ReadError),
        ..Observations::default()
    });
    let upstream = CustomUpstream::failing_when_producer_drops();
    let abandon_calls = Arc::clone(&upstream.abandon_calls);
    let request_headers_written = Arc::clone(&upstream.request_headers_written);
    let releases = Arc::clone(&upstream.releases);
    let server = RunningProxy::start(
        CacheFillProxy {
            cache: &*POST_WRITE_FAILURE_CACHE,
            observed: Arc::clone(&observed),
            handles_abandonment: opted_in,
        },
        upstream.connector,
    );
    let (fail, failed) = oneshot::channel();
    let producer = tokio::spawn(upstream.producer.run_until_failure(failed));
    let mut client = within(
        "HTTP client did not connect",
        TcpStream::connect(server.address),
    )
    .await
    .unwrap();
    within(
        "HTTP request was not written",
        client.write_all(
            format!("GET {path} HTTP/1.1\r\nHost: example.com\r\nConnection: close\r\n\r\n")
                .as_bytes(),
        ),
    )
    .await
    .unwrap();
    let mut client = BufReader::new(client);
    within(
        "initial response was not delivered",
        read_initial_response(&mut client),
    )
    .await;
    assert_eq!(request_headers_written.load(Ordering::Relaxed), 1);

    let rest = match client_event {
        Client::Connected => {
            fail.send(()).unwrap();
            let mut rest = Vec::new();
            within(
                "the proxy did not close the failed response",
                client.read_to_end(&mut rest),
            )
            .await
            .unwrap();
            Some(rest)
        }
        Client::Leaves => {
            drop(client);
            if opted_in {
                within(
                    "downstream abandonment was not reported",
                    observed.abandoned.notified(),
                )
                .await;
            } else {
                within(
                    "the proxy did not ignore the downstream error for the cache fill",
                    observed.downstream_error_ignored.notified(),
                )
                .await;
            }
            fail.send(()).unwrap();
            None
        }
    };
    within("upstream producer did not stop", producer)
        .await
        .unwrap();
    within(
        "failed request did not finish",
        observed.finished.notified(),
    )
    .await;
    if let Some(rest) = rest {
        assert!(
            rest.is_empty(),
            "unexpected bytes after the failure: {rest:?}"
        );
    }
    let abandoned = opted_in && matches!(client_event, Client::Leaves);
    assert_eq!(
        observed.callback_calls.load(Ordering::Relaxed),
        usize::from(abandoned)
    );
    assert_eq!(
        *abandon_calls.lock().unwrap(),
        if abandoned {
            vec!["abandon(proxy attempt failed)".to_owned()]
        } else {
            Vec::new()
        }
    );
    assert_eq!(releases.load(Ordering::Relaxed), 1);
    server.stop().await;
}

#[tokio::test]
async fn connected_post_write_failure_releases_custom_session() {
    assert_post_write_failure(
        "/custom-post-write-error-connected",
        Client::Connected,
        true,
    )
    .await;
}

#[tokio::test]
async fn connected_post_write_failure_releases_custom_session_without_the_opt_in() {
    assert_post_write_failure(
        "/custom-post-write-error-connected-opted-out",
        Client::Connected,
        false,
    )
    .await;
}

// Once the proxy stopped delivering to the client, the failed attempt may have stopped reading
// mid-response, so it is abandoned rather than released as reusable.
#[tokio::test]
async fn abandoned_post_write_failure_abandons_custom_session() {
    assert_post_write_failure("/custom-post-write-error-abandoned", Client::Leaves, true).await;
}

#[tokio::test]
async fn post_write_failure_after_client_left_releases_custom_session_without_the_opt_in() {
    assert_post_write_failure(
        "/custom-post-write-error-left-opted-out",
        Client::Leaves,
        false,
    )
    .await;
}
