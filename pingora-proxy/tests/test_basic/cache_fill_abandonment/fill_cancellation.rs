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

//! An application cancels an abandoned cache fill instead of waiting for the upstream.
//!
//! Every origin here sends the response header and a first body chunk, then stalls until the
//! test releases it. A cancelled fill must therefore end while the origin is still silent.
//!
//! A storage can also cancel a fill itself, from `get_miss_handler`, while the writer still holds
//! the cache lock. The writer's client is then served uncached and the lock passes to a waiter.

use super::*;
use crate::utils::server_utils::{
    cache_fill_cancellation_body_bytes, cache_fill_cancellation_handlers,
    cache_fill_cancellation_handlers_dropped_unfinished, cache_fill_cancellation_handlers_finished,
    cache_fill_cancellation_requested, cancel_cache_fill, recorded_cache_lookups,
    recorded_cache_reason, recorded_fail_to_proxy_code, reset_cancellation,
};
use pingora_cache::fill_cancellation::ERR_CACHE_FILL_CANCELLED;
use pingora_cache::fill_cancellation::REASON_CACHE_FILL_CANCELLED;
use pingora_cache::NoCacheReason;
use std::future::poll_fn;
use std::time::{Duration, Instant};

const FIRST_CHUNK: &[u8] = b"hello ";
const LAST_CHUNK: &[u8] = b"world!";
const CACHE_HOST: &str = "fill-cancellation.example";

#[derive(Clone, Copy)]
enum CancellationTiming {
    /// `downstream_abandoned_during_cache_fill` requests the cancellation.
    InHook,
    /// The test requests the cancellation after the callback ran.
    AfterAbandonment,
}

impl CancellationTiming {
    fn request_header(self) -> &'static str {
        match self {
            Self::InHook => "X-Cancel-Fill-On-Abandon: true\r\n",
            Self::AfterAbandonment => "",
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum OriginOutcome {
    /// The proxy closed its upstream connection before the test released the origin.
    ProxyClosed,
    Released,
}

/// Frame one chunk of a chunked transfer encoding.
fn h1_chunk(data: &[u8]) -> Vec<u8> {
    let mut chunk = format!("{:x}\r\n", data.len()).into_bytes();
    chunk.extend_from_slice(data);
    chunk.extend_from_slice(b"\r\n");
    chunk
}

/// An origin that sends the response head and [`FIRST_CHUNK`], then nothing until released.
///
/// The body is chunked because the proxy buffers content-length bodies until they complete,
/// which would hide the first chunk from the client while the origin stalls.
struct StalledH1Origin {
    port: u16,
    request_seen: tokio::sync::oneshot::Receiver<()>,
    send_response: tokio::sync::oneshot::Sender<()>,
    release: tokio::sync::oneshot::Sender<()>,
    task: tokio::task::JoinHandle<OriginOutcome>,
}

impl StalledH1Origin {
    async fn start(cache_control: &'static str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (request_seen_tx, request_seen) = tokio::sync::oneshot::channel();
        let (send_response, send_response_rx) = tokio::sync::oneshot::channel::<()>();
        let (release, release_rx) = tokio::sync::oneshot::channel::<()>();
        let task = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            read_h1_head(&mut socket).await;
            request_seen_tx.send(()).unwrap();
            send_response_rx.await.unwrap();
            socket
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\
                         Cache-Control: {cache_control}\r\n\r\n"
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
            socket.write_all(&h1_chunk(FIRST_CHUNK)).await.unwrap();

            let mut probe = [0u8; 1];
            tokio::select! {
                // The proxy never sends more request bytes, so any read result means it closed.
                _ = socket.read(&mut probe) => OriginOutcome::ProxyClosed,
                released = release_rx => {
                    released.unwrap();
                    let mut rest = h1_chunk(LAST_CHUNK);
                    rest.extend_from_slice(b"0\r\n\r\n");
                    socket.write_all(&rest).await.unwrap();
                    OriginOutcome::Released
                }
            }
        });
        Self {
            port,
            request_seen,
            send_response,
            release,
            task,
        }
    }
}

/// Reads a chunked H1 response incrementally, keeping bytes that arrive ahead of a read.
struct ChunkedResponseReader {
    stream: TcpStream,
    received: Vec<u8>,
}

impl ChunkedResponseReader {
    fn new(stream: TcpStream) -> Self {
        Self {
            stream,
            received: Vec::new(),
        }
    }

    async fn read_more(&mut self) {
        let mut buf = [0u8; 1024];
        let read = self.stream.read(&mut buf).await.unwrap();
        assert_ne!(
            0, read,
            "connection closed before the expected response bytes"
        );
        self.received.extend_from_slice(&buf[..read]);
    }

    /// Read through the end of the head and return it.
    async fn read_head(&mut self) -> String {
        let head_end = loop {
            if let Some(position) = self.received.windows(4).position(|w| w == b"\r\n\r\n") {
                break position + 4;
            }
            self.read_more().await;
        };
        let head = String::from_utf8_lossy(&self.received[..head_end]).into_owned();
        self.received.drain(..head_end);
        head
    }

    /// Decode whole chunks until exactly `len` body bytes were collected.
    async fn read_body(&mut self, len: usize) -> Vec<u8> {
        let mut body = Vec::with_capacity(len);
        while body.len() < len {
            let size_line_end = loop {
                if let Some(position) = self.received.windows(2).position(|w| w == b"\r\n") {
                    break position;
                }
                self.read_more().await;
            };
            let size = usize::from_str_radix(
                std::str::from_utf8(&self.received[..size_line_end]).unwrap(),
                16,
            )
            .unwrap();
            assert!(
                size > 0 && body.len() + size <= len,
                "unexpected chunk of {size} bytes after {} of {len} body bytes",
                body.len()
            );
            let chunk_end = size_line_end + 2 + size + 2;
            while self.received.len() < chunk_end {
                self.read_more().await;
            }
            body.extend_from_slice(&self.received[size_line_end + 2..chunk_end - 2]);
            assert_eq!(&self.received[chunk_end - 2..chunk_end], b"\r\n");
            self.received.drain(..chunk_end);
        }
        body
    }
}

