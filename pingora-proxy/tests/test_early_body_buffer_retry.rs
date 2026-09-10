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

//! Regression coverage for early request body buffering across an upstream retry.
//!
//! `buffer_request_body_early()` drains the downstream body before
//! `enable_retry_buffering()`, so the retry buffer never sees those bytes and the proxy must
//! replay the buffered copy itself. These tests use self-contained H1 and H2 origins to cover
//! retry replay, protocol completion, local `100 Continue` handling, and total buffering
//! deadlines.

#![cfg(feature = "early_body_buffer")]

use async_trait::async_trait;
use bytes::{Bytes, BytesMut};
use pingora_core::apps::HttpServerOptions;
use pingora_core::prelude::HttpPeer;
use pingora_core::protocols::ALPN;
use pingora_core::server::configuration::ServerConf;

use pingora_error::{Error, ErrorSource, ErrorType, Result};
use pingora_proxy::{FailToProxy, ProxyHttp, Session};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// Per-harness rather than global so these tests stay independent under parallel runs.
#[derive(Default)]
struct Recorder {
    /// (head, decoded body) per request, in arrival order.
    requests: Mutex<Vec<(String, String)>>,
}

impl Recorder {
    fn bodies(&self) -> Vec<String> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .map(|(_, b)| b.clone())
            .collect()
    }

    fn heads(&self) -> Vec<String> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .map(|(h, _)| h.clone())
            .collect()
    }
}

/// The bug sends `Content-Length: N` with no body, so waiting forever would hang instead of
/// reporting a short read.
const BODY_READ_TIMEOUT: Duration = Duration::from_millis(600);

async fn read_one_request(stream: &mut TcpStream, recorder: &Recorder) -> Option<()> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];

    let head_end = loop {
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos + 4;
        }
        let n = stream.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
    };

    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
    let head_lower = head.to_ascii_lowercase();

    let content_length = head_lower
        .lines()
        .find_map(|l| l.strip_prefix("content-length:"))
        .and_then(|v| v.trim().parse::<usize>().ok());
    let chunked = head_lower
        .lines()
        .any(|l| l.starts_with("transfer-encoding:") && l.contains("chunked"));

    let mut body_raw = buf[head_end..].to_vec();

    let body = if let Some(len) = content_length {
        while body_raw.len() < len {
            match tokio::time::timeout(BODY_READ_TIMEOUT, stream.read(&mut chunk)).await {
                Ok(Ok(0)) | Err(_) => break,
                Ok(Ok(n)) => body_raw.extend_from_slice(&chunk[..n]),
                Ok(Err(_)) => return None,
            }
        }
        String::from_utf8_lossy(&body_raw[..len.min(body_raw.len())]).to_string()
    } else if chunked {
        while !body_raw.windows(5).any(|w| w == b"0\r\n\r\n") {
            match tokio::time::timeout(BODY_READ_TIMEOUT, stream.read(&mut chunk)).await {
                Ok(Ok(0)) | Err(_) => break,
                Ok(Ok(n)) => body_raw.extend_from_slice(&chunk[..n]),
                Ok(Err(_)) => return None,
            }
        }
        dechunk(&body_raw)
    } else {
        String::new()
    };

    recorder.requests.lock().unwrap().push((head, body));
    Some(())
}

fn dechunk(raw: &[u8]) -> String {
    let mut out = Vec::new();
    let mut rest = raw;
    while let Some(pos) = rest.windows(2).position(|w| w == b"\r\n") {
        let size = usize::from_str_radix(
            String::from_utf8_lossy(&rest[..pos])
                .trim()
                .split(';')
                .next()
                .unwrap_or("0"),
            16,
        )
        .unwrap_or(0);
        if size == 0 {
            break;
        }
        let data_start = pos + 2;
        let data_end = (data_start + size).min(rest.len());
        out.extend_from_slice(&rest[data_start..data_end]);
        rest = &rest[(data_end + 2).min(rest.len())..];
    }
    String::from_utf8_lossy(&out).to_string()
}

