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

//! A cache hit that streams a fill another request is still writing, while that fill waits on
//! its origin. Without the opt-in the hit never reads its client while it waits. Opted in, the
//! hit watches its client instead, until its response is complete: an HTTP/2 client from the
//! start, any other client only if its request is complete when the hit starts. The client
//! leaving ends the hit and releases its reader, and an HTTP/1 next request sent early is read as
//! the regular proxy reads it, which ends the hit unless pipelining is enabled. Every scenario
//! runs with and without the opt-in on the reader.

use super::*;
use crate::utils::server_utils::{
    demand_fill_observations, recorded_error_log_offers, DemandFillObservations,
};
use pingora_error::ErrorType;
use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};

const HOST: &str = "stalled-hit.example";
/// The HTTP/1 cache proxy.
const H1_PROXY: &str = "127.0.0.1:6148";
/// The h2c cache proxy, which shares its cache with [`H1_PROXY`].
const H2_PROXY_PORT: u16 = 6154;

/// A cacheable response header with a chunked body, so the proxy flushes each chunk to the client
/// as it arrives. (It holds a small `Content-Length` body until the body is complete.)
const CHUNKED_HEADER: &[u8] = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\
    Cache-Control: max-age=60\r\nETag: \"v1\"\r\n\r\n";
const HELLO_CHUNK: &[u8] = b"6\r\nhello \r\n";
const WORLD_CHUNK: &[u8] = b"6\r\nworld!\r\n";
const MORE_CHUNK: &[u8] = b"5\r\nmore!\r\n";
const REST_CHUNK: &[u8] = b"5\r\nrest!\r\n";
const LAST_CHUNK: &[u8] = b"0\r\n\r\n";

/// Selects the storage whose readers fail once their fill's writer is dropped unfinished.
const FAIL_READERS_OF_DROPPED_WRITES: &str = "X-Cache-Fail-Readers-Of-Dropped-Writes: true\r\n";
/// Selects the storage that stops a fill once neither its writer nor a reader wants it.
const DEMAND_COUNTING: &str = "X-Cache-Demand-Counting: true\r\n";
/// Enables HTTP/1.1 pipelining on the request's connection.
const PIPELINING: &str = "X-Pipelining: true\r\n";
/// Disables `abort_on_close` on the request's HTTP/1 connection, so a half-closed client keeps
/// its response.
const KEEP_HALF_CLOSED: &str = "X-Abort-On-Close: false\r\n";
/// Selects an HTTP/2 upstream.
const H2_UPSTREAM: &str = "X-H2: true\r\n";

/// The reader's own header lines: it records its lifecycle, and opts in if `handles_abandonment`.
/// Writers never record theirs, so the recorded outcome of `path` is the reader's.
fn reader_headers(handles_abandonment: bool) -> String {
    let opt_in = if handles_abandonment {
        "X-Handle-Downstream-Abandonment: true\r\n"
    } else {
        ""
    };
    format!("X-Record-Request-Lifecycle: true\r\n{opt_in}")
}

/// A request to the HTTP/1 cache proxy. Its cache lock wait is long enough that only the fill
/// can end it.
fn h1_request(method: &str, path: &str, port: u16, extra: &str) -> String {
    format!(
        "{method} {path} HTTP/1.1\r\nHost: {HOST}\r\nX-Port: {port}\r\nX-Lock: true\r\n\
         X-Cache-Lock-Wait-Timeout-Ms: 10000\r\n{extra}\r\n"
    )
}

async fn connect() -> TcpStream {
    TcpStream::connect(H1_PROXY).await.unwrap()
}

/// Close `stream` with a reset, so the proxy's next read or write on it fails at once.
fn reset(stream: TcpStream) {
    stream.set_zero_linger().unwrap();
    drop(stream);
}

/// Read one byte at a time until `n` bytes arrived. `None` if the connection ends first.
async fn read_n(stream: &mut TcpStream, n: usize) -> Option<Vec<u8>> {
    let mut buf = vec![0u8; n];
    match tokio::time::timeout(TEST_TIMEOUT, stream.read_exact(&mut buf))
        .await
        .expect("the proxy neither sent the expected bytes nor closed the connection")
    {
        Ok(_) => Some(buf),
        Err(_) => None,
    }
}

/// Read an HTTP/1 response head without consuming any of its body. `None` if the connection
/// ends first.
async fn read_head(stream: &mut TcpStream) -> Option<String> {
    let mut head = Vec::new();
    while !head.ends_with(b"\r\n\r\n") {
        head.extend(read_n(stream, 1).await?);
    }
    Some(String::from_utf8(head).unwrap())
}

fn header_value<'a>(head: &'a str, name: &str) -> Option<&'a str> {
    head.lines().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        key.eq_ignore_ascii_case(name).then(|| value.trim())
    })
}

/// Read one chunk of a chunked body; the last chunk reads as an empty one. `None` if the
/// connection ends first.
async fn read_chunk(stream: &mut TcpStream) -> Option<Vec<u8>> {
    let mut size_line = Vec::new();
    while !size_line.ends_with(b"\r\n") {
        size_line.extend(read_n(stream, 1).await?);
    }
    let size_line = std::str::from_utf8(&size_line).unwrap().trim_end();
    let size = usize::from_str_radix(size_line, 16).expect(size_line);
    let chunk = read_n(stream, size).await?;
    assert_eq!(read_n(stream, 2).await?, b"\r\n", "chunk not terminated");
    Some(chunk)
}

/// How a [`ScriptedOrigin`] connection ended.
#[derive(Debug, PartialEq)]
enum OriginEnd {
    /// The test closed it after the parts it sent.
    Released,
    /// The proxy closed it first.
    ProxyClosed,
}