async fn wait_for_cache_fill_cancellation_handlers(path: &str, expected: usize) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while cache_fill_cancellation_handlers(path) < expected {
        assert!(
            Instant::now() < deadline,
            "cache admission did not create a cancellation-aware miss handler for {path}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(cache_fill_cancellation_handlers(path), expected);
}

/// The lock is free and the partial fill was discarded: the next request is a miss that
/// completes, and the one after it is a hit.
async fn assert_fill_recovers(path: &str) {
    let origin = HttpOrigin::bind(|_request| async {
        Response::builder()
            .header(header::CONTENT_LENGTH, "9")
            .header(header::CACHE_CONTROL, "max-age=30")
            .body(Bytes::from_static(b"recovered"))
            .unwrap()
    })
    .await
    .unwrap();
    let client = reqwest::Client::new();
    for expected_status in ["miss", "hit"] {
        let response = client
            .get(format!("http://127.0.0.1:6148{path}"))
            .header(header::HOST, CACHE_HOST)
            .header("x-port", origin.addr().port().to_string())
            .header("x-lock", "true")
            .header("x-cache-lock-wait-timeout-ms", "10000")
            .timeout(Duration::from_secs(5))
            .send()
            .await
            .expect("request after a cancelled fill remained blocked on the cache lock");
        assert_eq!(response.headers()["x-cache-status"], expected_status);
        assert_eq!(response.bytes().await.unwrap(), b"recovered".as_slice());
    }
    origin.shutdown().await;
}

struct Counters {
    attempts: usize,
    completed: usize,
    abandoned: usize,
    forced_retry_calls: usize,
    nonretryable_hook_calls: usize,
    handlers: usize,
    body_bytes: usize,
    finished: usize,
    dropped_unfinished: usize,
}

impl Counters {
    fn before(path: &str) -> Self {
        Self {
            attempts: recorded_upstream_attempts(path),
            completed: recorded_request_completions(path),
            abandoned: downstream_abandoned_during_cache_fill_calls(path),
            forced_retry_calls: forced_retry_error_hook_calls(path),
            nonretryable_hook_calls: forced_retry_error_hook_calls_with_retry_disabled(path),
            handlers: cache_fill_cancellation_handlers(path),
            body_bytes: cache_fill_cancellation_body_bytes(path),
            finished: cache_fill_cancellation_handlers_finished(path),
            dropped_unfinished: cache_fill_cancellation_handlers_dropped_unfinished(path),
        }
    }

    /// The request ended through the cancellation: once, without retry, with the writer discarded.
    fn assert_cancelled(&self, path: &str, body_bytes_written: usize) {
        assert_eq!(recorded_request_error(path), Some(ERR_CACHE_FILL_CANCELLED));
        assert_eq!(
            recorded_cache_reason(path),
            Some(NoCacheReason::Custom(REASON_CACHE_FILL_CANCELLED)),
            "an intentional stop must not be reported as a storage failure"
        );
        assert_eq!(
            recorded_fail_to_proxy_code(path),
            Some(0),
            "a cancelled fill's downstream is gone, so no status is reportable"
        );
        assert_eq!(
            recorded_upstream_attempts(path),
            self.attempts + 1,
            "the cancelled fill was retried"
        );
        assert_eq!(
            forced_retry_error_hook_calls(path),
            self.forced_retry_calls + 1
        );
        assert_eq!(
            forced_retry_error_hook_calls_with_retry_disabled(path),
            self.nonretryable_hook_calls + 1,
            "the cancellation error reached the retry hook as retryable"
        );
        assert_eq!(
            downstream_abandoned_during_cache_fill_calls(path),
            self.abandoned + 1
        );
        assert_eq!(cache_fill_cancellation_handlers(path), self.handlers + 1);
        assert_eq!(
            cache_fill_cancellation_body_bytes(path),
            self.body_bytes + body_bytes_written
        );
        assert_eq!(
            cache_fill_cancellation_handlers_finished(path),
            self.finished,
            "a cancelled fill must not finish its admission"
        );
        assert_eq!(
            cache_fill_cancellation_handlers_dropped_unfinished(path),
            self.dropped_unfinished + 1,
            "the cancelled fill did not release its miss handler"
        );
    }

    /// The request ignored the client's departure and the cancellation, as without the opt-in:
    /// the hook never ran, one attempt succeeded and the whole body was admitted.
    fn assert_filled_without_abandonment(&self, path: &str) {
        assert_eq!(
            downstream_abandoned_during_cache_fill_calls(path),
            self.abandoned
        );
        assert_eq!(recorded_request_error(path), None);
        assert_eq!(recorded_upstream_attempts(path), self.attempts + 1);
        assert_eq!(cache_fill_cancellation_handlers(path), self.handlers + 1);
        assert_eq!(
            cache_fill_cancellation_body_bytes(path),
            self.body_bytes + FIRST_CHUNK.len() + LAST_CHUNK.len()
        );
        assert_eq!(
            cache_fill_cancellation_handlers_finished(path),
            self.finished + 1,
            "a fill that did not opt in must run to completion"
        );
        assert_eq!(
            cache_fill_cancellation_handlers_dropped_unfinished(path),
            self.dropped_unfinished
        );
    }
}

/// A follow-up request for `path` is a hit that serves `expected`.
async fn assert_cache_hit(path: &str, port: u16, expected: &[u8]) {
    let cached = reqwest::Client::new()
        .get(format!("http://127.0.0.1:6148{path}"))
        .header(header::HOST, CACHE_HOST)
        .header("x-port", port.to_string())
        .timeout(Duration::from_secs(5))
        .send()
        .await
        .unwrap();
    assert_eq!(cached.headers()["x-cache-status"], "hit");
    assert_eq!(cached.bytes().await.unwrap(), expected);
}

/// Wait until the proxy reported the client's departure for `path` as an ignored downstream
/// error, which it does with or without the opt-in.
async fn wait_for_client_departure(path: &str, reported_before: usize) {
    wait_until("the proxy did not observe the client leaving", || {
        downstream_cache_warn_log_calls_for(path) > reported_before
    })
    .await;
}

/// Cache a fresh `seed` body for `path` through the default storage.
async fn seed_cache(path: &str) {
    let seed_origin = HttpOrigin::bind(|_request| async {
        Response::builder()
            .header(header::CONTENT_LENGTH, "4")
            .header(header::CACHE_CONTROL, "max-age=30")
            .body(Bytes::from_static(b"seed"))
            .unwrap()
    })
    .await
    .unwrap();
    let seeded = reqwest::Client::new()
        .get(format!("http://127.0.0.1:6148{path}"))
        .header(header::HOST, CACHE_HOST)
        .header("x-port", seed_origin.addr().port().to_string())
        .timeout(Duration::from_secs(5))
        .send()
        .await
        .unwrap();
    assert_eq!(seeded.headers()["x-cache-status"], "miss");
    assert_eq!(seeded.bytes().await.unwrap(), b"seed".as_slice());
    seed_origin.shutdown().await;
}

async fn assert_h1_cancel_ends_stalled_fill(timing: CancellationTiming) {
    init();

    let path = match timing {
        CancellationTiming::InHook => "/test_h1_fill_cancel_in_hook_ends_stalled_fill",
        CancellationTiming::AfterAbandonment => {
            "/test_h1_fill_cancel_after_abandonment_ends_stalled_fill"
        }
    };
    let origin = StalledH1Origin::start("max-age=30").await;
    let before = Counters::before(path);

    let mut downstream = TcpStream::connect("127.0.0.1:6148").await.unwrap();
    downstream
        .write_all(
            format!(
                "GET {path} HTTP/1.1\r\nHost: {CACHE_HOST}\r\nX-Port: {}\r\n\
                 X-Lock: true\r\nX-Cache-Lock-Wait-Timeout-Ms: 10000\r\n\
                 X-Record-Request-Lifecycle: true\r\nX-Cache-Fill-Cancellation-Watch: true\r\nX-Handle-Downstream-Abandonment: true\r\n\
                 X-Force-Retry-In-Error-Hook: true\r\n{}\r\n",
                origin.port,
                timing.request_header()
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    origin.request_seen.await.unwrap();
    origin.send_response.send(()).unwrap();

    let mut downstream = ChunkedResponseReader::new(downstream);
    let response_head = downstream.read_head().await;
    let first_chunk = downstream.read_body(FIRST_CHUNK.len()).await;
    assert!(
        response_head.lines().next().unwrap().contains(" 200 "),
        "{response_head}"
    );
    assert!(
        response_head.contains("x-cache-status: miss"),
        "{response_head}"
    );
    assert_eq!(first_chunk, FIRST_CHUNK);
    wait_for_cache_fill_cancellation_handlers(path, before.handlers + 1).await;

    drop(downstream);
    wait_for_cache_fill_abandonment(path, before.abandoned).await;
    if matches!(timing, CancellationTiming::AfterAbandonment) {
        assert_eq!(
            recorded_request_completions(path),
            before.completed,
            "the fill ended before anyone cancelled it"
        );
        assert!(!cache_fill_cancellation_requested(path));
        assert!(cancel_cache_fill(path));
    }

    // The origin is still stalled, so only the cancellation can end the request.
    wait_for_recorded_request_completion(path, before.completed).await;
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), origin.task)
            .await
            .expect("the proxy kept the stalled upstream connection open")
            .unwrap(),
        OriginOutcome::ProxyClosed
    );
    assert!(cache_fill_cancellation_requested(path));
    before.assert_cancelled(path, FIRST_CHUNK.len());
    drop(origin.release);

    assert_fill_recovers(path).await;
}

// The callback cancels; the fill ends without another upstream chunk.
#[tokio::test]
async fn test_h1_fill_cancel_in_hook_ends_stalled_fill() {
    tokio::time::timeout(
        Duration::from_secs(30),
        assert_h1_cancel_ends_stalled_fill(CancellationTiming::InHook),
    )
    .await
    .expect("cancelled H1 cache fill did not finish");
}

// A cancellation requested after the callback wakes the waiting fill.
#[tokio::test]
async fn test_h1_fill_cancel_after_abandonment_ends_stalled_fill() {
    tokio::time::timeout(
        Duration::from_secs(30),
        assert_h1_cancel_ends_stalled_fill(CancellationTiming::AfterAbandonment),
    )
    .await
    .expect("cancelled H1 cache fill did not finish");
}

