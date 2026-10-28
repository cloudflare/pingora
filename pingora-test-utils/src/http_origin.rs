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

//! An owned HTTP/1 origin for integration tests.

use crate::tcp_server::TestTcpServer;
use bytes::Bytes;
use http::{Request, Response};
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use std::future::Future;
use std::io;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use tokio::net::TcpStream;

/// An HTTP/1 origin whose listener and connection tasks are owned by this value.
///
/// The origin binds to an ephemeral localhost port by default. Dropping it
/// immediately stops the listener and all active connections. Use
/// [`HttpOrigin::shutdown`] when tests need to wait for cleanup or surface
/// handler and listener failures; [`Drop`] cannot await the server task.
pub struct HttpOrigin {
    server: TestTcpServer,
}

impl HttpOrigin {
    /// Bind an origin to an ephemeral IPv4 localhost port.
    pub async fn bind<H, F>(handler: H) -> io::Result<Self>
    where
        H: Fn(Request<Bytes>) -> F + Send + Sync + 'static,
        F: Future<Output = Response<Bytes>> + Send + 'static,
    {
        Self::bind_to(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)), handler).await
    }

    /// Bind an origin to `addr`.
    pub async fn bind_to<H, F>(addr: SocketAddr, handler: H) -> io::Result<Self>
    where
        H: Fn(Request<Bytes>) -> F + Send + Sync + 'static,
        F: Future<Output = Response<Bytes>> + Send + 'static,
    {
        let handler = Arc::new(handler);
        let server = TestTcpServer::bind_to(addr, "HTTP test origin", move |stream, _peer| {
            let handler = Arc::clone(&handler);
            async move {
                serve_connection(stream, handler).await;
            }
        })
        .await?;

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
    /// dropping it still aborts the server task. Panics from request handlers
    /// and listener accept failures are rethrown here.
    pub async fn shutdown(self) {
        self.server.shutdown().await;
    }
}

async fn serve_connection<H, F>(stream: TcpStream, handler: Arc<H>)
where
    H: Fn(Request<Bytes>) -> F + Send + Sync + 'static,
    F: Future<Output = Response<Bytes>> + Send + 'static,
{
    let service = service_fn(move |request: Request<Incoming>| {
        let handler = Arc::clone(&handler);
        async move {
            let (parts, body) = request.into_parts();
            let body = body.collect().await?.to_bytes();
            let response = handler(Request::from_parts(parts, body)).await;
            let (parts, body) = response.into_parts();
            Ok::<_, hyper::Error>(Response::from_parts(parts, Full::new(body)))
        }
    });

    let _ = http1::Builder::new()
        .serve_connection(TokioIo::new(stream), service)
        .await;
}
