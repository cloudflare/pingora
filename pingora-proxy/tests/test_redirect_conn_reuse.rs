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

//! Regression test for issue #866: when `response_filter` returns an error
//! (e.g. a 3xx redirect followed by the outer retry loop), the remaining
//! upstream response body must be drained so the connection is returned to
//! the pool instead of being dropped. A later request to the same origin
//! must reuse the drained connection.

use async_trait::async_trait;
use pingora_core::server::configuration::Opt;
use pingora_core::server::Server;
use pingora_core::services::ServiceWithDependents;
use pingora_core::upstreams::peer::HttpPeer;
use pingora_error::{Error, Result};
use pingora_http::ResponseHeader;
use pingora_proxy::{http_proxy_service, ProxyHttp, Session};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const PROXY_PORT: u16 = 6155;

/// Minimal HTTP/1.1 origin. Serves `GET /a` with a 302 + body split across
/// two writes (so the upstream half is still reading when the downstream
/// half aborts), and any other path with a 200. Reads requests on the same
/// connection until EOF.
async fn handle_origin_conn(mut sock: TcpStream) {
    loop {
        let mut head = Vec::new();
        let mut buf = [0u8; 512];
        loop {
            match sock.read(&mut buf).await {
                Ok(0) => return,
                Ok(n) => {
                    head.extend_from_slice(&buf[..n]);
                    if head.windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
                Err(_) => return,
            }
        }
        let head_str = String::from_utf8_lossy(&head);
        if head_str.starts_with("GET /a") {
            // Split the body across two writes with a delay so the upstream
            // half is still reading when the downstream half aborts in
            // response_filter — the drain path (issue #866) is what makes
            // the connection reusable in that window.
            sock.write_all(
                b"HTTP/1.1 302 Found\r\nLocation: /b\r\nContent-Length: 12\r\n\r\nredir",
            )
            .await
            .unwrap();
            tokio::time::sleep(Duration::from_millis(300)).await;
            sock.write_all(b"ecting\n").await.unwrap();
        } else {
            sock.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\n\r\nok\n")
                .await
                .unwrap();
        }
    }
}

/// Starts an origin and returns its port. Counts accepted TCP connections.
async fn run_origin(accepts: Arc<AtomicUsize>) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((sock, _)) = listener.accept().await {
            accepts.fetch_add(1, Ordering::SeqCst);
            tokio::spawn(handle_origin_conn(sock));
        }
    });
    port
}

/// Follows a 3xx redirect by rewriting the request URI and erroring out of
/// `response_filter` with retry enabled, relying on the outer retry loop.
/// The redirect target is served by a different origin so the retry is
/// guaranteed to open a new connection, leaving the drained connection in
/// the first origin's pool.
struct RedirectFollowProxy {
    origin_a: u16,
    origin_b: u16,
}

#[async_trait]
impl ProxyHttp for RedirectFollowProxy {
    type CTX = ();

    fn new_ctx(&self) -> Self::CTX {}

    async fn upstream_peer(&self, session: &mut Session, _ctx: &mut ()) -> Result<Box<HttpPeer>> {
        let port = if session.req_header().uri.path() == "/b" {
            self.origin_b
        } else {
            self.origin_a
        };
        Ok(Box::new(HttpPeer::new(
            format!("127.0.0.1:{port}"),
            false,
            String::new(),
        )))
    }

    async fn response_filter(
        &self,
        session: &mut Session,
        resp: &mut ResponseHeader,
        _ctx: &mut (),
    ) -> Result<()> {
        if resp.status == http::StatusCode::FOUND {
            if let Some(location) = resp.headers.get("location").and_then(|v| v.to_str().ok()) {
                if let Ok(uri) = location.parse::<http::Uri>() {
                    session.as_downstream_mut().req_header_mut().set_uri(uri);
                }
            }
            let mut e = Error::new_str("redirect follow");
            e.set_retry(true);
            return Err(e);
        }
        Ok(())
    }
}

fn start_proxy(origin_a: u16, origin_b: u16) {
    // Minimal conf: the shared test conf binds upstream client sockets to
    // 127.0.0.2, which is not portable to all platforms.
    std::fs::write("/tmp/pingora_redirect_reuse.yaml", "---\nversion: 1\n").unwrap();
    let opts = vec![
        "pingora-proxy".to_string(),
        "-c".to_string(),
        "/tmp/pingora_redirect_reuse.yaml".to_string(),
    ];
    let mut server = Server::new(Some(Opt::parse_from_args(opts))).unwrap();
    server.bootstrap();
    let conf = server.configuration.clone();
    let mut proxy = http_proxy_service(&conf, RedirectFollowProxy { origin_a, origin_b });
    proxy.add_tcp(&format!("0.0.0.0:{PROXY_PORT}"));
    let services: Vec<Box<dyn ServiceWithDependents>> = vec![Box::new(proxy)];
    server.add_services(services);
    thread::spawn(move || server.run_forever());
}

#[tokio::test(flavor = "multi_thread")]
async fn test_redirect_filter_error_reuses_upstream_connection() {
    let accepts_a = Arc::new(AtomicUsize::new(0));
    let accepts_b = Arc::new(AtomicUsize::new(0));
    let origin_a = run_origin(accepts_a.clone()).await;
    let origin_b = run_origin(accepts_b.clone()).await;
    start_proxy(origin_a, origin_b);

    let client = reqwest::Client::new();

    // Wait for the proxy to come up.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        if client
            .get(format!("http://127.0.0.1:{PROXY_PORT}/probe"))
            .send()
            .await
            .is_ok()
        {
            break;
        }
        if tokio::time::Instant::now() > deadline {
            panic!("proxy did not come up in time");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // Redirect flow: /a -> 302 -> response_filter error -> outer retry ->
    // /b on the other origin. The /a connection has an unfinished response
    // body at abort time; the drain must return it to origin A's pool.
    let res = client
        .get(format!("http://127.0.0.1:{PROXY_PORT}/a"))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), http::StatusCode::OK);
    assert_eq!(res.text().await.unwrap(), "ok\n");

    // Wait for origin A's delayed body to arrive and the drain to finish.
    tokio::time::sleep(Duration::from_millis(700)).await;

    // A later request to origin A must reuse the drained connection: no new
    // accept. Without the drain (issue #866), the aborted connection is
    // dropped and this request opens a new one.
    let res = client
        .get(format!("http://127.0.0.1:{PROXY_PORT}/c"))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), http::StatusCode::OK);
    assert_eq!(res.text().await.unwrap(), "ok\n");

    tokio::time::sleep(Duration::from_millis(200)).await;

    // Origin A: a single connection serves the probe, /a (reusing it), and
    // the follow-up /c — the drained connection must be reused instead of
    // opening a new one. Without the drain (issue #866), the aborted
    // connection is dropped and /c opens a second connection.
    assert_eq!(
        accepts_a.load(Ordering::SeqCst),
        1,
        "the drained connection must be reused by a later request"
    );
    // Origin B: only the redirect retry.
    assert_eq!(accepts_b.load(Ordering::SeqCst), 1);
}