// A cancellation requested before the origin responded takes effect as soon as the writer exists.
async fn assert_h1_cancel_before_admission_ends_fill_at_admission() {
    init();

    const PATH: &str = "/test_h1_fill_cancel_before_admission_ends_fill_at_admission";
    let origin = StalledH1Origin::start("max-age=30").await;
    let before = Counters::before(PATH);

    let mut downstream = TcpStream::connect("127.0.0.1:6148").await.unwrap();
    downstream
        .write_all(
            format!(
                "GET {PATH} HTTP/1.1\r\nHost: {CACHE_HOST}\r\nX-Port: {}\r\n\
                 X-Lock: true\r\nX-Cache-Lock-Wait-Timeout-Ms: 10000\r\n\
                 X-Record-Request-Lifecycle: true\r\nX-Cache-Fill-Cancellation-Watch: true\r\nX-Handle-Downstream-Abandonment: true\r\n\
                 X-Force-Retry-In-Error-Hook: true\r\nX-Cancel-Fill-On-Abandon: true\r\n\r\n",
                origin.port,
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    origin.request_seen.await.unwrap();
    drop(downstream);
    wait_for_cache_fill_abandonment(PATH, before.abandoned).await;
    assert!(
        cache_fill_cancellation_requested(PATH),
        "the callback did not cancel before admission"
    );
    assert_eq!(
        cache_fill_cancellation_handlers(PATH),
        before.handlers,
        "a writer existed before the origin responded"
    );

    origin.send_response.send(()).unwrap();
    wait_for_recorded_request_completion(PATH, before.completed).await;
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), origin.task)
            .await
            .expect("the proxy kept the stalled upstream connection open")
            .unwrap(),
        OriginOutcome::ProxyClosed
    );
    // The first chunk can share the upstream task batch that admits the response, so the
    // writer may or may not have seen it; what matters is that it was dropped unfinished.
    assert_eq!(recorded_request_error(PATH), Some(ERR_CACHE_FILL_CANCELLED));
    assert_eq!(recorded_upstream_attempts(PATH), before.attempts + 1);
    assert_eq!(cache_fill_cancellation_handlers(PATH), before.handlers + 1);
    assert_eq!(
        cache_fill_cancellation_handlers_finished(PATH),
        before.finished
    );
    assert_eq!(
        cache_fill_cancellation_handlers_dropped_unfinished(PATH),
        before.dropped_unfinished + 1
    );
    assert_eq!(
        forced_retry_error_hook_calls_with_retry_disabled(PATH),
        before.nonretryable_hook_calls + 1
    );
    drop(origin.release);

    assert_fill_recovers(PATH).await;
}

#[tokio::test]
async fn test_h1_fill_cancel_before_admission_ends_fill_at_admission() {
    tokio::time::timeout(
        Duration::from_secs(30),
        assert_h1_cancel_before_admission_ends_fill_at_admission(),
    )
    .await
    .expect("cache fill cancelled before admission did not finish");
}

