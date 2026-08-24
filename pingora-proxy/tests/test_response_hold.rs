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

//! End-to-end tests for holding the final upstream response header until a
//! body verdict releases it.

mod utils;

use async_trait::async_trait;
use bytes::Bytes;
use once_cell::sync::Lazy;
use pingora_cache::{CacheKey, CachePhase, MemCache, NoCacheReason};
use pingora_core::modules::http::compression::ResponseCompressionBuilder;
use pingora_core::modules::http::HttpModules;
use pingora_core::upstreams::peer::HttpPeer;
use pingora_error::{Error, Result};
use pingora_http::ResponseHeader;
use pingora_proxy::{HeldResponse, ProxyHttp, Session};
use std::net::SocketAddr;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use utils::response_hold::{
    closed_port, raw_request, start_h2_origin, start_proxy, H1Origin, Step,
};

static CACHE: Lazy<MemCache> = Lazy::new(MemCache::new);
static ORIGIN: Lazy<H1Origin> = Lazy::new(|| H1Origin::start(h1_route));
static PROXY: Lazy<SocketAddr> = Lazy::new(|| {
    start_proxy(Scanner {
        h1: ORIGIN.addr,
        h2: start_h2_origin(|_| b"HELLO"),
    })
});

const HELLO: &[u8] =
    b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 5\r\nConnection: close\r\n\r\nHELLO";
const HEADER_5: &[u8] =
    b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 5\r\nConnection: close\r\n\r\n";