/// An HTTP/1 origin for one request whose response the test writes part by part.
struct ScriptedOrigin {
    port: u16,
    requested: oneshot::Receiver<()>,
    writes: mpsc::UnboundedSender<(&'static [u8], oneshot::Sender<bool>)>,
    task: tokio::task::JoinHandle<OriginEnd>,
}

impl ScriptedOrigin {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (requested_tx, requested) = oneshot::channel();
        let (writes, mut writes_rx) =
            mpsc::unbounded_channel::<(&'static [u8], oneshot::Sender<bool>)>();
        let task = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            read_http1_head(&mut socket).await;
            requested_tx.send(()).unwrap();
            let mut probe = [0u8; 1];
            loop {
                tokio::select! {
                    write = writes_rx.recv() => match write {
                        Some((bytes, written)) => {
                            let _ = written.send(socket.write_all(bytes).await.is_ok());
                        }
                        None => return OriginEnd::Released,
                    },
                    read = socket.read(&mut probe) => {
                        assert!(matches!(read, Ok(0) | Err(_)), "unexpected request bytes");
                        return OriginEnd::ProxyClosed;
                    }
                }
            }
        });
        Self {
            port,
            requested,
            writes,
            task,
        }
    }

    async fn wait_for_request(&mut self) {
        tokio::time::timeout(TEST_TIMEOUT, &mut self.requested)
            .await
            .expect("the origin did not get the request")
            .unwrap();
    }

    /// Write the next part of the response.
    async fn send(&self, bytes: &'static [u8]) {
        let (written_tx, written) = oneshot::channel();
        self.writes.send((bytes, written_tx)).unwrap();
        let written = tokio::time::timeout(TEST_TIMEOUT, written)
            .await
            .expect("the origin did not write")
            .expect("the proxy closed the upstream connection first");
        assert!(written, "the origin could not write to the proxy");
    }

    /// Close the connection after what the test has sent.
    async fn finish(self) -> OriginEnd {
        drop(self.writes);
        tokio::time::timeout(TEST_TIMEOUT, self.task)
            .await
            .expect("the origin did not finish")
            .unwrap()
    }

    /// Wait for the proxy to close the upstream connection.
    async fn wait_for_proxy_close(self) {
        let end = tokio::time::timeout(TEST_TIMEOUT, self.task)
            .await
            .expect("the proxy kept reading the origin")
            .unwrap();
        assert_eq!(end, OriginEnd::ProxyClosed);
    }
}

/// An origin that answers every request with `next` and counts them.
struct NextOrigin {
    origin: HttpOrigin,
    requests: Arc<AtomicUsize>,
}

impl NextOrigin {
    async fn start() -> Self {
        let requests = Arc::new(AtomicUsize::new(0));
        let counted = requests.clone();
        let origin = HttpOrigin::bind(move |_request| {
            counted.fetch_add(1, Ordering::Relaxed);
            async {
                Response::builder()
                    .header(header::CONTENT_LENGTH, "4")
                    .body(Bytes::from_static(b"next"))
                    .unwrap()
            }
        })
        .await
        .unwrap();
        Self { origin, requests }
    }

    fn port(&self) -> u16 {
        self.origin.addr().port()
    }

    /// A request for `{path}/next` that bypasses the cache, sent as the client's next request.
    fn next_request(&self, path: &str) -> String {
        format!(
            "GET {path}/next HTTP/1.1\r\nHost: {HOST}\r\nX-Port: {}\r\nX-Bypass-Cache: true\r\n\r\n",
            self.port()
        )
    }

    /// Read the response to [`Self::next_request`]: its status line and body, or `None` if the
    /// connection ends first.
    async fn read_response(stream: &mut TcpStream) -> Option<(String, Vec<u8>)> {
        let head = read_head(stream).await?;
        let status_line = head.lines().next().unwrap().to_owned();
        Some((status_line, read_n(stream, 4).await?))
    }

    fn requests(&self) -> usize {
        self.requests.load(Ordering::Relaxed)
    }

    async fn shutdown(self) {
        self.origin.shutdown().await;
    }
}

/// Start a miss whose origin has sent the response header and `hello `, and read both as the
/// writer's client.
async fn start_stalled_fill(path: &str, extra: &str) -> (ScriptedOrigin, TcpStream) {
    let mut origin = ScriptedOrigin::start().await;
    let mut writer = connect().await;
    writer
        .write_all(h1_request("GET", path, origin.port, extra).as_bytes())
        .await
        .unwrap();
    origin.wait_for_request().await;
    origin.send(CHUNKED_HEADER).await;
    origin.send(HELLO_CHUNK).await;
    let head = read_head(&mut writer).await.unwrap();
    assert_eq!(
        header_value(&head, "x-cache-status"),
        Some("miss"),
        "{head}"
    );
    assert_eq!(read_chunk(&mut writer).await.unwrap(), b"hello ");
    (origin, writer)
}

/// An HTTP/1 reader of the partly written entry: it gets the cached header and `hello `, and
/// its hit then waits on the fill.
async fn start_reader(path: &str, port: u16, extra: &str) -> TcpStream {
    let mut reader = connect().await;
    reader
        .write_all(h1_request("GET", path, port, extra).as_bytes())
        .await
        .unwrap();
    let head = read_head(&mut reader).await.unwrap();
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    assert_eq!(header_value(&head, "x-cache-status"), Some("hit"), "{head}");
    assert_eq!(read_chunk(&mut reader).await.unwrap(), b"hello ");
    reader
}

/// Release the rest of the fill and check that its writer's client received all of it.
async fn complete_fill(origin: ScriptedOrigin, mut writer: TcpStream) {
    origin.send(WORLD_CHUNK).await;
    origin.send(LAST_CHUNK).await;
    assert_eq!(origin.finish().await, OriginEnd::Released);
    assert_eq!(read_chunk(&mut writer).await.unwrap(), b"world!");
    assert_eq!(read_chunk(&mut writer).await.unwrap(), b"");
}