/// Without the opt-in, a client that leaves before the origin responds is ignored for the fill:
/// the hook that would cancel does not run, a cancellation requested before admission is ignored
/// once the writer exists, and the fill completes, as before cancellation existed.
async fn assert_h1_cancel_before_admission_ignored_without_abandonment_handling() {
    init();

    const PATH: &str = "/test_h1_fill_cancel_before_admission_ignored_without_abandonment_handling";
    let origin = StalledH1Origin::start("max-age=30").await;
    let before = Counters::before(PATH);
    let reported_before = downstream_cache_warn_log_calls_for(PATH);

    let mut downstream = TcpStream::connect("127.0.0.1:6148").await.unwrap();
    downstream
        .write_all(
            format!(
                "GET {PATH} HTTP/1.1\r\nHost: {CACHE_HOST}\r\nX-Port: {}\r\n\
                 X-Lock: true\r\nX-Cache-Lock-Wait-Timeout-Ms: 10000\r\n\
                 X-Record-Request-Lifecycle: true\r\nX-Cache-Fill-Cancellation-Watch: true\r\n\
                 X-Force-Retry-In-Error-Hook: true\r\nX-Cancel-Fill-On-Abandon: true\r\n\r\n",
                origin.port,
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    origin.request_seen.await.unwrap();
    drop(downstream);
    wait_for_client_departure(PATH, reported_before).await;
    assert!(
        cancel_cache_fill(PATH),
        "the abandonment hook ran for a request that did not opt in"
    );
    assert_eq!(
        cache_fill_cancellation_handlers(PATH),
        before.handlers,
        "a writer existed before the origin responded"
    );

    origin.send_response.send(()).unwrap();
    wait_for_cache_fill_cancellation_handlers(PATH, before.handlers + 1).await;
    origin.release.send(()).unwrap();
    assert_eq!(origin.task.await.unwrap(), OriginOutcome::Released);
    wait_for_recorded_request_completion(PATH, before.completed).await;
    before.assert_filled_without_abandonment(PATH);
    assert_cache_hit(PATH, origin.port, &[FIRST_CHUNK, LAST_CHUNK].concat()).await;
}

#[tokio::test]
async fn test_h1_fill_cancel_before_admission_ignored_without_abandonment_handling() {
    tokio::time::timeout(
        Duration::from_secs(30),
        assert_h1_cancel_before_admission_ignored_without_abandonment_handling(),
    )
    .await
    .expect("fill without abandonment handling did not finish");
}

/// Selects the test storage that fails the first miss handler for each cache key with
/// [`ERR_CACHE_FILL_CANCELLED`], like a storage cancelling a fill before admission.
const CANCEL_FIRST_MISS_HANDLER: &str = "x-cache-cancel-first-miss-handler";

/// A storage cancels the first fill of `path` from `get_miss_handler` while its writer holds the
/// cache lock and a second request waits on that lock. The writer's client is still served,
/// uncached, and the lock passes to the waiter as for a storage error: the waiter becomes the
/// next writer and fills the cache, and a third request is a hit. The writer's no-cache reason is
/// the cancellation only if `handles_abandonment`.
async fn assert_h1_storage_cancelled_fill_hands_lock_to_waiter(
    path: &'static str,
    handles_abandonment: bool,
) {
    init();

    const REFILLED: &[u8] = b"refilled";
    let origin = StalledH1Origin::start("max-age=30").await;
    let refill_origin = HttpOrigin::bind(|_request| async {
        Response::builder()
            .header(header::CONTENT_LENGTH, REFILLED.len())
            .header(header::CACHE_CONTROL, "max-age=30")
            .body(Bytes::from_static(REFILLED))
            .unwrap()
    })
    .await
    .unwrap();
    let refill_port = refill_origin.addr().port();
    let lookups = recorded_cache_lookups(path);
    let completed = recorded_request_completions(path);

    let mut writer = TcpStream::connect("127.0.0.1:6148").await.unwrap();
    writer
        .write_all(
            format!(
                "GET {path} HTTP/1.1\r\nHost: {CACHE_HOST}\r\nX-Port: {}\r\n\
                 X-Lock: true\r\nX-Cache-Lock-Wait-Timeout-Ms: 10000\r\n\
                 X-Record-Request-Lifecycle: true\r\n{CANCEL_FIRST_MISS_HANDLER}: true\r\n{}\r\n",
                origin.port,
                h1_opt_in_line(handles_abandonment)
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    origin.request_seen.await.unwrap();

    // The writer's origin holds back its response header, so the writer still holds the lock.
    let waiter = tokio::spawn(async move {
        let response = client_opting_in(handles_abandonment)
            .get(format!("http://127.0.0.1:6148{path}"))
            .header(header::HOST, CACHE_HOST)
            .header("x-port", refill_port.to_string())
            .header("x-lock", "true")
            .header("x-cache-lock-wait-timeout-ms", "10000")
            .header("x-record-request-lifecycle", "true")
            .header(CANCEL_FIRST_MISS_HANDLER, "true")
            .timeout(TEST_TIMEOUT)
            .send()
            .await
            .expect("the request waiting on the cache lock failed");
        let headers = response.headers().clone();
        (headers, response.bytes().await.unwrap())
    });
    wait_until("the second request did not reach its cache lookup", || {
        recorded_cache_lookups(path) == lookups + 2
    })
    .await;

    // Admitting the response header asks storage for the miss handler, which it refuses.
    origin.send_response.send(()).unwrap();
    let mut writer = ChunkedResponseReader::new(writer);
    let head = writer.read_head().await;
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    assert!(head.contains("x-cache-status: no-cache"), "{head}");
    assert_eq!(writer.read_body(FIRST_CHUNK.len()).await, FIRST_CHUNK);

    let (waiter_headers, waiter_body) = tokio::time::timeout(TEST_TIMEOUT, waiter)
        .await
        .expect("the request waiting on the cache lock did not finish")
        .unwrap();
    assert_eq!(
        waiter_headers["x-cache-status"], "miss",
        "the request waiting on the cache lock did not become the next writer"
    );
    assert!(
        waiter_headers.contains_key("x-cache-lock-time-ms"),
        "the second request did not wait on the cache lock"
    );
    assert_eq!(waiter_body, REFILLED);
    // The writer is still waiting for the rest of its response, so this is the waiter.
    wait_for_recorded_request_completion(path, completed).await;

    origin.release.send(()).unwrap();
    assert_eq!(origin.task.await.unwrap(), OriginOutcome::Released);
    assert_eq!(writer.read_body(LAST_CHUNK.len()).await, LAST_CHUNK);
    wait_for_recorded_request_completion(path, completed + 1).await;
    assert_eq!(recorded_request_error(path), None);
    let expected_reason = if handles_abandonment {
        NoCacheReason::Custom(REASON_CACHE_FILL_CANCELLED)
    } else {
        NoCacheReason::StorageError
    };
    assert_eq!(recorded_cache_reason(path), Some(expected_reason));

    assert_cache_hit(path, refill_port, REFILLED).await;
    refill_origin.shutdown().await;
}

/// With the opt-in, the storage's cancellation disables the writer's cache as a cancelled fill,
/// and the lock still passes to the waiter instead of sending every waiter upstream uncached.
#[tokio::test]
async fn test_h1_storage_cancelled_fill_hands_lock_to_waiter() {
    tokio::time::timeout(
        Duration::from_secs(30),
        assert_h1_storage_cancelled_fill_hands_lock_to_waiter(
            "/test_h1_storage_cancelled_fill_hands_lock_to_waiter",
            true,
        ),
    )
    .await
    .expect("storage-cancelled H1 cache fill did not finish");
}

/// Without the opt-in, the storage's cancellation is a storage error, which passes the lock to
/// the waiter.
#[tokio::test]
async fn test_h1_storage_cancelled_fill_hands_lock_to_waiter_without_abandonment_handling() {
    tokio::time::timeout(
        Duration::from_secs(30),
        assert_h1_storage_cancelled_fill_hands_lock_to_waiter(
            "/test_h1_storage_cancelled_fill_hands_lock_to_waiter_without_abandonment_handling",
            false,
        ),
    )
    .await
    .expect("storage-cancelled H1 cache fill without abandonment handling did not finish");
}

// A cancellation is only honored once the downstream is gone; a connected client still gets everything.
async fn assert_h1_cancel_ignored_while_downstream_connected() {
    init();

    const PATH: &str = "/test_h1_fill_cancel_ignored_while_downstream_connected";
    let origin = StalledH1Origin::start("max-age=30").await;
    let before = Counters::before(PATH);

    let mut downstream = TcpStream::connect("127.0.0.1:6148").await.unwrap();
    downstream
        .write_all(
            format!(
                "GET {PATH} HTTP/1.1\r\nHost: {CACHE_HOST}\r\nX-Port: {}\r\n\
                 X-Record-Request-Lifecycle: true\r\nX-Cache-Fill-Cancellation-Watch: true\r\nX-Handle-Downstream-Abandonment: true\r\n\r\n",
                origin.port,
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    origin.request_seen.await.unwrap();
    origin.send_response.send(()).unwrap();

    let mut downstream = ChunkedResponseReader::new(downstream);
    let response_head = downstream.read_head().await;
    let first_chunk = downstream.read_body(FIRST_CHUNK.len()).await;
    assert!(
        response_head.contains("x-cache-status: miss"),
        "{response_head}"
    );
    assert_eq!(first_chunk, FIRST_CHUNK);
    wait_for_cache_fill_cancellation_handlers(PATH, before.handlers + 1).await;

    assert!(cancel_cache_fill(PATH));
    // Give a wrongly honored cancellation time to end the request before the origin is released.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        recorded_request_completions(PATH),
        before.completed,
        "the cancellation ended a fill whose downstream was still reading"
    );

    origin.release.send(()).unwrap();
    assert_eq!(origin.task.await.unwrap(), OriginOutcome::Released);
    let last_chunk = downstream.read_body(LAST_CHUNK.len()).await;
    assert_eq!(last_chunk, LAST_CHUNK);
    wait_for_recorded_request_completion(PATH, before.completed).await;
    assert_eq!(recorded_request_error(PATH), None);
    assert_eq!(
        downstream_abandoned_during_cache_fill_calls(PATH),
        before.abandoned
    );
    assert_eq!(
        cache_fill_cancellation_handlers_finished(PATH),
        before.finished + 1
    );
    assert_eq!(
        cache_fill_cancellation_handlers_dropped_unfinished(PATH),
        before.dropped_unfinished
    );
    assert_eq!(
        cache_fill_cancellation_body_bytes(PATH),
        before.body_bytes + FIRST_CHUNK.len() + LAST_CHUNK.len()
    );

    let cached = reqwest::Client::new()
        .get(format!("http://127.0.0.1:6148{PATH}"))
        .header(header::HOST, CACHE_HOST)
        .header("x-port", origin.port.to_string())
        .timeout(Duration::from_secs(5))
        .send()
        .await
        .unwrap();
    assert_eq!(cached.headers()["x-cache-status"], "hit");
    assert_eq!(
        cached.bytes().await.unwrap(),
        [FIRST_CHUNK, LAST_CHUNK].concat()
    );
}

#[tokio::test]
async fn test_h1_fill_cancel_ignored_while_downstream_connected() {
    tokio::time::timeout(
        Duration::from_secs(30),
        assert_h1_cancel_ignored_while_downstream_connected(),
    )
    .await
    .expect("connected cache fill did not finish");
}

// Offering a watcher changes nothing for a fill nobody cancels: it still runs to completion.
async fn assert_h1_watched_fill_without_cancellation_completes() {
    init();

    const PATH: &str = "/test_h1_watched_fill_without_cancellation_completes";
    let origin = StalledH1Origin::start("max-age=30").await;
    let before = Counters::before(PATH);

    let mut downstream = TcpStream::connect("127.0.0.1:6148").await.unwrap();
    downstream
        .write_all(
            format!(
                "GET {PATH} HTTP/1.1\r\nHost: {CACHE_HOST}\r\nX-Port: {}\r\n\
                 X-Record-Request-Lifecycle: true\r\nX-Cache-Fill-Cancellation-Watch: true\r\nX-Handle-Downstream-Abandonment: true\r\n\r\n",
                origin.port,
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    origin.request_seen.await.unwrap();
    origin.send_response.send(()).unwrap();

    let mut downstream = ChunkedResponseReader::new(downstream);
    downstream.read_head().await;
    let first_chunk = downstream.read_body(FIRST_CHUNK.len()).await;
    assert_eq!(first_chunk, FIRST_CHUNK);
    wait_for_cache_fill_cancellation_handlers(PATH, before.handlers + 1).await;

    drop(downstream);
    wait_for_cache_fill_abandonment(PATH, before.abandoned).await;
    // The abandoned fill is now watching. Without a cancellation it must keep waiting for the origin.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        recorded_request_completions(PATH),
        before.completed,
        "the watched fill ended without a cancellation"
    );
    assert!(!cache_fill_cancellation_requested(PATH));

    origin.release.send(()).unwrap();
    assert_eq!(origin.task.await.unwrap(), OriginOutcome::Released);
    wait_for_recorded_request_completion(PATH, before.completed).await;
    assert_eq!(recorded_request_error(PATH), None);
    assert_eq!(recorded_upstream_attempts(PATH), before.attempts + 1);
    assert_eq!(
        cache_fill_cancellation_handlers_finished(PATH),
        before.finished + 1
    );
    assert_eq!(
        cache_fill_cancellation_handlers_dropped_unfinished(PATH),
        before.dropped_unfinished
    );

    let cached = reqwest::Client::new()
        .get(format!("http://127.0.0.1:6148{PATH}"))
        .header(header::HOST, CACHE_HOST)
        .header("x-port", origin.port.to_string())
        .timeout(Duration::from_secs(5))
        .send()
        .await
        .unwrap();
    assert_eq!(cached.headers()["x-cache-status"], "hit");
    assert_eq!(
        cached.bytes().await.unwrap(),
        [FIRST_CHUNK, LAST_CHUNK].concat()
    );
}

#[tokio::test]
async fn test_h1_watched_fill_without_cancellation_completes() {
    tokio::time::timeout(
        Duration::from_secs(30),
        assert_h1_watched_fill_without_cancellation_completes(),
    )
    .await
    .expect("watched cache fill did not finish");
}

// A fill that refreshes an expired asset can be cancelled like a plain miss.
async fn assert_h1_cancel_ends_stalled_expired_refill() {
    init();

    const PATH: &str = "/test_h1_fill_cancel_ends_stalled_expired_refill";
    let before = Counters::before(PATH);

    seed_cache(PATH).await;

    let origin = StalledH1Origin::start("max-age=30").await;
    let mut downstream = TcpStream::connect("127.0.0.1:6148").await.unwrap();
    downstream
        .write_all(
            format!(
                "GET {PATH} HTTP/1.1\r\nHost: {CACHE_HOST}\r\nX-Port: {}\r\n\
                 X-Force-Expire: true\r\nX-Lock: true\r\nX-Cache-Lock-Wait-Timeout-Ms: 10000\r\n\
                 X-Record-Request-Lifecycle: true\r\nX-Cache-Fill-Cancellation-Watch: true\r\nX-Handle-Downstream-Abandonment: true\r\n\
                 X-Force-Retry-In-Error-Hook: true\r\n\r\n",
                origin.port,
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    origin.request_seen.await.unwrap();
    origin.send_response.send(()).unwrap();

    let mut downstream = ChunkedResponseReader::new(downstream);
    let response_head = downstream.read_head().await;
    let first_chunk = downstream.read_body(FIRST_CHUNK.len()).await;
    assert!(
        response_head.contains("x-cache-status: expired"),
        "{response_head}"
    );
    assert_eq!(first_chunk, FIRST_CHUNK);
    wait_for_cache_fill_cancellation_handlers(PATH, before.handlers + 1).await;

    drop(downstream);
    wait_for_cache_fill_abandonment(PATH, before.abandoned).await;
    assert!(cancel_cache_fill(PATH));

    wait_for_recorded_request_completion(PATH, before.completed).await;
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), origin.task)
            .await
            .expect("the proxy kept the stalled upstream connection open")
            .unwrap(),
        OriginOutcome::ProxyClosed
    );
    before.assert_cancelled(PATH, FIRST_CHUNK.len());
    drop(origin.release);

    // The cancelled refill left the lock free; the stale asset is still served.
    let stale = reqwest::Client::new()
        .get(format!("http://127.0.0.1:6148{PATH}"))
        .header(header::HOST, CACHE_HOST)
        .header("x-port", origin.port.to_string())
        .header("x-lock", "true")
        .header("x-cache-lock-wait-timeout-ms", "10000")
        .timeout(Duration::from_secs(5))
        .send()
        .await
        .expect("request after a cancelled refill remained blocked on the cache lock");
    assert_eq!(stale.headers()["x-cache-status"], "hit");
    assert_eq!(stale.bytes().await.unwrap(), b"seed".as_slice());
}

#[tokio::test]
async fn test_h1_fill_cancel_ends_stalled_expired_refill() {
    tokio::time::timeout(
        Duration::from_secs(30),
        assert_h1_cancel_ends_stalled_expired_refill(),
    )
    .await
    .expect("cancelled expired refill did not finish");
}

/// Without the opt-in, a client that leaves an expired refill is ignored for the fill: the hook
/// that would cancel does not run, a cancellation requested anyway is ignored, and the refill
/// replaces the stale asset, as before cancellation existed.
async fn assert_h1_cancel_ignored_for_expired_refill_without_abandonment_handling() {
    init();

    const PATH: &str =
        "/test_h1_fill_cancel_ignored_for_expired_refill_without_abandonment_handling";
    let before = Counters::before(PATH);
    seed_cache(PATH).await;

    let origin = StalledH1Origin::start("max-age=30").await;
    let reported_before = downstream_cache_warn_log_calls_for(PATH);
    let mut downstream = TcpStream::connect("127.0.0.1:6148").await.unwrap();
    downstream
        .write_all(
            format!(
                "GET {PATH} HTTP/1.1\r\nHost: {CACHE_HOST}\r\nX-Port: {}\r\n\
                 X-Force-Expire: true\r\nX-Lock: true\r\nX-Cache-Lock-Wait-Timeout-Ms: 10000\r\n\
                 X-Record-Request-Lifecycle: true\r\nX-Cache-Fill-Cancellation-Watch: true\r\n\
                 X-Force-Retry-In-Error-Hook: true\r\nX-Cancel-Fill-On-Abandon: true\r\n\r\n",
                origin.port,
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    origin.request_seen.await.unwrap();
    origin.send_response.send(()).unwrap();

    let mut downstream = ChunkedResponseReader::new(downstream);
    let response_head = downstream.read_head().await;
    let first_chunk = downstream.read_body(FIRST_CHUNK.len()).await;
    assert!(
        response_head.contains("x-cache-status: expired"),
        "{response_head}"
    );
    assert_eq!(first_chunk, FIRST_CHUNK);
    wait_for_cache_fill_cancellation_handlers(PATH, before.handlers + 1).await;

    drop(downstream);
    wait_for_client_departure(PATH, reported_before).await;
    assert!(
        cancel_cache_fill(PATH),
        "the abandonment hook ran for a request that did not opt in"
    );
    origin.release.send(()).unwrap();
    assert_eq!(origin.task.await.unwrap(), OriginOutcome::Released);
    wait_for_recorded_request_completion(PATH, before.completed).await;
    before.assert_filled_without_abandonment(PATH);
    assert_cache_hit(PATH, origin.port, &[FIRST_CHUNK, LAST_CHUNK].concat()).await;
}

#[tokio::test]
async fn test_h1_fill_cancel_ignored_for_expired_refill_without_abandonment_handling() {
    tokio::time::timeout(
        Duration::from_secs(30),
        assert_h1_cancel_ignored_for_expired_refill_without_abandonment_handling(),
    )
    .await
    .expect("expired refill without abandonment handling did not finish");
}

/// An HTTP/2 origin that sends the response head and [`FIRST_CHUNK`], then nothing until released.
struct StalledH2Origin {
    port: u16,
    release: tokio::sync::oneshot::Sender<()>,
    task: tokio::task::JoinHandle<OriginOutcome>,
}

impl StalledH2Origin {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (release, release_rx) = tokio::sync::oneshot::channel::<()>();
        let task = tokio::spawn(async move {
            let (io, _) = tokio::time::timeout(Duration::from_secs(5), listener.accept())
                .await
                .expect("proxy did not connect to the upstream")
                .unwrap();
            let mut connection = h2::server::handshake(io).await.unwrap();
            let (_request, mut response) =
                tokio::time::timeout(Duration::from_secs(5), connection.accept())
                    .await
                    .expect("proxy did not send the upstream request")
                    .expect("upstream connection closed before the request")
                    .unwrap();
            let connection_task = tokio::spawn(async move {
                while let Some(request) = connection.accept().await {
                    if request.is_err() {
                        break;
                    }
                }
            });

            let response_header = Response::builder()
                .status(StatusCode::OK)
                .header(header::CACHE_CONTROL, "max-age=30")
                .body(())
                .unwrap();
            let mut response_body = response.send_response(response_header, false).unwrap();
            response_body
                .send_data(Bytes::from_static(FIRST_CHUNK), false)
                .unwrap();

            let outcome = tokio::select! {
                // A reset or a closed connection both surface here once the proxy lets go.
                _ = poll_fn(|cx| response_body.poll_reset(cx)) => OriginOutcome::ProxyClosed,
                released = release_rx => {
                    released.unwrap();
                    response_body
                        .send_data(Bytes::from_static(LAST_CHUNK), true)
                        .unwrap();
                    OriginOutcome::Released
                }
            };
            // A released origin keeps serving the connection, so the rest of the response reaches
            // the proxy; the test's runtime drops it at the end.
            if outcome == OriginOutcome::ProxyClosed {
                connection_task.abort();
            }
            outcome
        });
        Self {
            port,
            release,
            task,
        }
    }
}

async fn assert_h2_cancel_ends_stalled_fill(timing: CancellationTiming) {
    init();

    let path = match timing {
        CancellationTiming::InHook => "/test_h2_fill_cancel_in_hook_ends_stalled_fill",
        CancellationTiming::AfterAbandonment => {
            "/test_h2_fill_cancel_after_abandonment_ends_stalled_fill"
        }
    };
    // `h2c_client` already holds the connection to the proxy, so the authority here only sets
    // `:authority`. It must be `CACHE_HOST`, the host H1 tests send: `cache_key_callback` keys
    // the fill by it, and so do the cancellation accessors below.
    let uri = format!("http://{CACHE_HOST}{path}");
    let origin = StalledH2Origin::start().await;

    let mut client = h2c_client(6154).await;
    let before = Counters::before(path);
    let mut request = Request::builder()
        .method("GET")
        .uri(uri.as_str())
        .header("x-h2", "true")
        .header("x-port", origin.port.to_string())
        .header("x-lock", "true")
        .header("x-cache-lock-wait-timeout-ms", "10000")
        .header("x-record-request-lifecycle", "true")
        .header("x-cache-fill-cancellation-watch", "true")
        .header(HANDLE_ABANDONMENT.0, HANDLE_ABANDONMENT.1)
        .header("x-force-retry-in-error-hook", "true");
    if matches!(timing, CancellationTiming::InHook) {
        request = request.header("x-cancel-fill-on-abandon", "true");
    }
    let request = request.body(()).unwrap();
    let (response, mut request_body) = client.send_request(request, true).unwrap();
    let response = tokio::time::timeout(Duration::from_secs(5), response)
        .await
        .expect("proxy did not send response headers")
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["x-cache-status"], "miss");
    let mut response_body = response.into_body();
    assert_eq!(read_h2_body_chunk(&mut response_body).await, FIRST_CHUNK);
    wait_for_cache_fill_cancellation_handlers(path, before.handlers + 1).await;

    request_body.send_reset(h2::Reason::CANCEL);
    wait_for_cache_fill_abandonment(path, before.abandoned).await;
    if matches!(timing, CancellationTiming::AfterAbandonment) {
        assert_eq!(
            recorded_request_completions(path),
            before.completed,
            "the fill ended before anyone cancelled it"
        );
        assert!(cancel_cache_fill(path));
    }

    wait_for_recorded_request_completion(path, before.completed).await;
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), origin.task)
            .await
            .expect("the proxy kept the stalled upstream stream open")
            .unwrap(),
        OriginOutcome::ProxyClosed
    );
    before.assert_cancelled(path, FIRST_CHUNK.len());
    drop(origin.release);

    // Not the shared `assert_failed_fill_recovers`: it probes under `Host: 127.0.0.1:6154`, the
    // host the rest of this suite fills under, which is a different cache entry than this one.
    assert_fill_recovers(path).await;
}

#[tokio::test]
async fn test_h2_fill_cancel_in_hook_ends_stalled_fill() {
    tokio::time::timeout(
        Duration::from_secs(30),
        assert_h2_cancel_ends_stalled_fill(CancellationTiming::InHook),
    )
    .await
    .expect("cancelled H2 cache fill did not finish");
}

#[tokio::test]
async fn test_h2_fill_cancel_after_abandonment_ends_stalled_fill() {
    tokio::time::timeout(
        Duration::from_secs(30),
        assert_h2_cancel_ends_stalled_fill(CancellationTiming::AfterAbandonment),
    )
    .await
    .expect("cancelled H2 cache fill did not finish");
}

/// Without the opt-in, an H2 reset is ignored for the fill: the hook that would cancel does not
/// run, a cancellation requested anyway is ignored, and the fill completes, as before
/// cancellation existed.
async fn assert_h2_cancel_ignored_without_abandonment_handling() {
    init();

    const PATH: &str = "/test_h2_fill_cancel_ignored_without_abandonment_handling";
    let origin = StalledH2Origin::start().await;
    let mut client = h2c_client(6154).await;
    let before = Counters::before(PATH);
    let reported_before = downstream_cache_warn_log_calls_for(PATH);
    let request = Request::builder()
        .method("GET")
        .uri(format!("http://{CACHE_HOST}{PATH}"))
        .header("x-h2", "true")
        .header("x-port", origin.port.to_string())
        .header("x-lock", "true")
        .header("x-cache-lock-wait-timeout-ms", "10000")
        .header("x-record-request-lifecycle", "true")
        .header("x-cache-fill-cancellation-watch", "true")
        .header("x-force-retry-in-error-hook", "true")
        .header("x-cancel-fill-on-abandon", "true")
        .body(())
        .unwrap();
    let (response, mut request_body) = client.send_request(request, true).unwrap();
    let response = tokio::time::timeout(Duration::from_secs(5), response)
        .await
        .expect("proxy did not send response headers")
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["x-cache-status"], "miss");
    let mut response_body = response.into_body();
    assert_eq!(read_h2_body_chunk(&mut response_body).await, FIRST_CHUNK);
    wait_for_cache_fill_cancellation_handlers(PATH, before.handlers + 1).await;

    request_body.send_reset(h2::Reason::CANCEL);
    wait_for_client_departure(PATH, reported_before).await;
    assert!(
        cancel_cache_fill(PATH),
        "the abandonment hook ran for a request that did not opt in"
    );
    origin.release.send(()).unwrap();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), origin.task)
            .await
            .expect("the proxy did not finish reading the released upstream")
            .unwrap(),
        OriginOutcome::Released
    );
    wait_for_recorded_request_completion(PATH, before.completed).await;
    before.assert_filled_without_abandonment(PATH);
    assert_cache_hit(PATH, origin.port, &[FIRST_CHUNK, LAST_CHUNK].concat()).await;
}

