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

//! An owned raw HTTP/TCP origin for integration tests.

use std::future::Future;
use std::io;
use std::net::SocketAddr;
use tokio::io::AsyncReadExt;
use tokio::net::TcpStream;

use crate::tcp_server::TestTcpServer;

/// A raw HTTP/TCP origin whose listener and connection tasks are owned by this value.
///
/// The origin binds to an ephemeral localhost port by default. Dropping it
/// immediately stops the listener and all active connections. Use
/// [`RawHttpOrigin::shutdown`] when tests need to wait for cleanup or surface
/// handler and listener failures; [`Drop`] cannot await the server task.
pub struct RawHttpOrigin {
    server: TestTcpServer,
}

impl RawHttpOrigin {
    /// Bind an origin to an ephemeral IPv4 localhost port.
    pub async fn bind<H, F>(handler: H) -> io::Result<Self>
    where
        H: Fn(TcpStream, SocketAddr) -> F + Send + Sync + 'static,
        F: Future<Output = ()> + Send + 'static,
    {
        let server = TestTcpServer::bind("raw HTTP test origin", handler).await?;
        Ok(Self { server })
    }

    /// Bind an origin to `addr`.
    pub async fn bind_to<H, F>(addr: SocketAddr, handler: H) -> io::Result<Self>
    where
        H: Fn(TcpStream, SocketAddr) -> F + Send + Sync + 'static,
        F: Future<Output = ()> + Send + 'static,
    {
        let server = TestTcpServer::bind_to(addr, "raw HTTP test origin", handler).await?;
        Ok(Self { server })
    }

    /// Return the address on which the origin is listening.
    pub fn addr(&self) -> SocketAddr {
        self.server.addr()
    }

    /// Return the origin's base URL without a trailing slash.
    pub fn url(&self) -> String {
        self.server.url()
    }

    /// Stop the listener and active connections, then wait for cleanup.
    ///
    /// This method consumes the origin. If the returned future is cancelled,
    /// dropping it still aborts the server task. Panics from connection handlers
    /// and listener accept failures are rethrown here.
    pub async fn shutdown(self) {
        self.server.shutdown().await;
    }
}

/// Read an HTTP/1 head from a raw stream and return the bytes as lossy UTF-8 text.
///
/// The read may include bytes following the terminating `\r\n\r\n` if they arrive in
/// the same socket read.
pub async fn read_http1_head(stream: &mut TcpStream) -> String {
    let mut head = Vec::new();
    let mut buffer = [0; 1024];
    while !head.windows(4).any(|window| window == b"\r\n\r\n") {
        let read = stream.read(&mut buffer).await.unwrap();
        assert_ne!(read, 0, "connection closed before the HTTP/1 head");
        head.extend_from_slice(&buffer[..read]);
    }
    String::from_utf8_lossy(&head).into_owned()
}
