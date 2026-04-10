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

//! Regression test for the bodyless-response body filter fix.
//!
//! Self-contained: spawns a pure-Rust HTTP/1 mock upstream and a pingora
//! proxy whose `response_filter` mutates 204 -> 200 and whose
//! `response_body_filter` injects a synthesized body. Verifies that the
//! downstream client receives the synthesized body, not an empty response.
//!
//! Does NOT depend on the openresty-based test fixture, so it runs
//! standalone (`cargo test -p pingora-proxy --test test_body_filter_bodyless`).

use async_trait::async_trait;
use bytes::Bytes;
use once_cell::sync::Lazy;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use pingora_core::server::configuration::Opt;
use pingora_core::server::Server;
use pingora_core::upstreams::peer::HttpPeer;
use pingora_core::Result;
use pingora_http::ResponseHeader;
use pingora_proxy::{ProxyHttp, Session};

const SYNTHESIZED_BODY: &[u8] = b"<synthesized/>";
const PROXY_ADDR: &str = "127.0.0.1:6180";
const UPSTREAM_ADDR: &str = "127.0.0.1:6181";

struct MockUpstream {
    _handle: thread::JoinHandle<()>,
}

impl MockUpstream {
    fn start() -> Self {
        let (tx, rx) = mpsc::channel();
        let handle = thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(Self::run(tx));
        });
        rx.recv_timeout(Duration::from_secs(5))
            .expect("mock upstream failed to bind within 5s");
        MockUpstream { _handle: handle }
    }

    async fn run(ready: mpsc::Sender<()>) {
        let listener = TcpListener::bind(UPSTREAM_ADDR).await.unwrap();
        let _ = ready.send(());
        loop {
            let (mut sock, _) = listener.accept().await.unwrap();
            tokio::spawn(async move {
                // Read request (best effort, tiny test).
                let mut buf = [0u8; 1024];
                let _ = sock.read(&mut buf).await;
                let req = String::from_utf8_lossy(&buf);
                let resp: &[u8] = if req.starts_with("GET /no-body") {
                    b"HTTP/1.1 204 No Content\r\n\r\n"
                } else {
                    b"HTTP/1.1 200 OK\r\nContent-Length: 13\r\n\r\nHello World!\n"
                };
                let _ = sock.write_all(resp).await;
            });
        }
    }
}

struct TestProxy;

struct TestCtx {
    inject: bool,
}

#[async_trait]
impl ProxyHttp for TestProxy {
    type CTX = TestCtx;
    fn new_ctx(&self) -> Self::CTX {
        TestCtx { inject: false }
    }

    async fn upstream_peer(
        &self,
        _session: &mut Session,
        _ctx: &mut Self::CTX,
    ) -> Result<Box<HttpPeer>> {
        Ok(Box::new(HttpPeer::new(UPSTREAM_ADDR, false, String::new())))
    }

    async fn response_filter(
        &self,
        _session: &mut Session,
        upstream_response: &mut ResponseHeader,
        ctx: &mut Self::CTX,
    ) -> Result<()> {
        if upstream_response.status.as_u16() == 204 {
            upstream_response.set_status(200)?;
            ctx.inject = true;
        }
        Ok(())
    }

    fn response_body_filter(
        &self,
        _session: &mut Session,
        body: &mut Option<Bytes>,
        end_of_stream: bool,
        ctx: &mut Self::CTX,
    ) -> Result<Option<std::time::Duration>> {
        if end_of_stream && ctx.inject {
            *body = Some(Bytes::from_static(SYNTHESIZED_BODY));
        }
        Ok(None)
    }
}

struct ProxyServer {
    _handle: thread::JoinHandle<()>,
}

impl ProxyServer {
    fn start() -> Self {
        let handle = thread::spawn(|| {
            let opt = Opt {
                upgrade: false,
                daemon: false,
                nocapture: false,
                test: false,
                conf: None,
            };
            let mut server = Server::new(Some(opt)).unwrap();
            server.bootstrap();
            let mut svc = pingora_proxy::http_proxy_service(&server.configuration, TestProxy);
            svc.add_tcp(PROXY_ADDR);
            server.add_service(svc);
            server.run_forever();
        });
        ProxyServer { _handle: handle }
    }
}

static UPSTREAM: Lazy<MockUpstream> = Lazy::new(MockUpstream::start);
static PROXY: Lazy<ProxyServer> = Lazy::new(ProxyServer::start);

fn init() {
    let _ = &*UPSTREAM;
    let _ = &*PROXY;
    // Give the pingora server a moment to bind.
    thread::sleep(Duration::from_millis(300));
}

#[tokio::test]
async fn test_body_filter_reaches_204_upstream() {
    init();
    let res = reqwest::get(format!("http://{PROXY_ADDR}/no-body"))
        .await
        .unwrap();
    assert_eq!(res.status(), reqwest::StatusCode::OK);
    let body = res.bytes().await.unwrap();
    assert_eq!(
        body.as_ref(),
        SYNTHESIZED_BODY,
        "expected synthesized body, got {:?}",
        String::from_utf8_lossy(&body)
    );
}

#[tokio::test]
async fn test_body_filter_passthrough_on_200_upstream() {
    init();
    let res = reqwest::get(format!("http://{PROXY_ADDR}/")).await.unwrap();
    assert_eq!(res.status(), reqwest::StatusCode::OK);
    let body = res.text().await.unwrap();
    assert_eq!(body, "Hello World!\n");
}