#[tokio::test]
async fn test_h2_fill_cancel_ignored_without_abandonment_handling() {
    tokio::time::timeout(
        Duration::from_secs(30),
        assert_h2_cancel_ignored_without_abandonment_handling(),
    )
    .await
    .expect("H2 fill without abandonment handling did not finish");
}

// The abandonment hook must key its cancellation exactly like the miss handler does
// (`cache_key_full`: host, path and query), not by `uri.path()` alone. For a request whose path
// carries a query string, keying by path alone would target a different, nonexistent fill and
// leave this one stalled until the test's outer timeout fires.
async fn assert_h1_cancel_in_hook_ends_stalled_fill_with_query_string() {
    init();

    const PATH: &str = "/test_h1_fill_cancel_in_hook_ends_stalled_fill_with_query_string?a=1&b=2";
    let origin = StalledH1Origin::start("max-age=30").await;
    let before = Counters::before(PATH);

    let mut downstream = TcpStream::connect("127.0.0.1:6148").await.unwrap();
    downstream
        .write_all(
            format!(
                "GET {PATH} HTTP/1.1\r\nHost: {CACHE_HOST}\r\nX-Port: {}\r\n\
                 X-Lock: true\r\nX-Cache-Lock-Wait-Timeout-Ms: 10000\r\n\
                 X-Record-Request-Lifecycle: true\r\nX-Cache-Fill-Cancellation-Watch: true\r\nX-Handle-Downstream-Abandonment: true\r\n\
                 X-Force-Retry-In-Error-Hook: true\r\nX-Cancel-Fill-On-Abandon: true\r\n\r\n",
                origin.port,
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    origin.request_seen.await.unwrap();
    origin.send_response.send(()).unwrap();

    let mut downstream = ChunkedResponseReader::new(downstream);
    let response_head = downstream.read_head().await;
    let first_chunk = downstream.read_body(FIRST_CHUNK.len()).await;
    assert!(
        response_head.lines().next().unwrap().contains(" 200 "),
        "{response_head}"
    );
    assert!(
        response_head.contains("x-cache-status: miss"),
        "{response_head}"
    );
    assert_eq!(first_chunk, FIRST_CHUNK);
    wait_for_cache_fill_cancellation_handlers(PATH, before.handlers + 1).await;

    drop(downstream);
    wait_for_cache_fill_abandonment(PATH, before.abandoned).await;
    wait_for_recorded_request_completion(PATH, before.completed).await;
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), origin.task)
            .await
            .expect(
                "the proxy kept the stalled upstream connection open: the hook's cancellation \
                 key did not match the miss handler's for a path with a query string"
            )
            .unwrap(),
        OriginOutcome::ProxyClosed
    );
    before.assert_cancelled(PATH, FIRST_CHUNK.len());
}

