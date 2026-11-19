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

//! The caching proxy's behaviour when a client leaves, or sends its next request, while a response
//! or a cache fill is still in progress. Every request here uses only the default proxy
//! settings, so these tests describe what an application that configures nothing new observes.

use super::*;
use crate::utils::server_utils::{
    downstream_cache_warn_log_calls_for, recorded_cache_reason, recorded_request_completions,
    recorded_request_error, recorded_upstream_attempts,
};
use pingora_cache::NoCacheReason;
use pingora_error::ErrorType;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot};

#[path = "gate_off_contract/custom.rs"]
mod custom;
#[path = "gate_off_contract/h1_client_events.rs"]
mod h1_client_events;
#[path = "gate_off_contract/h1_early_request.rs"]
mod h1_early_request;
#[path = "gate_off_contract/h2_downstream.rs"]
mod h2_downstream;
#[path = "gate_off_contract/storage_errors.rs"]
mod storage_errors;

/// Bound for a wait that only a hang can exceed.
const TEST_TIMEOUT: Duration = Duration::from_secs(10);
const HOST: &str = "gate-off-contract.example";

/// Poll `condition` every 10 ms until it holds, failing with `message` after [`TEST_TIMEOUT`].
async fn wait_until(message: &str, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + TEST_TIMEOUT;
    while !condition() {
        assert!(Instant::now() < deadline, "{message}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Read an HTTP/1 header block one byte at a time, so no body byte is consumed.
async fn read_head(stream: &mut TcpStream) -> String {
    let mut head = Vec::new();
    while !head.ends_with(b"\r\n\r\n") {
        let mut byte = [0u8; 1];
        let read = tokio::time::timeout(TEST_TIMEOUT, stream.read(&mut byte))
            .await
            .unwrap_or_else(|_| panic!("header did not arrive, got {head:?}"))
            .unwrap();
        assert_eq!(read, 1, "connection ended inside the header: {head:?}");
        head.push(byte[0]);
    }
    String::from_utf8(head).unwrap()
}

async fn read_n(stream: &mut TcpStream, n: usize) -> Vec<u8> {
    let mut buf = vec![0u8; n];
    tokio::time::timeout(TEST_TIMEOUT, stream.read_exact(&mut buf))
        .await
        .expect("expected bytes did not arrive")
        .unwrap();
    buf
}

/// Everything the proxy still sends before it closes the connection.
async fn read_rest(stream: &mut TcpStream) -> Vec<u8> {
    let mut rest = Vec::new();
    // A reset instead of a clean close still ends the connection.
    let _ = tokio::time::timeout(TEST_TIMEOUT, stream.read_to_end(&mut rest))
        .await
        .expect("the proxy did not close the connection");
    rest
}

fn header_value<'a>(head: &'a str, name: &str) -> Option<&'a str> {
    head.lines().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        key.eq_ignore_ascii_case(name).then(|| value.trim())
    })
}

/// A request to the HTTP/1 cache proxy that records its lifecycle.
fn request(method: &str, path: &str, port: u16, extra: &str) -> String {
    format!(
        "{method} {path} HTTP/1.1\r\nHost: {HOST}\r\nX-Port: {port}\r\n\
         X-Record-Request-Lifecycle: true\r\nX-Lock: true\r\n{extra}\r\n"
    )
}

/// How a [`ScriptedOrigin`] connection ended.
#[derive(Debug, PartialEq)]
enum OriginEnd {
    /// The test finished the response and the origin closed the connection.
    Released,
    /// The proxy closed the connection before the test finished the response.
    ProxyClosed,
}

/// A follow-up request on a new connection, answered from cache: the origin behind `port` no
/// longer accepts anything, so a miss would fail.
async fn assert_cached(path: &str, port: u16, body: &[u8]) {
    let (head, cached) = fetch(path, port).await;
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    assert_eq!(header_value(&head, "x-cache-status"), Some("hit"), "{head}");
    assert_eq!(cached, body);
}

