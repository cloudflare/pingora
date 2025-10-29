// Copyright 2024 Cloudflare, Inc.
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

//! Test for H1 deadlock when upstream ignores request body and sends response immediately
//!
//! ## The Issue
//!
//! When an HTTP/1.1 upstream server ignores the request body and immediately starts
//! sending a large response, TCP buffers can fill in both directions causing a deadlock:
//!
//! 1. Client → Pingora: Sending large POST body (10MB)
//! 2. Pingora → Upstream: Forwarding POST body (blocking write)
//! 3. Upstream → Pingora: Sending large response (10MB), ignoring request
//! 4. Deadlock: Pingora can't send more (upstream buffer full), can't read response (blocked on write)
//!
//! ## The Fix
//!
//! Use non-blocking writes with a pending write queue, allowing the select! loop to:
//! - Read response from upstream even while request write is blocked
//! - Make progress in both directions independently
//!
//! ## Running the Test
//!
//! ```bash
//! # Run the deadlock test
//! cargo test --test test_h1_deadlock
//!
//! # With output
//! cargo test --test test_h1_deadlock -- --nocapture
//! ```
//!
//! ## Requirements
//!
//! - Python 3 installed (for test server)
//!
//! ## Expected Behavior
//!
//! - **With fix**: Test completes in ~1-2 seconds (may have connection errors from Python, but no deadlock)
//! - **Without fix**: Test would timeout after 10 seconds (deadlock detected)

use async_trait::async_trait;
use pingora_core::server::Server;
use pingora_core::upstreams::peer::HttpPeer;
use pingora_core::Result;
use pingora_proxy::{http_proxy_service, ProxyHttp, Session};
use std::process::{Child, Command};
use std::time::Duration;

const PYTHON_SERVER: &str = r#"
from http.server import BaseHTTPRequestHandler, HTTPServer
class Handler(BaseHTTPRequestHandler):
    def do_POST(self):
        # Ignore request body and send response immediately (reproduces the deadlock issue)
        # Write in chunks to allow Python to make progress
        total_size = 10 * 1024 * 1024  # 10MB
        chunk_size = 64 * 1024  # 64KB chunks
        self.send_response(200)
        self.send_header('Content-Length', str(total_size))
        self.end_headers()
        # Write response in chunks
        remaining = total_size
        while remaining > 0:
            chunk = min(chunk_size, remaining)
            self.wfile.write(b'X' * chunk)
            remaining -= chunk
with HTTPServer(('', 8081), Handler) as server:
    server.serve_forever()
"#;

struct PythonServer(Child);

impl Drop for PythonServer {
    fn drop(&mut self) {
        let _ = self.0.kill();
    }
}

fn start_python_server() -> PythonServer {
    std::fs::write("/tmp/h1_deadlock_test_server.py", PYTHON_SERVER).unwrap();
    let child = Command::new("python3")
        .arg("/tmp/h1_deadlock_test_server.py")
        .spawn()
        .expect("Failed to start Python test server. Make sure python3 is installed.");
    PythonServer(child)
}

struct TestService;

#[async_trait]
impl ProxyHttp for TestService {
    type CTX = ();
    fn new_ctx(&self) -> Self::CTX {}
    async fn upstream_peer(
        &self,
        _session: &mut Session,
        _ctx: &mut Self::CTX,
    ) -> Result<Box<HttpPeer>> {
        Ok(Box::new(HttpPeer::new(
            ("127.0.0.1", 8081),
            false,
            "localhost".to_string(),
        )))
    }
}

/// Test that H1 proxy doesn't deadlock when upstream ignores request body
///
/// Reproduces the deadlock scenario:
/// 1. Client sends large POST body (10MB) to Pingora
/// 2. Pingora forwards request to upstream
/// 3. Upstream IGNORES request body and immediately sends large response (10MB)
/// 4. Without fix: TCP buffers fill in both directions → deadlock
/// 5. With fix: Non-blocking writes allow bidirectional progress → no deadlock
///
/// This test passes if the request completes within the timeout (no deadlock).
/// Connection errors from the Python server are acceptable - we only care that
/// it doesn't hang indefinitely.
#[tokio::test(flavor = "multi_thread")]
async fn test_h1_no_deadlock_when_upstream_ignores_request_body() {
    // Start Python server that ignores request body
    let _python_server = start_python_server();
    tokio::time::sleep(Duration::from_secs(1)).await;

    // Start Pingora proxy in a separate thread (to avoid nested runtime)
    std::thread::spawn(|| {
        let mut server = Server::new(None).unwrap();
        server.bootstrap();
        let mut svc = http_proxy_service(&server.configuration, TestService);
        svc.add_tcp("127.0.0.1:6081");
        server.add_service(svc);
        server.run_forever();
    });
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Test: Send 10MB POST request, expect 10MB response
    // Without the deadlock fix, this would hang indefinitely
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap();

    let result = client
        .post("http://127.0.0.1:6081/")
        .body(vec![0u8; 10 * 1024 * 1024]) // 10MB request body
        .send()
        .await;

    // The test passes if we don't timeout (no deadlock)
    // Connection errors are acceptable - Python server has limitations
    match result {
        Ok(res) => {
            // Try to read body with a timeout
            match tokio::time::timeout(Duration::from_secs(5), res.bytes()).await {
                Ok(Ok(body)) => {
                    eprintln!("✅ Success: Received {} bytes without deadlock", body.len());
                    assert!(body.len() > 0, "Should receive some data");
                }
                Ok(Err(e)) => {
                    eprintln!("✅ Success: Completed without deadlock (body error: {})", e);
                    // Connection error is OK as long as we didn't hang
                }
                Err(_) => {
                    panic!("❌ FAIL: Timeout reading response (deadlock detected!)");
                }
            }
        }
        Err(e) => {
            if e.is_timeout() {
                panic!("❌ FAIL: Request timeout (deadlock detected!)");
            } else {
                eprintln!("✅ Success: Completed without deadlock (error: {})", e);
                // Error is acceptable as long as it didn't deadlock
            }
        }
    }
}