/// With `fail_first`, the first connection answers request #1 so the proxy pools it, then
/// abandons request #2. That shape is required: the failure must land after connect (a connect
/// failure never reaches the code that forwards the buffered body) and on a reused connection
/// (`error_while_proxy` only retries when `decide_reuse(client_reused)` holds). The connection
/// must also stay open while idle, or the pool discards it on the FIN instead of retrying.
async fn spawn_origin(recorder: Arc<Recorder>, fail_first: bool) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let first_conn = Arc::new(AtomicBool::new(true));

    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            let recorder = recorder.clone();
            let may_poison = fail_first && first_conn.swap(false, Ordering::SeqCst);
            tokio::spawn(async move {
                let mut seen = 0usize;
                loop {
                    if read_one_request(&mut stream, &recorder).await.is_none() {
                        return;
                    }
                    seen += 1;
                    if may_poison && seen == 2 {
                        let _ = stream.shutdown().await;
                        return;
                    }
                    let _ = stream
                        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\n\r\nok\n")
                        .await;
                    let _ = stream.flush().await;
                }
            });
        }
    });

    port
}

async fn spawn_h2_origin(recorder: Arc<Recorder>) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();

    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let recorder = recorder.clone();
            tokio::spawn(async move {
                let Ok(mut connection) = h2::server::handshake(stream).await else {
                    return;
                };
                while let Some(Ok((request, mut respond))) = connection.accept().await {
                    let recorder = recorder.clone();
                    tokio::spawn(async move {
                        let head = format!("{:?}", request.headers());
                        let mut body = request.into_body();
                        let mut body_bytes = BytesMut::new();

                        let body_result = tokio::time::timeout(Duration::from_secs(3), async {
                            while let Some(chunk) = body.data().await {
                                body_bytes.extend_from_slice(&chunk?);
                            }
                            Ok::<(), h2::Error>(())
                        })
                        .await;
                        if !matches!(body_result, Ok(Ok(()))) {
                            return;
                        }

                        recorder
                            .requests
                            .lock()
                            .unwrap()
                            .push((head, String::from_utf8_lossy(&body_bytes).into_owned()));

                        let response = http::Response::builder().status(200).body(()).unwrap();
                        let Ok(mut response_body) = respond.send_response(response, false) else {
                            return;
                        };
                        let _ = response_body.send_data(Bytes::from_static(b"ok\n"), true);
                    });
                }
            });
        }
    });

    port
}

struct EarlyBufferProxy {
    origin_port: u16,
    limit: usize,
    buffer_in_request_filter: bool,
    buffer_timeout: Option<Duration>,
    drop_body_in_early_filter: bool,
    replacement_body: Option<&'static str>,
    body_filter_calls: Arc<AtomicUsize>,
    upstream_filter_expect_seen: Arc<AtomicBool>,
    use_h2_upstream: bool,
    max_attempts: Arc<AtomicUsize>,
}

/// Attempts are counted per request, not globally, so a retry in one request cannot mask the
/// absence of a retry in the next.
#[derive(Default)]
struct Ctx {
    attempt: usize,
}

#[async_trait]
impl ProxyHttp for EarlyBufferProxy {
    type CTX = Ctx;
    fn new_ctx(&self) -> Ctx {
        Ctx::default()
    }

    fn early_request_body_buffer_limit(&self, _session: &Session, _ctx: &Ctx) -> Option<usize> {
        (!self.buffer_in_request_filter).then_some(self.limit)
    }

    fn early_request_body_buffer_timeout(
        &self,
        _session: &Session,
        _ctx: &Ctx,
    ) -> Option<Duration> {
        self.buffer_timeout
    }

    async fn early_request_body_filter(
        &self,
        session: &mut Session,
        body: &mut Option<Bytes>,
        _end_of_stream: bool,
        _ctx: &mut Ctx,
    ) -> Result<()> {
        if self.drop_body_in_early_filter {
            *body = None;
            let req = session.downstream_session.req_header_mut();
            req.remove_header(&http::header::CONTENT_LENGTH);
            req.remove_header(&http::header::TRANSFER_ENCODING);
        }
        Ok(())
    }

    async fn request_filter(&self, session: &mut Session, _ctx: &mut Ctx) -> Result<bool> {
        if let Some(replacement) = self.replacement_body {
            session.set_buffered_body(Some(Bytes::from_static(replacement.as_bytes())));
            session
                .req_header_mut()
                .insert_header(http::header::CONTENT_LENGTH, replacement.len().to_string())?;
            return Ok(false);
        }

        if !self.buffer_in_request_filter {
            return Ok(false);
        }

        let mut body = BytesMut::new();
        while let Some(chunk) = session.downstream_session.read_request_body().await? {
            body.extend_from_slice(&chunk);
            if session.downstream_session.is_body_done() {
                break;
            }
        }
        session.set_buffered_body(Some(body.freeze()));
        Ok(false)
    }