/// A follow-up `GET` on a new connection is served the complete entry from the cache. The origin
/// behind `port` no longer accepts connections, so a miss would fail.
async fn assert_cached(path: &str, port: u16) {
    let mut stream = connect().await;
    stream
        .write_all(
            format!(
                "GET {path} HTTP/1.1\r\nHost: {HOST}\r\nX-Port: {port}\r\nConnection: close\r\n\r\n"
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    let head = read_head(&mut stream).await.unwrap();
    assert_eq!(header_value(&head, "x-cache-status"), Some("hit"), "{head}");
    let mut body = Vec::new();
    loop {
        let chunk = read_chunk(&mut stream).await.unwrap();
        if chunk.is_empty() {
            break;
        }
        body.extend(chunk);
    }
    assert_eq!(body, b"hello world!");
}

/// The counters of the reader's request for `path` before it starts.
struct Before {
    completed: usize,
    abandoned: usize,
    offers: usize,
}

impl Before {
    fn of(path: &str) -> Self {
        Self {
            completed: recorded_request_completions(path),
            abandoned: downstream_abandoned_during_cache_fill_calls(path),
            offers: recorded_error_log_offers(path),
        }
    }
}

/// How the reader's request ended, as the application saw it.
#[derive(Debug, PartialEq)]
struct ReaderOutcome {
    /// Its final error, as `logging` received it.
    error: Option<ErrorType>,
    /// Final errors offered to `suppress_error_log` before they were logged.
    error_log_offers: usize,
    /// Abandonment hook calls.
    abandonments: usize,
}

impl ReaderOutcome {
    /// A reader whose hit completed.
    const COMPLETED: Self = Self {
        error: None,
        error_log_offers: 0,
        abandonments: 0,
    };

    /// Wait for the reader's request for `path` to end.
    async fn wait(path: &str, before: &Before) -> Self {
        wait_for_recorded_request_completion(path, before.completed).await;
        Self::recorded(path, before)
    }

    fn recorded(path: &str, before: &Before) -> Self {
        Self {
            error: recorded_request_error(path),
            error_log_offers: recorded_error_log_offers(path) - before.offers,
            abandonments: downstream_abandoned_during_cache_fill_calls(path) - before.abandoned,
        }
    }
}

/// Run `scenario` without the opt-in, then with it, and return both results.
async fn opted_out_then_in<F, T>(
    opted_out_path: &'static str,
    opted_in_path: &'static str,
    scenario: impl Fn(&'static str, bool) -> F,
) -> (T, T)
where
    F: Future<Output = T>,
{
    let run = async {
        let opted_out = scenario(opted_out_path, false).await;
        let opted_in = scenario(opted_in_path, true).await;
        (opted_out, opted_in)
    };
    tokio::time::timeout(std::time::Duration::from_secs(60), run)
        .await
        .expect("stalled hit scenario did not finish")
}

/// What a reader that sent its next request early observed.
#[derive(Debug, PartialEq)]
struct EarlyRequestOutcome {
    /// The reader's claims on the fill once its hit had the chance to end while the fill still
    /// waited for its origin.
    reader_claims_while_fill_waits: usize,
    /// The body chunks after `hello ` until the connection ended or the body was complete; the
    /// last chunk reads as an empty one.
    rest_of_body: Vec<Vec<u8>>,
    /// The next request's response, if it was served on the same connection.
    next_response: Option<(String, Vec<u8>)>,
    /// Requests the next request's origin received.
    next_requests: usize,
    reader: ReaderOutcome,
}

async fn run_early_next_request_during_stalled_hit(
    path: &'static str,
    handles_abandonment: bool,
    pipelining: bool,
) -> EarlyRequestOutcome {
    init();
    let before = Before::of(path);
    let next = NextOrigin::start().await;
    let (origin, mut writer) = start_stalled_fill(path, DEMAND_COUNTING).await;
    let pipelining_header = if pipelining { PIPELINING } else { "" };
    let mut reader = start_reader(
        path,
        origin.port,
        &format!(
            "{DEMAND_COUNTING}{pipelining_header}{}",
            reader_headers(handles_abandonment)
        ),
    )
    .await;
    reader
        .write_all(next.next_request(path).as_bytes())
        .await
        .unwrap();
    if handles_abandonment && !pipelining {
        // The fill still waits for its origin, so only the watch can end the hit and release the
        // reader's claim.
        wait_for_fill(path, "the reader did not release its claim", |fill| {
            fill.reader_claims == 0
        })
        .await;
    }
    let reader_claims_while_fill_waits = fill(path).reader_claims;

    // The fill resumes in two steps. The next request is on the connection before `world!`
    // reaches the fill, and the hit waits on the fill again after sending it, so a watching hit
    // has read from the client before the end of the body reaches the fill.
    let mut rest_of_body = Vec::new();
    origin.send(WORLD_CHUNK).await;
    if let Some(chunk) = read_chunk(&mut reader).await {
        rest_of_body.push(chunk);
    }
    origin.send(LAST_CHUNK).await;
    assert_eq!(origin.finish().await, OriginEnd::Released);
    if !rest_of_body.is_empty() {
        if let Some(chunk) = read_chunk(&mut reader).await {
            rest_of_body.push(chunk);
        }
    }
    let next_response = NextOrigin::read_response(&mut reader).await;
    assert_eq!(read_chunk(&mut writer).await.unwrap(), b"world!");
    assert_eq!(read_chunk(&mut writer).await.unwrap(), b"");
    let reader = ReaderOutcome::wait(path, &before).await;
    let next_requests = next.requests();
    next.shutdown().await;
    EarlyRequestOutcome {
        reader_claims_while_fill_waits,
        rest_of_body,
        next_response,
        next_requests,
        reader,
    }
}

/// A reader that completed its hit and was served its next request on the same connection.
fn early_request_reused_connection() -> EarlyRequestOutcome {
    EarlyRequestOutcome {
        reader_claims_while_fill_waits: 1,
        rest_of_body: vec![b"world!".to_vec(), Vec::new()],
        next_response: Some(("HTTP/1.1 200 OK".to_owned(), b"next".to_vec())),
        next_requests: 1,
        reader: ReaderOutcome::COMPLETED,
    }
}

// The reader of a stalled hit sends its next request early, with pipelining disabled. Without the
// opt-in the hit never reads it, completes, and the next request is served on the same
// connection. Opted in, the hit's watch reads its first byte, which ends the hit with the
// `ConnectError` of the regular HTTP/1 proxy loops while the fill still waits, releases the
// reader, and closes the connection. The client was still connected, so that error is logged as
// any failed hit's is, without an offer to `suppress_error_log`.
#[tokio::test]
async fn test_h1_early_next_request_during_stalled_hit_closes_connection_only_when_opted_in() {
    let (opted_out, opted_in) = opted_out_then_in(
        "/test_h1_early_next_request_during_stalled_hit_opted_out",
        "/test_h1_early_next_request_during_stalled_hit_opted_in",
        |path, handles| run_early_next_request_during_stalled_hit(path, handles, false),
    )
    .await;
    assert_eq!(opted_out, early_request_reused_connection());
    assert_eq!(
        opted_in,
        EarlyRequestOutcome {
            reader_claims_while_fill_waits: 0,
            rest_of_body: Vec::new(),
            next_response: None,
            next_requests: 0,
            reader: ReaderOutcome {
                error: Some(ErrorType::ConnectError),
                error_log_offers: 0,
                abandonments: 0,
            },
        }
    );
}

// The same with pipelining enabled. Opted in, the watch keeps the next request's first byte for
// the next session, as the regular HTTP/1 proxy loops do, so the hit completes and the next
// request is served on the same connection, as without the opt-in.
#[tokio::test]
async fn test_h1_early_next_request_during_stalled_hit_with_pipelining_reuses_connection_like_opt_out(
) {
    let (opted_out, opted_in) = opted_out_then_in(
        "/test_h1_early_next_request_with_pipelining_during_stalled_hit_opted_out",
        "/test_h1_early_next_request_with_pipelining_during_stalled_hit_opted_in",
        |path, handles| run_early_next_request_during_stalled_hit(path, handles, true),
    )
    .await;
    assert_eq!(opted_out, early_request_reused_connection());
    assert_eq!(opted_in, opted_out);
}

/// An HTTP/2 origin for one request. It sends a cacheable response head with a `Content-Length`
/// and that whole body at once, but ends the stream, with an empty DATA frame, only once released.
/// Until then the fill has the whole body but not its end, so a hit streaming it can complete its
/// response while its cache reader still waits for that end.
struct UnendedH2Origin {
    port: u16,
    release: oneshot::Sender<()>,
    task: tokio::task::JoinHandle<()>,
}

impl UnendedH2Origin {
    const BODY: &'static [u8] = b"hello world!";

    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (release, released) = oneshot::channel::<()>();
        let task = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut connection = h2::server::handshake(socket).await.unwrap();
            let (_request, mut respond) = connection.accept().await.unwrap().unwrap();
            // Keep driving the connection, so the frames below reach the proxy.
            tokio::spawn(async move { while let Some(Ok(_)) = connection.accept().await {} });
            let head = Response::builder()
                .header(header::CONTENT_LENGTH, Self::BODY.len())
                .header(header::CACHE_CONTROL, "max-age=60")
                .body(())
                .unwrap();
            let mut body = respond.send_response(head, false).unwrap();
            body.send_data(Bytes::from_static(Self::BODY), false)
                .unwrap();
            released.await.unwrap();
            body.send_data(Bytes::new(), true).unwrap();
        });
        Self {
            port,
            release,
            task,
        }
    }

    /// End the response's stream.
    async fn release(self) {
        self.release.send(()).unwrap();
        tokio::time::timeout(TEST_TIMEOUT, self.task)
            .await
            .expect("the origin did not end its response")
            .unwrap();
    }
}