#[tokio::test]
async fn test_h1_fill_cancel_in_hook_ends_stalled_fill_with_query_string() {
    tokio::time::timeout(
        Duration::from_secs(30),
        assert_h1_cancel_in_hook_ends_stalled_fill_with_query_string(),
    )
    .await
    .expect("cancelled H1 cache fill with a query string did not finish");
}

// A cancelled fill's cancellation must not leak into a later, unrelated fill of the same path:
// after `reset_cancellation`, a fresh watched fill of the same path runs to completion and
// commits normally instead of inheriting the earlier, already-fired cancellation.
async fn assert_h1_watched_refill_after_cancellation_completes() {
    init();

    const PATH: &str = "/test_h1_watched_refill_after_cancellation_completes";
    let origin = StalledH1Origin::start("max-age=30").await;
    let before = Counters::before(PATH);

    let mut downstream = TcpStream::connect("127.0.0.1:6148").await.unwrap();
    downstream
        .write_all(
            format!(
                "GET {PATH} HTTP/1.1\r\nHost: {CACHE_HOST}\r\nX-Port: {}\r\n\
                 X-Lock: true\r\nX-Cache-Lock-Wait-Timeout-Ms: 10000\r\n\
                 X-Record-Request-Lifecycle: true\r\nX-Cache-Fill-Cancellation-Watch: true\r\nX-Handle-Downstream-Abandonment: true\r\n\
                 X-Force-Retry-In-Error-Hook: true\r\nX-Cancel-Fill-On-Abandon: true\r\n\r\n",
                origin.port,
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    origin.request_seen.await.unwrap();
    origin.send_response.send(()).unwrap();

    let mut downstream = ChunkedResponseReader::new(downstream);
    let response_head = downstream.read_head().await;
    let first_chunk = downstream.read_body(FIRST_CHUNK.len()).await;
    assert!(
        response_head.contains("x-cache-status: miss"),
        "{response_head}"
    );
    assert_eq!(first_chunk, FIRST_CHUNK);
    wait_for_cache_fill_cancellation_handlers(PATH, before.handlers + 1).await;

    drop(downstream);
    wait_for_cache_fill_abandonment(PATH, before.abandoned).await;
    wait_for_recorded_request_completion(PATH, before.completed).await;
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), origin.task)
            .await
            .expect("the proxy kept the stalled upstream connection open")
            .unwrap(),
        OriginOutcome::ProxyClosed
    );
    before.assert_cancelled(PATH, FIRST_CHUNK.len());

    // Without this, the second fill below would inherit the first fill's already-fired
    // cancellation and end immediately instead of running to completion.
    reset_cancellation(PATH);

    let origin = StalledH1Origin::start("max-age=30").await;
    let before = Counters::before(PATH);

    let mut downstream = TcpStream::connect("127.0.0.1:6148").await.unwrap();
    downstream
        .write_all(
            format!(
                "GET {PATH} HTTP/1.1\r\nHost: {CACHE_HOST}\r\nX-Port: {}\r\n\
                 X-Record-Request-Lifecycle: true\r\nX-Cache-Fill-Cancellation-Watch: true\r\nX-Handle-Downstream-Abandonment: true\r\n\r\n",
                origin.port,
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    origin.request_seen.await.unwrap();
    origin.send_response.send(()).unwrap();

    let mut downstream = ChunkedResponseReader::new(downstream);
    let response_head = downstream.read_head().await;
    let first_chunk = downstream.read_body(FIRST_CHUNK.len()).await;
    assert!(
        response_head.contains("x-cache-status: miss"),
        "{response_head}"
    );
    assert_eq!(first_chunk, FIRST_CHUNK);
    wait_for_cache_fill_cancellation_handlers(PATH, before.handlers + 1).await;
    assert!(!cache_fill_cancellation_requested(PATH));

    origin.release.send(()).unwrap();
    assert_eq!(origin.task.await.unwrap(), OriginOutcome::Released);
    wait_for_recorded_request_completion(PATH, before.completed).await;
    assert_eq!(recorded_request_error(PATH), None);
    assert_eq!(recorded_upstream_attempts(PATH), before.attempts + 1);
    assert_eq!(
        cache_fill_cancellation_handlers_finished(PATH),
        before.finished + 1
    );
    assert_eq!(
        cache_fill_cancellation_handlers_dropped_unfinished(PATH),
        before.dropped_unfinished
    );

    let cached = reqwest::Client::new()
        .get(format!("http://127.0.0.1:6148{PATH}"))
        .header(header::HOST, CACHE_HOST)
        .header("x-port", origin.port.to_string())
        .timeout(Duration::from_secs(5))
        .send()
        .await
        .unwrap();
    assert_eq!(cached.headers()["x-cache-status"], "hit");
    assert_eq!(
        cached.bytes().await.unwrap(),
        [FIRST_CHUNK, LAST_CHUNK].concat()
    );
}

#[tokio::test]
async fn test_h1_watched_refill_after_cancellation_completes() {
    tokio::time::timeout(
        Duration::from_secs(30),
        assert_h1_watched_refill_after_cancellation_completes(),
    )
    .await
    .expect("watched refill after a cancelled fill did not finish");
}

/// Without the opt-in the proxy never watches for a cancellation: the abandonment hook that would
/// cancel does not run, a cancellation requested anyway is ignored, and the fill runs to
/// completion, as before cancellation existed.
async fn assert_h1_cancel_ignored_without_abandonment_handling() {
    init();

    let path = "/test_h1_fill_cancel_ignored_without_abandonment_handling";
    let origin = StalledH1Origin::start("max-age=30").await;
    let before = Counters::before(path);
    let ignored_before = downstream_cache_warn_log_calls_for(path);

    let mut downstream = TcpStream::connect("127.0.0.1:6148").await.unwrap();
    downstream
        .write_all(
            format!(
                "GET {path} HTTP/1.1\r\nHost: {CACHE_HOST}\r\nX-Port: {}\r\n\
                 X-Lock: true\r\nX-Cache-Lock-Wait-Timeout-Ms: 10000\r\n\
                 X-Record-Request-Lifecycle: true\r\nX-Cache-Fill-Cancellation-Watch: true\r\n\
                 X-Cancel-Fill-On-Abandon: true\r\n\r\n",
                origin.port
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    origin.request_seen.await.unwrap();
    origin.send_response.send(()).unwrap();

    let mut downstream = ChunkedResponseReader::new(downstream);
    let head = downstream.read_head().await;
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    assert_eq!(downstream.read_body(FIRST_CHUNK.len()).await, FIRST_CHUNK);
    wait_for_cache_fill_cancellation_handlers(path, before.handlers + 1).await;
    drop(downstream);
    // The proxy saw the client leave and kept filling, as it does without the opt-in, before
    // anyone asks for the cancellation.
    wait_until("the proxy did not observe the client leaving", || {
        downstream_cache_warn_log_calls_for(path) > ignored_before
    })
    .await;

    assert!(
        cancel_cache_fill(path),
        "the abandonment hook ran for a request that did not opt in"
    );
    origin.release.send(()).unwrap();
    assert_eq!(origin.task.await.unwrap(), OriginOutcome::Released);
    wait_for_recorded_request_completion(path, before.completed).await;

    assert_eq!(
        downstream_abandoned_during_cache_fill_calls(path),
        before.abandoned
    );
    assert_eq!(recorded_request_error(path), None);
    assert_eq!(recorded_upstream_attempts(path), before.attempts + 1);
    assert_eq!(
        cache_fill_cancellation_body_bytes(path),
        before.body_bytes + FIRST_CHUNK.len() + LAST_CHUNK.len()
    );
    assert_eq!(
        cache_fill_cancellation_handlers_finished(path),
        before.finished + 1,
        "a fill that did not opt in must run to completion"
    );
    assert_eq!(
        cache_fill_cancellation_handlers_dropped_unfinished(path),
        before.dropped_unfinished
    );

    let cached = reqwest::Client::new()
        .get(format!("http://127.0.0.1:6148{path}"))
        .header(header::HOST, CACHE_HOST)
        .header("x-port", origin.port.to_string())
        .timeout(Duration::from_secs(5))
        .send()
        .await
        .unwrap();
    assert_eq!(cached.headers()["x-cache-status"], "hit");
    assert_eq!(
        cached.bytes().await.unwrap(),
        [FIRST_CHUNK, LAST_CHUNK].concat()
    );
}

#[tokio::test]
async fn test_h1_fill_cancel_ignored_without_abandonment_handling() {
    tokio::time::timeout(
        Duration::from_secs(30),
        assert_h1_cancel_ignored_without_abandonment_handling(),
    )
    .await
    .expect("fill without abandonment handling did not finish");
}

/// Body bytes the streaming origin offers: far more than the socket buffers and the proxy's
/// upstream reads in flight hold, so a fill that keeps reading visibly pulls the rest.
const STREAMED_BODY_BYTES: usize = 16 << 20;
/// The streaming origin writes the body in chunks of this size, checking before each one whether
/// the proxy saw the client leave. Its count of bytes sent after that point can miss one chunk.
const STREAMED_CHUNK_BYTES: usize = 16 << 10;
/// Send buffer of the streaming origin's socket, receive buffer of the proxy's upstream socket
/// and receive buffer of the client. Fixed sizes stop the kernel from growing them.
const STREAMING_SOCKET_BUFFER_BYTES: usize = 64 << 10;
/// Most body bytes the streaming origin may send once the proxy saw the client leave and the
/// abandonment hook cancelled the fill: a sixteenth of the body. What the origin can still send
/// then is the two fixed socket buffers (the kernel doubles each requested size) plus the
/// proxy's upstream reads in flight, well under this.
const BYTES_AFTER_CANCEL_LIMIT: usize = 1 << 20;

struct StreamedBody {
    /// Body bytes the origin's socket accepted.
    sent: usize,
    /// `sent` when the origin first saw the proxy report the client's departure.
    sent_before_client_left: Option<usize>,
    /// Whether the origin sent the whole body and its terminating chunk.
    complete: bool,
}

impl StreamedBody {
    fn sent_after_client_left(&self) -> usize {
        self.sent
            - self
                .sent_before_client_left
                .expect("the proxy did not report the client leaving while the origin streamed")
    }
}

/// An origin that streams [`STREAMED_BODY_BYTES`] as fast as the proxy reads them, and notes
/// how much it had sent when the proxy first reported the client of `path` leaving.
async fn start_streaming_origin(
    path: &'static str,
) -> (u16, tokio::task::JoinHandle<StreamedBody>) {
    let socket = tokio::net::TcpSocket::new_v4().unwrap();
    socket
        .set_send_buffer_size(STREAMING_SOCKET_BUFFER_BYTES as u32)
        .unwrap();
    socket.bind("127.0.0.1:0".parse().unwrap()).unwrap();
    let listener = socket.listen(1).unwrap();
    let port = listener.local_addr().unwrap().port();
    let reported_before = downstream_cache_warn_log_calls_for(path);
    let task = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        read_h1_head(&mut socket).await;
        socket
            .write_all(
                b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\
                  Cache-Control: max-age=30\r\n\r\n",
            )
            .await
            .unwrap();
        let chunk = h1_chunk(&[b'x'; STREAMED_CHUNK_BYTES]);
        let mut body = StreamedBody {
            sent: 0,
            sent_before_client_left: None,
            complete: false,
        };
        while body.sent < STREAMED_BODY_BYTES {
            if body.sent_before_client_left.is_none()
                && downstream_cache_warn_log_calls_for(path) > reported_before
            {
                body.sent_before_client_left = Some(body.sent);
            }
            if socket.write_all(&chunk).await.is_err() {
                // The proxy reported the departure during the write that failed, so nothing
                // counted followed it.
                if downstream_cache_warn_log_calls_for(path) > reported_before {
                    body.sent_before_client_left.get_or_insert(body.sent);
                }
                return body;
            }
            body.sent += STREAMED_CHUNK_BYTES;
        }
        body.complete = socket.write_all(b"0\r\n\r\n").await.is_ok();
        body
    });
    (port, task)
}

/// Start a fill of `path` from the streaming origin, then leave once some body reached the client.
async fn leave_streamed_fill(path: &str, origin_port: u16, opted_in: bool, handlers_before: usize) {
    let socket = tokio::net::TcpSocket::new_v4().unwrap();
    socket
        .set_recv_buffer_size(STREAMING_SOCKET_BUFFER_BYTES as u32)
        .unwrap();
    let mut downstream = socket
        .connect("127.0.0.1:6148".parse().unwrap())
        .await
        .unwrap();
    downstream
        .write_all(
            format!(
                "GET {path} HTTP/1.1\r\nHost: {CACHE_HOST}\r\nX-Port: {origin_port}\r\n\
                 X-Upstream-Tcp-Recv-Buf: {STREAMING_SOCKET_BUFFER_BYTES}\r\n\
                 X-Record-Request-Lifecycle: true\r\nX-Cache-Fill-Cancellation-Watch: true\r\n\
                 X-Cancel-Fill-On-Abandon: true\r\n{}\r\n",
                if opted_in {
                    "X-Handle-Downstream-Abandonment: true\r\n"
                } else {
                    ""
                }
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    let mut downstream = ChunkedResponseReader::new(downstream);
    let response_head = downstream.read_head().await;
    assert!(
        response_head.contains("x-cache-status: miss"),
        "{response_head}"
    );
    while downstream.received.is_empty() {
        downstream.read_more().await;
    }
    wait_for_cache_fill_cancellation_handlers(path, handlers_before + 1).await;
    drop(downstream);
}

// The goal of the cancellation: once the client left and the hook cancelled the fill, an origin
// that streams as fast as the proxy reads sends only a bounded number of further bytes.
async fn assert_cancel_bounds_streaming_origin_bytes() {
    init();

    const PATH: &str = "/test_h1_fill_cancel_bounds_streaming_origin_bytes";
    let before = Counters::before(PATH);
    let (port, origin) = start_streaming_origin(PATH).await;
    leave_streamed_fill(PATH, port, true, before.handlers).await;
    wait_for_cache_fill_abandonment(PATH, before.abandoned).await;
    wait_for_recorded_request_completion(PATH, before.completed).await;

    let body = tokio::time::timeout(TEST_TIMEOUT, origin)
        .await
        .expect("the proxy kept the upstream connection open after the cancellation")
        .unwrap();
    assert!(!body.complete, "the cancelled fill read the whole body");
    let sent_after_client_left = body.sent_after_client_left();
    assert!(
        sent_after_client_left <= BYTES_AFTER_CANCEL_LIMIT,
        "the origin sent {sent_after_client_left} bytes after the proxy saw the client leave"
    );
    assert_eq!(recorded_request_error(PATH), Some(ERR_CACHE_FILL_CANCELLED));
    assert_eq!(
        downstream_abandoned_during_cache_fill_calls(PATH),
        before.abandoned + 1
    );
    assert_eq!(
        cache_fill_cancellation_handlers_finished(PATH),
        before.finished
    );
    assert_eq!(
        cache_fill_cancellation_handlers_dropped_unfinished(PATH),
        before.dropped_unfinished + 1
    );
}

#[tokio::test]
async fn test_h1_fill_cancel_bounds_streaming_origin_bytes() {
    tokio::time::timeout(
        Duration::from_secs(30),
        assert_cancel_bounds_streaming_origin_bytes(),
    )
    .await
    .expect("cancelled streaming fill did not finish");
}

// Without the opt-in the same fill keeps reading after the client left: the origin sends the
// whole body, far more than the bound above after the departure, and it is cached.
async fn assert_streaming_origin_sends_whole_body_without_abandonment_handling() {
    init();

    const PATH: &str = "/test_h1_streaming_origin_sends_whole_body_without_abandonment_handling";
    let before = Counters::before(PATH);
    let (port, origin) = start_streaming_origin(PATH).await;
    leave_streamed_fill(PATH, port, false, before.handlers).await;

    let body = tokio::time::timeout(TEST_TIMEOUT, origin)
        .await
        .expect("the origin did not finish streaming")
        .unwrap();
    assert!(body.complete, "the fill stopped reading the origin");
    assert_eq!(body.sent, STREAMED_BODY_BYTES);
    let sent_after_client_left = body.sent_after_client_left();
    assert!(
        sent_after_client_left > BYTES_AFTER_CANCEL_LIMIT,
        "the client left too late to exercise the bound: only {sent_after_client_left} bytes \
         followed"
    );
    wait_for_recorded_request_completion(PATH, before.completed).await;
    assert_eq!(
        downstream_abandoned_during_cache_fill_calls(PATH),
        before.abandoned
    );
    assert_eq!(recorded_request_error(PATH), None);
    assert_eq!(
        cache_fill_cancellation_body_bytes(PATH),
        before.body_bytes + STREAMED_BODY_BYTES
    );
    assert_eq!(
        cache_fill_cancellation_handlers_finished(PATH),
        before.finished + 1
    );

    let cached = reqwest::Client::new()
        .get(format!("http://127.0.0.1:6148{PATH}"))
        .header(header::HOST, CACHE_HOST)
        .header("x-port", port.to_string())
        .timeout(TEST_TIMEOUT)
        .send()
        .await
        .unwrap();
    assert_eq!(cached.headers()["x-cache-status"], "hit");
    assert_eq!(cached.bytes().await.unwrap().len(), STREAMED_BODY_BYTES);
}

#[tokio::test]
async fn test_h1_streaming_origin_sends_whole_body_without_abandonment_handling() {
    tokio::time::timeout(
        Duration::from_secs(30),
        assert_streaming_origin_sends_whole_body_without_abandonment_handling(),
    )
    .await
    .expect("streaming fill without abandonment handling did not finish");
}