    async fn request_body_filter(
        &self,
        _session: &mut Session,
        body: &mut Option<Bytes>,
        _end_of_stream: bool,
        _ctx: &mut Ctx,
    ) -> Result<()> {
        if body.is_some() {
            self.body_filter_calls.fetch_add(1, Ordering::SeqCst);
        }
        Ok(())
    }

    async fn upstream_request_filter(
        &self,
        _session: &mut Session,
        upstream_request: &mut pingora_http::RequestHeader,
        _ctx: &mut Ctx,
    ) -> Result<()> {
        if upstream_request.headers.contains_key(http::header::EXPECT) {
            self.upstream_filter_expect_seen
                .store(true, Ordering::SeqCst);
        }
        Ok(())
    }

    async fn upstream_peer(&self, _session: &mut Session, ctx: &mut Ctx) -> Result<Box<HttpPeer>> {
        ctx.attempt += 1;
        self.max_attempts.fetch_max(ctx.attempt, Ordering::SeqCst);

        let mut peer = Box::new(HttpPeer::new(
            format!("127.0.0.1:{}", self.origin_port),
            false,
            String::new(),
        ));
        if self.use_h2_upstream {
            peer.options.alpn = ALPN::H2;
        }
        // Pool connections so the proxy reuses one instead of dialing fresh every attempt.
        peer.options.idle_timeout = Some(Duration::from_secs(60));
        Ok(peer)
    }

    async fn fail_to_proxy(
        &self,
        session: &mut Session,
        error: &Error,
        _ctx: &mut Ctx,
    ) -> FailToProxy {
        let error_code = match error.etype() {
            ErrorType::HTTPStatus(code) => *code,
            ErrorType::ReadTimedout if error.esource() == &ErrorSource::Downstream => 408,
            _ => 500,
        };
        session.respond_error(error_code).await.unwrap();
        FailToProxy {
            error_code,
            can_reuse_downstream: false,
        }
    }
}

struct Harness {
    proxy_port: u16,
    body_filter_calls: Arc<AtomicUsize>,
    max_attempts: Arc<AtomicUsize>,
    recorder: Arc<Recorder>,
    upstream_filter_expect_seen: Arc<AtomicBool>,
}

async fn start_harness(limit: usize, fail_first: bool) -> Harness {
    start_harness_with_config(
        limit,
        HarnessConfig {
            fail_first,
            ..HarnessConfig::default()
        },
    )
    .await
}

async fn start_harness_with_mode(
    limit: usize,
    fail_first: bool,
    buffer_in_request_filter: bool,
) -> Harness {
    start_harness_with_config(
        limit,
        HarnessConfig {
            fail_first,
            buffer_in_request_filter,
            ..HarnessConfig::default()
        },
    )
    .await
}

#[derive(Default)]
struct HarnessConfig {
    buffer_in_request_filter: bool,
    buffer_timeout: Option<Duration>,
    drop_body_in_early_filter: bool,
    replacement_body: Option<&'static str>,
    fail_first: bool,
    use_h2_downstream: bool,
    use_h2_upstream: bool,
}