/// What a reader that sent its next request once its response was complete observed.
#[derive(Debug, PartialEq)]
struct SequentialRequestOutcome {
    /// The next request's response, if it was served on the same connection.
    next_response: Option<(String, Vec<u8>)>,
    /// Requests the next request's origin received.
    next_requests: usize,
    reader: ReaderOutcome,
}

async fn run_next_request_after_complete_response_during_stalled_hit(
    path: &'static str,
    handles_abandonment: bool,
) -> SequentialRequestOutcome {
    init();
    let before = Before::of(path);
    let next = NextOrigin::start().await;
    let origin = UnendedH2Origin::start().await;
    let mut writer = connect().await;
    writer
        .write_all(h1_request("GET", path, origin.port, H2_UPSTREAM).as_bytes())
        .await
        .unwrap();
    let head = read_head(&mut writer).await.unwrap();
    assert_eq!(
        header_value(&head, "x-cache-status"),
        Some("miss"),
        "{head}"
    );
    let content_length = UnendedH2Origin::BODY.len();
    assert_eq!(
        read_n(&mut writer, content_length).await.unwrap(),
        UnendedH2Origin::BODY
    );

    let mut reader = connect().await;
    reader
        .write_all(
            h1_request(
                "GET",
                path,
                origin.port,
                &reader_headers(handles_abandonment),
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    let head = read_head(&mut reader).await.unwrap();
    assert_eq!(header_value(&head, "x-cache-status"), Some("hit"), "{head}");
    assert_eq!(
        header_value(&head, "content-length"),
        Some(content_length.to_string().as_str()),
        "{head}"
    );
    assert_eq!(
        read_n(&mut reader, content_length).await.unwrap(),
        UnendedH2Origin::BODY
    );
    // The response is complete, while the hit still waits for the end of the fill.
    reader
        .write_all(next.next_request(path).as_bytes())
        .await
        .unwrap();
    // Give a hit that wrongly still watches its client time to read that request before the fill
    // ends.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    origin.release().await;
    let next_response = NextOrigin::read_response(&mut reader).await;
    let reader = ReaderOutcome::wait(path, &before).await;
    let next_requests = next.requests();
    next.shutdown().await;
    SequentialRequestOutcome {
        next_response,
        next_requests,
        reader,
    }
}

// A reader that waits for its `Content-Length` response sends its next request once it has the
// whole body, which can be before its hit reads the end of the fill. That request belongs to the
// next session on the connection, so an opted-in hit stops watching once its response is complete
// and leaves it unread, even with pipelining disabled, where reading it would end the hit. The
// next request is served on the same connection, as without the opt-in.
#[tokio::test]
async fn test_h1_next_request_after_complete_response_during_stalled_hit_reuses_connection_like_opt_out(
) {
    let (opted_out, opted_in) = opted_out_then_in(
        "/test_h1_next_request_after_complete_response_during_stalled_hit_opted_out",
        "/test_h1_next_request_after_complete_response_during_stalled_hit_opted_in",
        run_next_request_after_complete_response_during_stalled_hit,
    )
    .await;
    assert_eq!(
        opted_out,
        SequentialRequestOutcome {
            next_response: Some(("HTTP/1.1 200 OK".to_owned(), b"next".to_vec())),
            next_requests: 1,
            reader: ReaderOutcome::COMPLETED,
        }
    );
    assert_eq!(opted_in, opted_out);
}

/// What a still connected reader observed when its fill's writer failed.
#[derive(Debug, PartialEq)]
struct ConnectedFailureOutcome {
    /// What the reader received after `hello `: `None` if the connection ended first.
    rest_of_body: Option<Vec<u8>>,
    reader: ReaderOutcome,
}

async fn run_hit_failure_with_connected_client(
    path: &'static str,
    handles_abandonment: bool,
) -> ConnectedFailureOutcome {
    init();
    let before = Before::of(path);
    let (origin, mut writer) = start_stalled_fill(path, FAIL_READERS_OF_DROPPED_WRITES).await;
    let mut reader = start_reader(
        path,
        origin.port,
        &format!(
            "{FAIL_READERS_OF_DROPPED_WRITES}{}",
            reader_headers(handles_abandonment)
        ),
    )
    .await;
    // The origin closes mid-body, so the writer's request fails, its unfinished write is
    // dropped, and the storage fails the reader's pending read.
    assert_eq!(origin.finish().await, OriginEnd::Released);
    assert_eq!(read_chunk(&mut writer).await, None);
    let reader_outcome = ReaderOutcome::wait(path, &before).await;
    ConnectedFailureOutcome {
        rest_of_body: read_chunk(&mut reader).await,
        reader: reader_outcome,
    }
}

// A hit whose storage read fails while its client is still connected logs that failure as it
// does without the opt-in: it is not offered to `suppress_error_log`, which only gets the
// failures the hit's client watch caused.
#[tokio::test]
async fn test_h1_stalled_hit_storage_failure_logs_like_opt_out() {
    let (opted_out, opted_in) = opted_out_then_in(
        "/test_h1_stalled_hit_storage_failure_opted_out",
        "/test_h1_stalled_hit_storage_failure_opted_in",
        run_hit_failure_with_connected_client,
    )
    .await;
    assert_eq!(
        opted_out,
        ConnectedFailureOutcome {
            rest_of_body: None,
            reader: ReaderOutcome {
                error: Some(ErrorType::InternalError),
                error_log_offers: 0,
                abandonments: 0,
            },
        }
    );
    assert_eq!(opted_in, opted_out);
}

/// Wait until the reader's hit has ended if its watch is expected to end it while the fill still
/// waits. Otherwise release `world!`, which only the hit's write to the client that left ends,
/// then wait.
async fn end_stalled_hit(
    path: &str,
    before: &Before,
    origin: &ScriptedOrigin,
    watched: bool,
) -> ReaderOutcome {
    if watched {
        // The fill still waits for its origin, so only the watch can have ended the hit.
        let outcome = ReaderOutcome::wait(path, before).await;
        origin.send(WORLD_CHUNK).await;
        outcome
    } else {
        origin.send(WORLD_CHUNK).await;
        ReaderOutcome::wait(path, before).await
    }
}

async fn run_h1_reset_during_stalled_hit(
    path: &'static str,
    handles_abandonment: bool,
) -> ReaderOutcome {
    init();
    let before = Before::of(path);
    let (origin, mut writer) = start_stalled_fill(path, "").await;
    let reader = start_reader(path, origin.port, &reader_headers(handles_abandonment)).await;
    reset(reader);
    let outcome = end_stalled_hit(path, &before, &origin, handles_abandonment).await;
    // The reader leaving does not disturb the fill.
    origin.send(LAST_CHUNK).await;
    let port = origin.port;
    assert_eq!(origin.finish().await, OriginEnd::Released);
    assert_eq!(read_chunk(&mut writer).await.unwrap(), b"world!");
    assert_eq!(read_chunk(&mut writer).await.unwrap(), b"");
    assert_cached(path, port).await;
    outcome
}

// An HTTP/1 client that resets its connection while its hit waits on a stalled fill. Opted in,
// the hit's watch ends the hit at once, with the read error, and offers that error to
// `suppress_error_log`. Without the opt-in the hit only fails once the fill resumes and it writes
// to the client, and the write error is logged without an offer.
#[tokio::test]
async fn test_h1_reset_ends_stalled_hit_only_when_opted_in() {
    let (opted_out, opted_in) = opted_out_then_in(
        "/test_h1_reset_ends_stalled_hit_opted_out",
        "/test_h1_reset_ends_stalled_hit_opted_in",
        run_h1_reset_during_stalled_hit,
    )
    .await;
    assert_eq!(
        opted_out,
        ReaderOutcome {
            error: Some(ErrorType::WriteError),
            error_log_offers: 0,
            abandonments: 0,
        }
    );
    assert_eq!(
        opted_in,
        ReaderOutcome {
            error: Some(ErrorType::ReadError),
            error_log_offers: 1,
            abandonments: 0,
        }
    );
}

/// What a reader that half-closed its connection during a stalled hit observed.
#[derive(Debug, PartialEq)]
struct HalfCloseOutcome {
    /// The reader's claims on the fill once its hit had the chance to end while the fill still
    /// waited for its origin.
    reader_claims_while_fill_waits: usize,
    /// The body chunks after `hello ` until the connection ended or the body was complete; the
    /// last chunk reads as an empty one.
    rest_of_body: Vec<Vec<u8>>,
    /// Whether the proxy then closed the connection.
    closed: bool,
    reader: ReaderOutcome,
}

async fn run_h1_half_close_during_stalled_hit(
    path: &'static str,
    handles_abandonment: bool,
    abort_on_close: bool,
) -> HalfCloseOutcome {
    init();
    let before = Before::of(path);
    let (origin, mut writer) = start_stalled_fill(path, DEMAND_COUNTING).await;
    let keep_half_closed = if abort_on_close { "" } else { KEEP_HALF_CLOSED };
    let mut reader = start_reader(
        path,
        origin.port,
        &format!(
            "{DEMAND_COUNTING}{keep_half_closed}{}",
            reader_headers(handles_abandonment)
        ),
    )
    .await;
    // A FIN, not a reset: the reader can still receive the rest of its response.
    reader.shutdown().await.unwrap();
    // The fill still waits for its origin, so only the watch can end the hit and release the
    // reader's claim. Wait for that where it is expected; otherwise give a hit that wrongly ends
    // on the half-close the time to do so.
    let patience = if handles_abandonment && abort_on_close {
        TEST_TIMEOUT
    } else {
        std::time::Duration::from_millis(200)
    };
    let reader_claims_while_fill_waits = reader_claims_within(path, patience).await;

    let mut rest_of_body = Vec::new();
    origin.send(WORLD_CHUNK).await;
    if let Some(chunk) = read_chunk(&mut reader).await {
        rest_of_body.push(chunk);
    }
    origin.send(LAST_CHUNK).await;
    let port = origin.port;
    assert_eq!(origin.finish().await, OriginEnd::Released);
    if !rest_of_body.is_empty() {
        if let Some(chunk) = read_chunk(&mut reader).await {
            rest_of_body.push(chunk);
        }
    }
    let closed = read_n(&mut reader, 1).await.is_none();
    // The reader's half-close does not disturb the fill.
    assert_eq!(read_chunk(&mut writer).await.unwrap(), b"world!");
    assert_eq!(read_chunk(&mut writer).await.unwrap(), b"");
    let reader = ReaderOutcome::wait(path, &before).await;
    assert_cached(path, port).await;
    HalfCloseOutcome {
        reader_claims_while_fill_waits,
        rest_of_body,
        closed,
        reader,
    }
}

/// A half-closed reader whose hit kept waiting on the fill and delivered the rest of its body,
/// after which the proxy closed the connection.
fn half_closed_reader_kept_response() -> HalfCloseOutcome {
    HalfCloseOutcome {
        reader_claims_while_fill_waits: 1,
        rest_of_body: vec![b"world!".to_vec(), Vec::new()],
        closed: true,
        reader: ReaderOutcome::COMPLETED,
    }
}

// An HTTP/1 client that half-closes its connection, with a FIN rather than a reset, while its hit
// waits on a stalled fill. The default `abort_on_close` treats that client as gone. Opted in, the
// hit's watch ends the hit at once with `ConnectionClosed`, offers that error to
// `suppress_error_log`, releases the reader's claim while the fill waits, and closes the
// connection. Without the opt-in the hit never reads the FIN: it delivers the rest of the body
// once the fill resumes and completes, and the connection then closes.
#[tokio::test]
async fn test_h1_half_close_ends_stalled_hit_only_when_opted_in() {
    let (opted_out, opted_in) = opted_out_then_in(
        "/test_h1_half_close_ends_stalled_hit_opted_out",
        "/test_h1_half_close_ends_stalled_hit_opted_in",
        |path, handles| run_h1_half_close_during_stalled_hit(path, handles, true),
    )
    .await;
    assert_eq!(opted_out, half_closed_reader_kept_response());
    assert_eq!(
        opted_in,
        HalfCloseOutcome {
            reader_claims_while_fill_waits: 0,
            rest_of_body: Vec::new(),
            closed: true,
            reader: ReaderOutcome {
                error: Some(ErrorType::ConnectionClosed),
                error_log_offers: 1,
                abandonments: 0,
            },
        }
    );
}

// The same with `abort_on_close` disabled, which keeps a half-closed client's response. Opted in,
// the hit's watch reads the FIN and keeps waiting on the fill, so the reader receives the whole
// body once the fill completes and the connection then closes, as without the opt-in.
#[tokio::test]
async fn test_h1_half_close_without_abort_on_close_keeps_stalled_hit_response_like_opt_out() {
    let (opted_out, opted_in) = opted_out_then_in(
        "/test_h1_half_close_without_abort_on_close_during_stalled_hit_opted_out",
        "/test_h1_half_close_without_abort_on_close_during_stalled_hit_opted_in",
        |path, handles| run_h1_half_close_during_stalled_hit(path, handles, false),
    )
    .await;
    assert_eq!(opted_out, half_closed_reader_kept_response());
    assert_eq!(opted_in, opted_out);
}

/// How an HTTP/2 reader's request ends.
#[derive(Clone, Copy)]
enum H2RequestEnd {
    /// END_STREAM on its HEADERS frame.
    Headers,
    /// END_STREAM on a separate, empty DATA frame, sent once the hit waits on the fill.
    EmptyData,
}

/// What an HTTP/2 reader does once it has `hello `.
#[derive(Clone, Copy)]
enum H2ReaderEvent {
    /// It resets its stream while the fill waits.
    Reset,
    /// It stays and reads the rest of the response.
    Stays,
}

/// What an HTTP/2 reader of a stalled hit observed.
#[derive(Debug, PartialEq)]
struct H2ReaderOutcome {
    /// What a reader that stayed received after `hello `, and whether its response ended
    /// cleanly.
    rest_of_body: Option<(Vec<u8>, bool)>,
    reader: ReaderOutcome,
}

async fn run_h2_stalled_hit(
    path: &'static str,
    handles_abandonment: bool,
    end: H2RequestEnd,
    event: H2ReaderEvent,
) -> H2ReaderOutcome {
    init();
    let before = Before::of(path);
    let probe = NextOrigin::start().await;
    let (origin, mut writer) = start_stalled_fill(path, "").await;

    let mut client = h2c_client(H2_PROXY_PORT).await;
    let request = Request::builder()
        .method("GET")
        .uri(format!("http://{HOST}{path}"))
        .header("x-port", origin.port.to_string())
        .header("x-lock", "true")
        .header("x-cache-lock-wait-timeout-ms", "10000")
        .header("x-record-request-lifecycle", "true");
    let request = if handles_abandonment {
        request.header(HANDLE_ABANDONMENT.0, HANDLE_ABANDONMENT.1)
    } else {
        request
    };
    let (response, mut stream) = client
        .send_request(
            request.body(()).unwrap(),
            matches!(end, H2RequestEnd::Headers),
        )
        .unwrap();
    let response = tokio::time::timeout(TEST_TIMEOUT, response)
        .await
        .expect("the reader's response header did not arrive")
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["x-cache-status"], "hit");
    let mut body = response.into_body();
    assert_eq!(read_h2_body_chunk(&mut body).await, b"hello ".as_slice());
    if matches!(end, H2RequestEnd::EmptyData) {
        // Only now, while the hit waits on the fill, does the request end.
        stream.send_data(Bytes::new(), true).unwrap();
    }

    // Waiting on an HTTP/2 stream reads nothing, so every opted-in HTTP/2 hit watches its client,
    // however its request ends.
    let watched = handles_abandonment;
    let (rest_of_body, reader) = match event {
        H2ReaderEvent::Reset => {
            stream.send_reset(h2::Reason::CANCEL);
            // The proxy answers a request sent after the reset on the same connection only
            // once it has processed the reset.
            let probe_request = Request::builder()
                .method("GET")
                .uri(format!("http://{HOST}{path}/probe"))
                .header("x-port", probe.port().to_string())
                .header("x-bypass-cache", "true")
                .body(())
                .unwrap();
            let (probe_response, _) = client.send_request(probe_request, true).unwrap();
            let probe_response = tokio::time::timeout(TEST_TIMEOUT, probe_response)
                .await
                .expect("the probe response did not arrive")
                .unwrap();
            assert_eq!(probe_response.status(), StatusCode::OK);
            (None, end_stalled_hit(path, &before, &origin, watched).await)
        }
        H2ReaderEvent::Stays => {
            origin.send(WORLD_CHUNK).await;
            let mut rest = read_h2_body_chunk(&mut body).await.to_vec();
            origin.send(LAST_CHUNK).await;
            let ended = loop {
                match tokio::time::timeout(TEST_TIMEOUT, body.data())
                    .await
                    .expect("the reader's response neither ended nor failed")
                {
                    Some(Ok(chunk)) => {
                        body.flow_control().release_capacity(chunk.len()).unwrap();
                        rest.extend_from_slice(&chunk);
                    }
                    Some(Err(_)) => break false,
                    None => break true,
                }
            };
            (
                Some((rest, ended)),
                ReaderOutcome::wait(path, &before).await,
            )
        }
    };
    if matches!(event, H2ReaderEvent::Reset) {
        origin.send(LAST_CHUNK).await;
    }
    let port = origin.port;
    assert_eq!(origin.finish().await, OriginEnd::Released);
    assert_eq!(read_chunk(&mut writer).await.unwrap(), b"world!");
    assert_eq!(read_chunk(&mut writer).await.unwrap(), b"");
    assert_cached(path, port).await;
    probe.shutdown().await;
    H2ReaderOutcome {
        rest_of_body,
        reader,
    }
}

// An HTTP/2 client that resets its stream while its hit waits on a stalled fill. Opted in, the
// hit's watch ends the hit at once and offers that error to `suppress_error_log`. Without the
// opt-in the hit only fails once the fill resumes and it writes to the reset stream, and that
// error is logged without an offer. Both are HTTP/2 stream errors.
#[tokio::test]
async fn test_h2_reset_ends_stalled_hit_only_when_opted_in() {
    let (opted_out, opted_in) = opted_out_then_in(
        "/test_h2_reset_ends_stalled_hit_opted_out",
        "/test_h2_reset_ends_stalled_hit_opted_in",
        |path, handles| {
            run_h2_stalled_hit(path, handles, H2RequestEnd::Headers, H2ReaderEvent::Reset)
        },
    )
    .await;
    assert_eq!(
        opted_out,
        H2ReaderOutcome {
            rest_of_body: None,
            reader: ReaderOutcome {
                error: Some(ErrorType::H2Error),
                error_log_offers: 0,
                abandonments: 0,
            },
        }
    );
    assert_eq!(
        opted_in,
        H2ReaderOutcome {
            rest_of_body: None,
            reader: ReaderOutcome {
                error: Some(ErrorType::H2Error),
                error_log_offers: 1,
                abandonments: 0,
            },
        }
    );
}

// An HTTP/2 GET whose END_STREAM arrives on a separate empty DATA frame only once its hit waits
// on the fill, so the request is still open when the hit starts. Waiting on its stream reads
// nothing, so an opted-in hit still watches it and a reset ends the hit at once, as for a
// request that ended on its HEADERS frame.
#[tokio::test]
async fn test_h2_reset_of_hit_ending_in_empty_data_frame_ends_it_only_when_opted_in() {
    let (opted_out, opted_in) = opted_out_then_in(
        "/test_h2_reset_of_hit_ending_in_empty_data_frame_opted_out",
        "/test_h2_reset_of_hit_ending_in_empty_data_frame_opted_in",
        |path, handles| {
            run_h2_stalled_hit(path, handles, H2RequestEnd::EmptyData, H2ReaderEvent::Reset)
        },
    )
    .await;
    assert_eq!(
        opted_out,
        H2ReaderOutcome {
            rest_of_body: None,
            reader: ReaderOutcome {
                error: Some(ErrorType::H2Error),
                error_log_offers: 0,
                abandonments: 0,
            },
        }
    );
    assert_eq!(
        opted_in,
        H2ReaderOutcome {
            rest_of_body: None,
            reader: ReaderOutcome {
                error: Some(ErrorType::H2Error),
                error_log_offers: 1,
                abandonments: 0,
            },
        }
    );
}

// The same GET, connected: the request's END_STREAM arriving during the watch neither ends the
// hit nor counts as the client leaving, and the hit delivers the whole entry once the fill
// completes, as without the opt-in.
#[tokio::test]
async fn test_h2_hit_ending_in_empty_data_frame_completes_like_opt_out() {
    let (opted_out, opted_in) = opted_out_then_in(
        "/test_h2_hit_ending_in_empty_data_frame_completes_opted_out",
        "/test_h2_hit_ending_in_empty_data_frame_completes_opted_in",
        |path, handles| {
            run_h2_stalled_hit(path, handles, H2RequestEnd::EmptyData, H2ReaderEvent::Stays)
        },
    )
    .await;
    assert_eq!(
        opted_out,
        H2ReaderOutcome {
            rest_of_body: Some((b"world!".to_vec(), true)),
            reader: ReaderOutcome::COMPLETED,
        }
    );
    assert_eq!(opted_in, opted_out);
}

/// A hit whose response is only a header.
#[derive(Clone, Copy)]
enum HeaderOnlyHit {
    Head,
    /// A conditional `GET` whose `If-None-Match` matches the entry.
    NotModified,
}

/// What a reader of a header-only hit observed.
#[derive(Debug, PartialEq)]
struct HeaderOnlyOutcome {
    status_line: String,
    cache_status: Option<String>,
    /// The next request's response on the same connection.
    next_response: Option<(String, Vec<u8>)>,
    reader: ReaderOutcome,
}

async fn run_header_only_hit_during_stalled_fill(
    path: &'static str,
    handles_abandonment: bool,
    kind: HeaderOnlyHit,
) -> HeaderOnlyOutcome {
    init();
    let before = Before::of(path);
    let next = NextOrigin::start().await;
    let (origin, writer) = start_stalled_fill(path, "").await;
    let (method, conditional) = match kind {
        HeaderOnlyHit::Head => ("HEAD", ""),
        HeaderOnlyHit::NotModified => ("GET", "If-None-Match: \"v1\"\r\n"),
    };
    let mut reader = connect().await;
    reader
        .write_all(
            h1_request(
                method,
                path,
                origin.port,
                &format!("{conditional}{}", reader_headers(handles_abandonment)),
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    // The response is complete with its header, while the fill still waits for its origin.
    let head = read_head(&mut reader).await.unwrap();
    let reader_outcome = ReaderOutcome::wait(path, &before).await;
    reader
        .write_all(next.next_request(path).as_bytes())
        .await
        .unwrap();
    let next_response = NextOrigin::read_response(&mut reader).await;
    // The client leaves after its complete responses, while the fill still waits.
    reset(reader);
    complete_fill(origin, writer).await;
    next.shutdown().await;
    assert_eq!(
        ReaderOutcome::recorded(path, &before),
        reader_outcome,
        "the client leaving after its complete response changed its request's outcome"
    );
    HeaderOnlyOutcome {
        status_line: head.lines().next().unwrap().to_owned(),
        cache_status: header_value(&head, "x-cache-status").map(str::to_owned),
        next_response,
        reader: reader_outcome,
    }
}

// A HEAD hit on a stalled fill is complete with its header, opted in or not: it never waits on
// the fill, its connection serves the next request, and the client leaving afterwards is not
// an abandonment.
#[tokio::test]
async fn test_h1_head_hit_during_stalled_fill_matches_opt_out() {
    let (opted_out, opted_in) = opted_out_then_in(
        "/test_h1_head_hit_during_stalled_fill_opted_out",
        "/test_h1_head_hit_during_stalled_fill_opted_in",
        |path, handles| run_header_only_hit_during_stalled_fill(path, handles, HeaderOnlyHit::Head),
    )
    .await;
    assert_eq!(
        opted_out,
        HeaderOnlyOutcome {
            status_line: "HTTP/1.1 200 OK".to_owned(),
            cache_status: Some("hit".to_owned()),
            next_response: Some(("HTTP/1.1 200 OK".to_owned(), b"next".to_vec())),
            reader: ReaderOutcome::COMPLETED,
        }
    );
    assert_eq!(opted_in, opted_out);
}

// The same for a conditional GET that the partly written entry answers with a 304.
#[tokio::test]
async fn test_h1_not_modified_hit_during_stalled_fill_matches_opt_out() {
    let (opted_out, opted_in) = opted_out_then_in(
        "/test_h1_not_modified_hit_during_stalled_fill_opted_out",
        "/test_h1_not_modified_hit_during_stalled_fill_opted_in",
        |path, handles| {
            run_header_only_hit_during_stalled_fill(path, handles, HeaderOnlyHit::NotModified)
        },
    )
    .await;
    assert_eq!(
        opted_out,
        HeaderOnlyOutcome {
            status_line: "HTTP/1.1 304 Not Modified".to_owned(),
            cache_status: Some("hit".to_owned()),
            next_response: Some(("HTTP/1.1 200 OK".to_owned(), b"next".to_vec())),
            reader: ReaderOutcome::COMPLETED,
        }
    );
    assert_eq!(opted_in, opted_out);
}

async fn run_reset_after_complete_hit(
    path: &'static str,
    handles_abandonment: bool,
) -> ReaderOutcome {
    init();
    let before = Before::of(path);
    let (origin, writer) = start_stalled_fill(path, "").await;
    let mut reader = start_reader(path, origin.port, &reader_headers(handles_abandonment)).await;
    let port = origin.port;
    complete_fill(origin, writer).await;
    assert_eq!(read_chunk(&mut reader).await.unwrap(), b"world!");
    assert_eq!(read_chunk(&mut reader).await.unwrap(), b"");
    let outcome = ReaderOutcome::wait(path, &before).await;
    reset(reader);
    assert_cached(path, port).await;
    assert_eq!(ReaderOutcome::recorded(path, &before), outcome);
    outcome
}

// A client that resets its connection once it has the hit's complete response leaves nothing
// to watch: the hit completed, opted in or not.
#[tokio::test]
async fn test_h1_reset_after_complete_hit_response_matches_opt_out() {
    let (opted_out, opted_in) = opted_out_then_in(
        "/test_h1_reset_after_complete_hit_response_opted_out",
        "/test_h1_reset_after_complete_hit_response_opted_in",
        run_reset_after_complete_hit,
    )
    .await;
    assert_eq!(opted_out, ReaderOutcome::COMPLETED);
    assert_eq!(opted_in, opted_out);
}

/// What the demand-counting storage observed for the latest fill of `path`.
fn fill(path: &str) -> DemandFillObservations {
    demand_fill_observations(&format!("{HOST}{path}"))
}

async fn wait_for_fill(
    path: &str,
    message: &str,
    condition: impl Fn(&DemandFillObservations) -> bool,
) {
    wait_until(&format!("{message} for {path}"), || condition(&fill(path))).await;
}

/// The reader claims on the fill of `path` once none is left, or after `patience` if one still
/// is.
async fn reader_claims_within(path: &str, patience: std::time::Duration) -> usize {
    let deadline = std::time::Instant::now() + patience;
    loop {
        let claims = fill(path).reader_claims;
        if claims == 0 || std::time::Instant::now() >= deadline {
            return claims;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

/// The fill after its last reader left and its next write failed.
#[derive(Debug, PartialEq)]
struct StoppedFillOutcome {
    /// Body writes the fill had taken when its last claim went.
    writes_when_stopped: usize,
    /// The fill once it ended.
    body_writes: usize,
    failed_writes: usize,
    finished: bool,
    /// The writer's final error.
    writer_error: Option<ErrorType>,
}

/// The opted-in writer's client leaves, its abandonment hook releases the writer's demand, and a
/// coalesced reader keeps the fill going. That reader then resets its connection while the fill
/// waits on its origin, and is opted in only if `reader_handles_abandonment`.
async fn run_last_reader_leaves_stalled_fill(
    path: &'static str,
    reader_handles_abandonment: bool,
) -> StoppedFillOutcome {
    init();
    let completed_before = recorded_request_completions(path);
    let abandoned_before = downstream_abandoned_during_cache_fill_calls(path);
    let (origin, writer) =
        start_stalled_fill(path, &format!("{DEMAND_COUNTING}{}", reader_headers(true))).await;
    wait_for_fill(path, "the fill did not take its first chunk", |fill| {
        fill.body_writes == 1
    })
    .await;
    let reader_extra = if reader_handles_abandonment {
        "X-Handle-Downstream-Abandonment: true\r\n"
    } else {
        ""
    };
    let mut reader = start_reader(
        path,
        origin.port,
        &format!("{DEMAND_COUNTING}{reader_extra}"),
    )
    .await;
    assert_eq!(fill(path).reader_claims, 1);

    reset(writer);
    wait_for_fill(
        path,
        "the writer's hook did not release its demand",
        |fill| !fill.writer_claim,
    )
    .await;
    assert_eq!(
        downstream_abandoned_during_cache_fill_calls(path),
        abandoned_before + 1
    );
    // The reader keeps the fill going.
    origin.send(WORLD_CHUNK).await;
    wait_for_fill(path, "the fill did not take its second chunk", |fill| {
        fill.body_writes == 2
    })
    .await;
    assert_eq!(read_chunk(&mut reader).await.unwrap(), b"world!");

    reset(reader);
    if reader_handles_abandonment {
        // The fill still waits for its origin, so only the hit's watch can release the claim.
        wait_for_fill(
            path,
            "the leaving reader did not release its claim",
            |fill| fill.reader_claims == 0,
        )
        .await;
    } else {
        // The hit notices only when it writes the next chunk to its client.
        origin.send(MORE_CHUNK).await;
        wait_for_fill(
            path,
            "the leaving reader did not release its claim after the next chunk",
            |fill| fill.body_writes == 3 && fill.reader_claims == 0,
        )
        .await;
    }
    let stopped = fill(path);
    assert!(stopped.stopped, "{stopped:?}");

    // Nobody wants the fill, so its next write fails and that ends the request.
    origin.send(REST_CHUNK).await;
    origin.wait_for_proxy_close().await;
    wait_for_recorded_request_completion(path, completed_before).await;
    let ended = fill(path);
    StoppedFillOutcome {
        writes_when_stopped: stopped.body_writes,
        body_writes: ended.body_writes,
        failed_writes: ended.failed_writes,
        finished: ended.finished,
        writer_error: recorded_request_error(path),
    }
}

// The goal of the hit watch. Once the writer's demand is gone, a coalesced reader holds the fill.
// Opted in, that reader leaving while the fill waits on its origin releases its claim at once, so
// the fill stops before it takes any more of the origin's body. Without the opt-in the reader's
// claim goes only when the fill resumes and the hit writes to the client that left, one chunk
// later. Either way the stopped fill's next write fails and ends the request.
#[tokio::test]
async fn test_h1_last_reader_leaving_stalled_fill_releases_claim_only_when_opted_in() {
    let (opted_out, opted_in) = opted_out_then_in(
        "/test_h1_last_reader_leaving_stalled_fill_opted_out",
        "/test_h1_last_reader_leaving_stalled_fill_opted_in",
        run_last_reader_leaves_stalled_fill,
    )
    .await;
    assert_eq!(
        opted_out,
        StoppedFillOutcome {
            writes_when_stopped: 3,
            body_writes: 3,
            failed_writes: 1,
            finished: false,
            writer_error: Some(ErrorType::FileWriteError),
        }
    );
    assert_eq!(
        opted_in,
        StoppedFillOutcome {
            writes_when_stopped: 2,
            body_writes: 2,
            failed_writes: 1,
            finished: false,
            writer_error: Some(ErrorType::FileWriteError),
        }
    );
}