/// A `GET` for `path` on a new connection to the HTTP/1 cache proxy, with the decoded body.
async fn fetch(path: &str, port: u16) -> (String, Vec<u8>) {
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
    let head = read_head(&mut stream).await;
    let body = if is_chunked(&head) {
        read_chunked_body(&mut stream).await
    } else {
        read_rest(&mut stream).await
    };
    (head, body)
}

async fn connect() -> TcpStream {
    TcpStream::connect("127.0.0.1:6148").await.unwrap()
}

fn is_chunked(head: &str) -> bool {
    header_value(head, "transfer-encoding").is_some_and(|v| v.eq_ignore_ascii_case("chunked"))
}

/// Read one chunk of a chunked body; the last chunk (and the empty trailer section after it)
/// reads as an empty chunk.
async fn read_chunk(stream: &mut TcpStream) -> Vec<u8> {
    let mut size_line = Vec::new();
    while !size_line.ends_with(b"\r\n") {
        size_line.extend(read_n(stream, 1).await);
    }
    let size_line = std::str::from_utf8(&size_line).unwrap().trim_end();
    let size = usize::from_str_radix(size_line, 16).expect(size_line);
    let chunk = read_n(stream, size).await;
    assert_eq!(
        read_n(stream, 2).await,
        b"\r\n",
        "chunk of {size} bytes not terminated"
    );
    chunk
}

/// Read the rest of a chunked body, up to and including its last chunk.
async fn read_chunked_body(stream: &mut TcpStream) -> Vec<u8> {
    let mut body = Vec::new();
    loop {
        let chunk = read_chunk(stream).await;
        if chunk.is_empty() {
            return body;
        }
        body.extend(chunk);
    }
}

/// A cacheable response header with a chunked body, so the proxy flushes each part of the
/// response to the client as it arrives. (With `Content-Length` framing the proxy buffers a
/// small response until it is complete.)
const CHUNKED_HEADER: &[u8] =
    b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nCache-Control: max-age=60\r\n\r\n";
const HELLO_CHUNK: &[u8] = b"6\r\nhello \r\n";
const WORLD_CHUNK_AND_END: &[u8] = b"6\r\nworld!\r\n0\r\n\r\n";
const FULL_CHUNKED_RESPONSE: &[u8] = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\
    Cache-Control: max-age=60\r\n\r\n6\r\nhello \r\n6\r\nworld!\r\n0\r\n\r\n";

/// An HTTP/1 origin for one request whose response the test writes part by part.
struct ScriptedOrigin {
    port: u16,
    requested: oneshot::Receiver<String>,
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
            requested_tx.send(read_head(&mut socket).await).unwrap();
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

    /// The request header the proxy sent.
    async fn wait_for_request(&mut self) -> String {
        tokio::time::timeout(TEST_TIMEOUT, &mut self.requested)
            .await
            .expect("the origin did not get the request")
            .unwrap()
    }

    /// Write the next part of the response, once the request header has arrived.
    async fn send(&self, bytes: &'static [u8]) {
        let (written_tx, written) = oneshot::channel();
        self.writes.send((bytes, written_tx)).unwrap();
        let written = tokio::time::timeout(TEST_TIMEOUT, written)
            .await
            .expect("the origin did not write")
            .expect("the proxy closed the upstream connection first");
        assert!(written, "the origin could not write to the proxy");
    }

    /// Close the connection, after whatever the test has sent.
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
            .expect("the proxy kept the upstream connection open")
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

    /// A request for `{path}/next` that bypasses the cache, sent as the client's next request.
    fn next_request(&self, path: &str) -> String {
        format!(
            "GET {path}/next HTTP/1.1\r\nHost: {HOST}\r\nX-Port: {}\r\nX-Bypass-Cache: true\r\n\r\n",
            self.origin.addr().port()
        )
    }