async fn start_harness_with_config(limit: usize, config: HarnessConfig) -> Harness {
    let recorder = Arc::new(Recorder::default());
    let origin_port = if config.use_h2_upstream {
        spawn_h2_origin(recorder.clone()).await
    } else {
        spawn_origin(recorder.clone(), config.fail_first).await
    };
    let body_filter_calls = Arc::new(AtomicUsize::new(0));
    let max_attempts = Arc::new(AtomicUsize::new(0));
    let upstream_filter_expect_seen = Arc::new(AtomicBool::new(false));

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_port = listener.local_addr().unwrap().port();
    drop(listener);

    let app_attempts = max_attempts.clone();
    let app_body_filter_calls = body_filter_calls.clone();
    let app_expect_seen = upstream_filter_expect_seen.clone();
    std::thread::spawn(move || {
        let mut server = pingora_core::server::Server::new(None).unwrap();
        server.bootstrap();
        let conf = Arc::new(ServerConf::default());
        let mut service = pingora_proxy::http_proxy_service(
            &conf,
            EarlyBufferProxy {
                origin_port,
                limit,
                buffer_in_request_filter: config.buffer_in_request_filter,
                buffer_timeout: config.buffer_timeout,
                drop_body_in_early_filter: config.drop_body_in_early_filter,
                replacement_body: config.replacement_body,
                body_filter_calls: app_body_filter_calls,
                upstream_filter_expect_seen: app_expect_seen,
                use_h2_upstream: config.use_h2_upstream,
                max_attempts: app_attempts,
            },
        );
        if config.use_h2_downstream {
            let mut options = HttpServerOptions::default();
            options.h2c = true;
            service.app_logic_mut().unwrap().server_options = Some(options);
        }
        service.add_tcp(&format!("127.0.0.1:{proxy_port}"));
        server.add_service(service);
        server.run_forever();
    });

    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let mut listening = false;
    while std::time::Instant::now() < deadline {
        if TcpStream::connect(("127.0.0.1", proxy_port)).await.is_ok() {
            listening = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        listening,
        "proxy did not start listening on 127.0.0.1:{proxy_port} within 10s"
    );

    Harness {
        proxy_port,
        body_filter_calls,
        max_attempts,
        recorder,
        upstream_filter_expect_seen,
    }
}

/// Applications may need header-phase policy checks before buffering, so they can leave
/// `early_request_body_buffer_limit()` disabled, consume the body in `request_filter()`, and
/// provide the verified body through `Session::set_buffered_body()`. That application-supplied
/// body must follow the same forwarding, filtering, and retry path as an automatically buffered
/// body.
#[tokio::test]
async fn application_buffered_body_is_filtered_and_survives_upstream_retry() {
    let harness = start_harness_with_mode(64 * 1024, true, true).await;
    let body = "application-managed-buffer";

    assert_eq!(
        post(harness.proxy_port, body).await,
        "200",
        "priming request should succeed"
    );
    let attempts_after_priming = harness.max_attempts.load(Ordering::SeqCst);

    assert_eq!(
        post(harness.proxy_port, body).await,
        "200",
        "retried request should still succeed"
    );

    let attempts = harness.max_attempts.load(Ordering::SeqCst);
    assert!(
        attempts > attempts_after_priming,
        "test setup failed to force a retry: max attempts per request stayed at {attempts}"
    );
    assert_all_requests_carried(&harness.recorder, body);
    assert_eq!(
        harness.body_filter_calls.load(Ordering::SeqCst),
        harness.recorder.bodies().len(),
        "request_body_filter must run once for each forwarding attempt"
    );
}

/// These tests break upstream connections on purpose; bound the client so a regression into
/// never responding fails instead of hanging CI.
const CLIENT_TIMEOUT: Duration = Duration::from_secs(15);

async fn post(port: u16, body: &str) -> String {
    let client = reqwest::Client::builder()
        .pool_max_idle_per_host(0)
        .timeout(CLIENT_TIMEOUT)
        .build()
        .unwrap();
    let res = client
        .post(format!("http://127.0.0.1:{port}/echo"))
        .body(body.to_string())
        .send()
        .await
        .expect("proxy did not answer the request");
    format!("{}", res.status().as_u16())
}

/// Hand-rolled because reqwest needs its `stream` feature to send a chunked request.
async fn post_chunked(port: u16, body: &str) -> String {
    tokio::time::timeout(CLIENT_TIMEOUT, async {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let req = format!(
            "POST /echo HTTP/1.1\r\nHost: 127.0.0.1\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n{}\r\n0\r\n\r\n",
            body.len(),
            body
        );
        stream.write_all(req.as_bytes()).await.unwrap();
        stream.flush().await.unwrap();

        let mut resp = Vec::new();
        let mut chunk = [0u8; 4096];
        while !resp.windows(2).any(|w| w == b"\r\n") {
            let n = stream.read(&mut chunk).await.unwrap();
            if n == 0 {
                break;
            }
            resp.extend_from_slice(&chunk[..n]);
        }
        String::from_utf8_lossy(&resp)
            .split_whitespace()
            .nth(1)
            .unwrap_or("000")
            .to_string()
    })
    .await
    .expect("proxy did not answer the chunked request")
}

async fn read_response_head(stream: &mut TcpStream) -> String {
    let mut response = Vec::new();
    let mut chunk = [0u8; 4096];
    while !response.windows(4).any(|w| w == b"\r\n\r\n") {
        let n = stream.read(&mut chunk).await.unwrap();
        if n == 0 {
            break;
        }
        response.extend_from_slice(&chunk[..n]);
    }
    String::from_utf8_lossy(&response).into_owned()
}

async fn post_empty(port: u16) -> String {
    tokio::time::timeout(CLIENT_TIMEOUT, async {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        stream
            .write_all(b"POST /echo HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: 0\r\n\r\n")
            .await
            .unwrap();
        stream.flush().await.unwrap();

        read_response_head(&mut stream)
            .await
            .split_whitespace()
            .nth(1)
            .unwrap_or("000")
            .to_string()
    })
    .await
    .expect("proxy did not answer the empty request")
}

async fn post_empty_h2(port: u16) -> String {
    tokio::time::timeout(CLIENT_TIMEOUT, async {
        let tcp = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let (client, connection) = h2::client::handshake(tcp).await.unwrap();
        tokio::spawn(async move {
            let _ = connection.await;
        });

        let request = http::Request::builder()
            .method("POST")
            .uri(format!("http://127.0.0.1:{port}/echo"))
            .header(http::header::CONTENT_LENGTH, "0")
            .body(())
            .unwrap();
        let mut client = client.ready().await.unwrap();
        let (response, _) = client.send_request(request, true).unwrap();
        response.await.unwrap().status().as_u16().to_string()
    })
    .await
    .expect("proxy did not answer the empty H2 request")
}

async fn post_with_expect_continue(port: u16, body: &str) -> (String, String) {
    tokio::time::timeout(CLIENT_TIMEOUT, async {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let head = format!(
            "POST /echo HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: {}\r\nExpect: 100-continue\r\n\r\n",
            body.len()
        );
        stream.write_all(head.as_bytes()).await.unwrap();
        stream.flush().await.unwrap();

        let interim = read_response_head(&mut stream).await;
        stream.write_all(body.as_bytes()).await.unwrap();
        stream.flush().await.unwrap();
        let final_response = read_response_head(&mut stream).await;
        (interim, final_response)
    })
    .await
    .expect("proxy did not complete the Expect: 100-continue exchange")
}

async fn post_slow_body(port: u16) -> String {
    tokio::time::timeout(CLIENT_TIMEOUT, async {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        stream
            .write_all(b"POST /echo HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: 2\r\n\r\na")
            .await
            .unwrap();
        stream.flush().await.unwrap();

        tokio::time::sleep(Duration::from_millis(200)).await;
        let _ = stream.write_all(b"b").await;
        read_response_head(&mut stream)
            .await
            .split_whitespace()
            .nth(1)
            .unwrap_or("000")
            .to_string()
    })
    .await
    .expect("proxy did not enforce the total buffering timeout")
}

/// Asserts every recorded request carried `body` under framing that matches it. The
/// Content-Length check is not redundant: the origin stops at a short read, so an inflated
/// Content-Length still records the full payload and would pass the body check alone.
fn assert_all_requests_carried(recorder: &Recorder, body: &str) {
    let bodies = recorder.bodies();
    assert!(
        bodies.len() >= 3,
        "expected priming + failed attempt + retry, saw {bodies:?}"
    );
    for (i, seen) in bodies.iter().enumerate() {
        assert_eq!(
            seen, body,
            "origin request #{i} lost the early-buffered body; origin saw {bodies:?}"
        );
    }
    for (i, head) in recorder.heads().iter().enumerate() {
        if let Some(cl) = head
            .to_ascii_lowercase()
            .lines()
            .find_map(|l| l.strip_prefix("content-length:"))
            .and_then(|v| v.trim().parse::<usize>().ok())
        {
            assert_eq!(
                cl,
                body.len(),
                "origin request #{i} Content-Length disagrees with the body sent; head:\n{head}"
            );
        }
    }
}

/// The first POST pools an upstream connection; the second reuses it, the origin abandons it
/// mid-exchange, and the retry that follows must still carry the buffered body.
#[tokio::test]
async fn early_buffered_body_survives_upstream_retry() {
    let harness = start_harness(64 * 1024, true).await;
    let body = "cody-early-buffer-payload";

    assert_eq!(
        post(harness.proxy_port, body).await,
        "200",
        "priming request should succeed"
    );
    let attempts_after_priming = harness.max_attempts.load(Ordering::SeqCst);

    let status = post(harness.proxy_port, body).await;

    let attempts = harness.max_attempts.load(Ordering::SeqCst);
    assert!(
        attempts > attempts_after_priming,
        "test setup failed to force a retry: max attempts per request stayed at {attempts}"
    );
    assert_eq!(status, "200", "retried request should still succeed");

    assert_all_requests_carried(&harness.recorder, body);
}

/// Same, for a chunked body — the framing the early buffer has to reconstruct itself.
#[tokio::test]
async fn early_buffered_chunked_body_survives_upstream_retry() {
    let harness = start_harness(64 * 1024, true).await;
    let body = "chunked-payload-across-retry";

    assert_eq!(
        post_chunked(harness.proxy_port, body).await,
        "200",
        "priming request should succeed"
    );
    let attempts_after_priming = harness.max_attempts.load(Ordering::SeqCst);

    assert_eq!(
        post_chunked(harness.proxy_port, body).await,
        "200",
        "retried request should still succeed"
    );

    let attempts = harness.max_attempts.load(Ordering::SeqCst);
    assert!(
        attempts > attempts_after_priming,
        "test setup failed to force a retry: max attempts per request stayed at {attempts}"
    );

    assert_all_requests_carried(&harness.recorder, body);
}

/// Guards against "fixing" the retry by dropping the size limit.
#[tokio::test]
async fn oversized_body_is_rejected_not_silently_emptied() {
    let harness = start_harness(8, false).await;
    let status = post(
        harness.proxy_port,
        "this body is definitely longer than 8 bytes",
    )
    .await;
    assert_eq!(
        status, "413",
        "body over the early-buffer limit should be rejected with 413"
    );
    assert!(
        harness.recorder.bodies().is_empty(),
        "rejected request must never reach the origin, saw {:?}",
        harness.recorder.bodies()
    );
}

/// Baseline: replaying on retry must not double-send when there is no retry.
#[tokio::test]
async fn buffered_body_forwarded_once_without_retry() {
    let harness = start_harness(64 * 1024, false).await;
    let body = "single-attempt-body";

    assert_eq!(post(harness.proxy_port, body).await, "200");
    assert_eq!(
        harness.max_attempts.load(Ordering::SeqCst),
        1,
        "expected no retry"
    );

    let bodies = harness.recorder.bodies();
    assert_eq!(bodies, vec![body.to_string()], "body should be sent once");
}

#[tokio::test]
async fn expect_continue_is_answered_locally_and_not_forwarded_to_h1_or_h2() {
    let body = "continue-after-interim";

    for use_h2_upstream in [false, true] {
        let harness = start_harness_with_config(
            64 * 1024,
            HarnessConfig {
                use_h2_upstream,
                ..HarnessConfig::default()
            },
        )
        .await;
        let protocol = if use_h2_upstream { "H2" } else { "H1" };

        let (interim, final_response) = post_with_expect_continue(harness.proxy_port, body).await;

        assert!(
            interim.starts_with("HTTP/1.1 100 Continue"),
            "expected local 100 Continue before {protocol} forwarding, got: {interim}"
        );
        assert!(
            final_response.starts_with("HTTP/1.1 200"),
            "expected final 200 response from {protocol} origin, got: {final_response}"
        );
        assert!(
            harness.upstream_filter_expect_seen.load(Ordering::SeqCst),
            "upstream_request_filter should see Expect before {protocol} proxy cleanup"
        );
        assert_eq!(harness.recorder.bodies(), vec![body.to_string()]);
        assert!(
            harness
                .recorder
                .heads()
                .iter()
                .all(|head| !head.to_ascii_lowercase().contains("expect")),
            "Expect must not be forwarded to the {protocol} origin after buffering"
        );
    }
}

#[tokio::test]
async fn total_buffering_timeout_bounds_slow_request_body() {
    let harness = start_harness_with_config(
        64 * 1024,
        HarnessConfig {
            buffer_timeout: Some(Duration::from_millis(100)),
            ..HarnessConfig::default()
        },
    )
    .await;

    assert_eq!(post_slow_body(harness.proxy_port).await, "408");
    assert!(
        harness.recorder.bodies().is_empty(),
        "timed-out request must not reach the origin"
    );
}

#[tokio::test]
async fn filtered_empty_body_ends_h2_upstream_stream() {
    let harness = start_harness_with_config(
        64 * 1024,
        HarnessConfig {
            drop_body_in_early_filter: true,
            use_h2_upstream: true,
            ..HarnessConfig::default()
        },
    )
    .await;

    assert_eq!(post(harness.proxy_port, "removed-body").await, "200");
    assert_eq!(
        harness.recorder.bodies(),
        vec![String::new()],
        "H2 origin should observe a completed empty body"
    );
}

#[tokio::test]
async fn originally_empty_body_does_not_end_h2_upstream_stream_twice() {
    let harness = start_harness_with_config(
        64 * 1024,
        HarnessConfig {
            use_h2_upstream: true,
            ..HarnessConfig::default()
        },
    )
    .await;

    assert_eq!(post_empty(harness.proxy_port).await, "200");
    assert_eq!(
        harness.recorder.bodies(),
        vec![String::new()],
        "H2 origin should observe the body completed by the HEADERS frame"
    );
}

#[tokio::test]
async fn empty_h1_request_body_can_be_replaced_for_h1_upstream() {
    const REPLACEMENT: &str = "replacement-body";
    let harness = start_harness_with_config(
        64 * 1024,
        HarnessConfig {
            replacement_body: Some(REPLACEMENT),
            ..HarnessConfig::default()
        },
    )
    .await;

    assert_eq!(post_empty(harness.proxy_port).await, "200");
    assert_eq!(
        harness.recorder.bodies(),
        vec![REPLACEMENT.to_string()],
        "H1 origin should receive the application-supplied body"
    );
}

#[tokio::test]
async fn empty_h1_request_body_can_be_replaced_for_h2_upstream() {
    const REPLACEMENT: &str = "replacement-body";
    let harness = start_harness_with_config(
        64 * 1024,
        HarnessConfig {
            replacement_body: Some(REPLACEMENT),
            use_h2_upstream: true,
            ..HarnessConfig::default()
        },
    )
    .await;

    assert_eq!(post_empty(harness.proxy_port).await, "200");
    assert_eq!(
        harness.recorder.bodies(),
        vec![REPLACEMENT.to_string()],
        "H2 origin should receive the application-supplied body"
    );
}

#[tokio::test]
async fn empty_h2_request_body_can_be_replaced_for_h1_upstream() {
    const REPLACEMENT: &str = "replacement-body";
    let harness = start_harness_with_config(
        64 * 1024,
        HarnessConfig {
            replacement_body: Some(REPLACEMENT),
            use_h2_downstream: true,
            ..HarnessConfig::default()
        },
    )
    .await;

    assert_eq!(post_empty_h2(harness.proxy_port).await, "200");
    assert_eq!(
        harness.recorder.bodies(),
        vec![REPLACEMENT.to_string()],
        "H1 origin should receive the application-supplied body"
    );
}

#[tokio::test]
async fn empty_h2_request_body_can_be_replaced_for_h2_upstream() {
    const REPLACEMENT: &str = "replacement-body";
    let harness = start_harness_with_config(
        64 * 1024,
        HarnessConfig {
            replacement_body: Some(REPLACEMENT),
            use_h2_downstream: true,
            use_h2_upstream: true,
            ..HarnessConfig::default()
        },
    )
    .await;

    assert_eq!(post_empty_h2(harness.proxy_port).await, "200");
    assert_eq!(
        harness.recorder.bodies(),
        vec![REPLACEMENT.to_string()],
        "H2 origin should receive the application-supplied body"
    );
}

#[tokio::test]
async fn explicit_empty_buffer_does_not_write_after_h2_end_stream() {
    let harness = start_harness_with_config(
        64 * 1024,
        HarnessConfig {
            replacement_body: Some(""),
            use_h2_downstream: true,
            use_h2_upstream: true,
            ..HarnessConfig::default()
        },
    )
    .await;

    assert_eq!(post_empty_h2(harness.proxy_port).await, "200");
    assert_eq!(
        harness.recorder.bodies(),
        vec![String::new()],
        "H2 origin should observe the explicit empty buffer"
    );
}