fn h1_route(path: &str) -> Vec<Step> {
    use Step::*;
    let pause = Pause(Duration::from_millis(20));
    match path {
        "/compress" => vec![
            Send(b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 1024\r\nConnection: close\r\n\r\n"),
            Send(&[b'a'; 1024]),
        ],
        "/split" | "/block-split" => vec![Send(HEADER_5), pause, Send(b"HELLO")],
        "/release-early-filtered" => vec![Send(HEADER_5), Send(b"HE"), pause, Send(b"LLO")],
        "/trailers" => vec![Send(
            b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nTransfer-Encoding: chunked\r\nTrailer: x-checksum\r\nConnection: close\r\n\r\n5\r\nHELLO\r\n0\r\nx-checksum: ok\r\n\r\n",
        )],
        "/hints" => vec![Send(
            b"HTTP/1.1 103 Early Hints\r\nLink: </style.css>; rel=preload\r\n\r\nHTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 5\r\nConnection: close\r\n\r\nHELLO",
        )],
        "/block-chunked" => vec![Send(
            b"HTTP/1.0 200 OK\r\nContent-Type: text/plain\r\nConnection: close\r\n\r\nEVIL!",
        )],
        "/retry-held" => vec![Send(b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\nLEAKS")],
        "/upgrade" => vec![Send(
            b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\nTUNNEL",
        )],
        "/empty" => vec![Send(b"HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n")],
        _ => vec![Send(HELLO)],
    }
}

/// A scanner-style proxy that holds each response header until its body
/// verdict is ready.
struct Scanner {
    h1: SocketAddr,
    h2: SocketAddr,
}

#[derive(Default)]
struct ScannerCtx {
    path: String,
    attempts: usize,
    buffer: Vec<u8>,
}

impl ScannerCtx {
    /// Whether the held filter should reach its verdict now.
    fn ready(&self, end_of_stream: bool) -> bool {
        end_of_stream || (self.path.contains("/release-early") && !self.buffer.is_empty())
    }

    /// Decide on the buffered body, changing `header` as needed. Returns the
    /// body to send after release, or `None` to leave the header held.
    fn verdict(&mut self, header: &mut ResponseHeader) -> Result<Option<Bytes>> {
        if self.path.ends_with("/broken") {
            return Ok(None);
        }
        if self.path.ends_with("/invalid") {
            header.set_status(103)?;
        } else if self.path.contains("block") {
            header.set_status(403)?;
            if header
                .headers
                .get(http::header::TRANSFER_ENCODING)
                .is_none()
            {
                header.insert_header(http::header::CONTENT_LENGTH, "7")?;
            }
            return Ok(Some(Bytes::from_static(b"BLOCKED")));
        }
        Ok(Some(std::mem::take(&mut self.buffer).into()))
    }
}

#[async_trait]
impl ProxyHttp for Scanner {
    type CTX = ScannerCtx;

    fn new_ctx(&self) -> Self::CTX {
        ScannerCtx::default()
    }

    fn init_downstream_modules(&self, modules: &mut HttpModules) {
        // Compression requires the final header before any body.
        modules.add_module(ResponseCompressionBuilder::enable(6));
    }

    fn hold_response_header(&self, _session: &Session, _ctx: &Self::CTX) -> bool {
        true
    }

    async fn upstream_peer(
        &self,
        session: &mut Session,
        ctx: &mut Self::CTX,
    ) -> Result<Box<HttpPeer>> {
        ctx.path = session.req_header().uri.path().to_string();
        ctx.attempts += 1;
        let h2 = ctx.path.starts_with("/h2-");
        let addr = if ctx.path.contains("/retry-connect") && ctx.attempts == 1 {
            SocketAddr::from(([127, 0, 0, 1], closed_port()))
        } else if h2 {
            self.h2
        } else {
            self.h1
        };
        let mut peer = HttpPeer::new(addr, false, String::new());
        if h2 {
            peer.options.set_http_version(2, 2);
        }
        Ok(Box::new(peer))
    }

    fn fail_to_connect(
        &self,
        _session: &mut Session,
        _peer: &HttpPeer,
        ctx: &mut Self::CTX,
        mut e: Box<Error>,
    ) -> Box<Error> {
        e.set_retry(ctx.path.contains("/retry-connect"));
        e
    }

    fn error_while_proxy(
        &self,
        _peer: &HttpPeer,
        _session: &mut Session,
        mut e: Box<Error>,
        ctx: &mut Self::CTX,
        _client_reused: bool,
    ) -> Box<Error> {
        e.set_retry(ctx.path.contains("/retry-held"));
        e
    }

    fn request_cache_filter(&self, session: &mut Session, _ctx: &mut Self::CTX) -> Result<()> {
        session.cache.enable(&*CACHE, None, None, None, None);
        Ok(())
    }

    fn cache_key_callback(&self, session: &Session, _ctx: &mut Self::CTX) -> Result<CacheKey> {
        Ok(CacheKey::new("hold", session.req_header().uri.path()))
    }

    async fn proxy_upstream_filter(
        &self,
        session: &mut Session,
        _ctx: &mut Self::CTX,
    ) -> Result<bool> {
        // The proxy must bypass the cache again after this hook.
        session.cache.enable(&*CACHE, None, None, None, None);
        Ok(true)
    }

    async fn response_filter(
        &self,
        _session: &mut Session,
        upstream_response: &mut ResponseHeader,
        ctx: &mut Self::CTX,
    ) -> Result<()> {
        upstream_response.insert_header("x-response-filter", "ran")?;
        upstream_response.insert_header("x-attempts", ctx.attempts.to_string())?;
        Ok(())
    }

    async fn response_body_filter(
        &self,
        _session: &mut Session,
        body: &mut Option<Bytes>,
        _end_of_stream: bool,
        ctx: &mut Self::CTX,
    ) -> Result<Option<Duration>> {
        if ctx.path.ends_with("-filtered") {
            if let Some(b) = body {
                *b = b.to_ascii_lowercase().into();
            }
        }
        Ok(None)
    }

    // The API-specific part: hand the verdict to the held response.

    async fn held_upstream_response_body_filter(
        &self,
        mut response: HeldResponse<'_>,
        body: Option<Bytes>,
        end_of_stream: bool,
        ctx: &mut Self::CTX,
    ) -> Result<Option<Duration>> {
        assert!(matches!(
            response.session().cache.phase(),
            CachePhase::Disabled(NoCacheReason::Custom("response header hold"))
        ));
        assert!(!ctx.path.contains("/upgrade"), "a 101 must not be held");
        if let Some(chunk) = body {
            ctx.buffer.extend_from_slice(&chunk);
        }
        if ctx.ready(end_of_stream) {
            if let Some(released) = ctx.verdict(response.header_mut())? {
                response.release(Some(released))?;
            }
        }
        Ok(None)
    }

    async fn held_upstream_response_trailer_filter(
        &self,
        mut response: HeldResponse<'_>,
        _upstream_trailers: &mut http::HeaderMap,
        ctx: &mut Self::CTX,
    ) -> Result<()> {
        if let Some(released) = ctx.verdict(response.header_mut())? {
            response.release(Some(released))?;
        }
        Ok(())
    }
}

async fn get(path: &str) -> reqwest::Response {
    // A response whose header is never released must fail fast, not hang.
    reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap()
        .get(format!("http://{}{path}", *PROXY))
        .send()
        .await
        .unwrap()
}

/// Check the status and body of `path`, returning its headers.
async fn assert_response(path: &str, status: u16, body: &[u8]) -> reqwest::header::HeaderMap {
    let res = get(path).await;
    assert_eq!(res.status().as_u16(), status, "{path}");
    let headers = res.headers().clone();
    assert_eq!(res.bytes().await.unwrap(), body, "{path}");
    headers
}

#[tokio::test]
async fn allow_releases_header_with_response_filter_changes() {
    let headers = assert_response("/allow", 200, b"HELLO").await;
    assert_eq!(headers["x-response-filter"], "ran");
}

#[tokio::test]
async fn downstream_compression_sees_released_header_first() {
    let res = reqwest::Client::builder()
        .gzip(false)
        .build()
        .unwrap()
        .get(format!("http://{}/compress", *PROXY))
        .header(reqwest::header::ACCEPT_ENCODING, "gzip")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    assert_eq!(res.headers()[reqwest::header::CONTENT_ENCODING], "gzip");
    assert!(!res.bytes().await.unwrap().is_empty());
}

#[tokio::test]
async fn released_and_later_chunks_take_the_normal_body_filters() {
    assert_response("/release-early-filtered", 200, b"hello").await;
}

#[tokio::test]
async fn allow_across_read_batches() {
    assert_response("/split", 200, b"HELLO").await;
}

#[tokio::test]
async fn allow_h2_upstream() {
    assert_response("/h2-allow", 200, b"HELLO").await;
}

#[tokio::test]
async fn h2_trailer_filter_releases_with_buffered_body() {
    assert_response("/h2-trailers", 200, b"HELLO").await;
}

#[tokio::test]
async fn h2_trailer_release_takes_the_normal_body_filter() {
    assert_response("/h2-trailers-filtered", 200, b"hello").await;
}

#[tokio::test]
async fn h1_chunked_trailers_release_from_body_filter() {
    // The HTTP/1 client drops trailers, so end of stream reaches the body filter.
    assert_response("/trailers", 200, b"HELLO").await;
}

#[tokio::test]
async fn block_changes_held_header() {
    let headers = assert_response("/block", 403, b"BLOCKED").await;
    assert_eq!(headers["x-response-filter"], "ran");
}

#[tokio::test]
async fn block_across_read_batches() {
    assert_response("/block-split", 403, b"BLOCKED").await;
}

#[tokio::test]
async fn block_h2_upstream() {
    assert_response("/h2-block", 403, b"BLOCKED").await;
}

#[tokio::test]
async fn block_keeps_chunked_framing() {
    let headers = assert_response("/block-chunked", 403, b"BLOCKED").await;
    assert_eq!(headers[reqwest::header::TRANSFER_ENCODING], "chunked");
}

#[tokio::test]
async fn informational_header_goes_out_before_released_header() {
    let response = raw_request(
        *PROXY,
        "GET /hints HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    )
    .await;
    let hints = response.find("103 Early Hints").unwrap();
    let final_header = response.find("200 OK").unwrap();
    assert!(hints < final_header, "{response}");
}

#[tokio::test]
async fn header_only_response_passes_through() {
    let headers = assert_response("/empty", 204, b"").await;
    assert_eq!(headers["x-response-filter"], "ran");
}

#[tokio::test]
async fn switching_protocols_passes_through() {
    let mut stream = tokio::net::TcpStream::connect(*PROXY).await.unwrap();
    stream
        .write_all(
            b"GET /upgrade HTTP/1.1\r\nHost: localhost\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\n",
        )
        .await
        .unwrap();
    let mut response = Vec::new();
    let mut buf = [0u8; 1024];
    while !response.ends_with(b"TUNNEL") {
        let n = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut buf))
            .await
            .unwrap()
            .unwrap();
        assert_ne!(n, 0, "{}", String::from_utf8_lossy(&response));
        response.extend_from_slice(&buf[..n]);
    }
    assert!(response.starts_with(b"HTTP/1.1 101"));
}

#[tokio::test]
async fn release_with_informational_status_fails() {
    assert_eq!(get("/invalid").await.status(), 500);
}

#[tokio::test]
async fn header_never_released_fails() {
    assert_eq!(get("/broken").await.status(), 500);
}

#[tokio::test]
async fn retry_after_connect_failure_is_still_held() {
    let headers = assert_response("/retry-connect-block", 403, b"BLOCKED").await;
    assert_eq!(headers["x-attempts"], "2");
}

#[tokio::test]
async fn no_retry_once_header_is_held() {
    let res = get("/retry-held").await;
    assert!(res.status().is_server_error(), "{}", res.status());
    let body = res.bytes().await.unwrap_or_default();
    assert!(!body.windows(5).any(|w| w == b"LEAKS"), "{body:?}");
    assert_eq!(ORIGIN.connections("/retry-held"), 1);
}