    fn requests(&self) -> usize {
        self.requests.load(Ordering::Relaxed)
    }

    async fn shutdown(self) {
        self.origin.shutdown().await;
    }
}

/// What the proxy recorded for the instrumented requests of a path since a [`Baseline`].
#[derive(Debug, PartialEq)]
struct Recorded {
    /// The final error of the last request that completed.
    error: Option<ErrorType>,
    upstream_attempts: usize,
    ignored_downstream_errors: usize,
    /// Why the last request that completed ended with its cache disabled, if it did.
    disabled_cache: Option<NoCacheReason>,
}

/// Counts for a path, taken before a test sends its requests.
struct Baseline {
    path: String,
    completions: usize,
    ignored: usize,
    attempts: usize,
}

impl Baseline {
    fn new(path: &str) -> Self {
        Self {
            path: path.to_owned(),
            completions: recorded_request_completions(path),
            ignored: downstream_cache_warn_log_calls_for(path),
            attempts: recorded_upstream_attempts(path),
        }
    }

    /// Wait until the proxy has reported `n` ignored downstream errors since the baseline.
    async fn wait_for_ignored_errors(&self, n: usize) {
        wait_until(
            &format!(
                "the proxy did not ignore a downstream error for {}",
                self.path
            ),
            || downstream_cache_warn_log_calls_for(&self.path) >= self.ignored + n,
        )
        .await;
        assert_eq!(
            downstream_cache_warn_log_calls_for(&self.path),
            self.ignored + n
        );
    }

    /// Wait until `n` instrumented requests completed since the baseline, then return what the
    /// proxy recorded.
    async fn wait_for_completions(&self, n: usize) -> Recorded {
        wait_until(
            &format!("the request for {} did not complete", self.path),
            || recorded_request_completions(&self.path) >= self.completions + n,
        )
        .await;
        assert_eq!(
            recorded_request_completions(&self.path),
            self.completions + n
        );
        self.recorded()
    }

    fn recorded(&self) -> Recorded {
        Recorded {
            error: recorded_request_error(&self.path),
            upstream_attempts: recorded_upstream_attempts(&self.path) - self.attempts,
            ignored_downstream_errors: downstream_cache_warn_log_calls_for(&self.path)
                - self.ignored,
            disabled_cache: recorded_cache_reason(&self.path),
        }
    }
}

/// A request whose fill succeeded without the client, and nothing else to note.
const FILLED_AFTER_IGNORED_ERROR: Recorded = Recorded {
    error: None,
    upstream_attempts: 1,
    ignored_downstream_errors: 1,
    disabled_cache: None,
};

/// A HEAD miss is complete once its header is written. A client that then closes while the fill
/// still reads the body is reported as an ignored downstream error and the fill completes.
#[tokio::test]
async fn h1_head_miss_client_close_after_complete_response_keeps_filling() {
    init();
    let path = "/gate_off/h1_head_miss_client_close_after_complete_response";
    let baseline = Baseline::new(path);
    let mut origin = ScriptedOrigin::start().await;
    let mut client = connect().await;
    client
        .write_all(request("HEAD", path, origin.port, "").as_bytes())
        .await
        .unwrap();
    let upstream_request = origin.wait_for_request().await;
    assert!(
        upstream_request.starts_with(&format!("GET {path} ")),
        "{upstream_request}"
    );
    origin
        .send(b"HTTP/1.1 200 OK\r\nContent-Length: 12\r\nCache-Control: max-age=60\r\n\r\nhello ")
        .await;
    let head = read_head(&mut client).await;
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    drop(client);
    baseline.wait_for_ignored_errors(1).await;
    origin.send(b"world!").await;
    let port = origin.port;
    assert_eq!(origin.finish().await, OriginEnd::Released);
    assert_eq!(baseline.wait_for_completions(1).await.error, None);
    assert_cached(path, port, b"hello world!").await;
}
