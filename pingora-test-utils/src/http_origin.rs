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
use std::panic;
use std::sync::Arc;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;
use tokio::task::{JoinHandle, JoinSet};

/// An HTTP/1 origin whose listener and connection tasks are owned by this value.
///
/// The origin binds to an ephemeral localhost port by default. Dropping it
/// immediately stops the listener and all active connections. Use
/// [`HttpOrigin::shutdown`] when tests need to wait for cleanup or surface
/// handler and listener failures; [`Drop`] cannot await the server task.
pub struct HttpOrigin {
    addr: SocketAddr,
    shutdown_tx: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<()>>,
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
        let listener = TcpListener::bind(addr).await?;
        let addr = listener.local_addr()?;
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let task = tokio::spawn(run(listener, Arc::new(handler), shutdown_rx));

        Ok(Self {
            addr,
            shutdown_tx: Some(shutdown_tx),
            task: Some(task),
        })
    }

    /// Return the address on which the origin is listening.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Return the origin's base URL without a trailing slash.
    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// Stop the listener and active connections, then wait for cleanup.
    ///
    /// This method consumes the origin. If the returned future is cancelled,
    /// dropping it still aborts the server task. Panics from request handlers
    /// and listener accept failures are rethrown here.
    pub async fn shutdown(mut self) {
        self.signal_shutdown();
        if let Some(task) = self.task.as_mut() {
            if let Err(error) = task.await {
                if error.is_panic() {
                    panic::resume_unwind(error.into_panic());
                }
            }
        }
        self.task.take();
    }

    fn signal_shutdown(&mut self) {
        if let Some(shutdown_tx) = self.shutdown_tx.take() {
            let _ = shutdown_tx.send(());
        }
    }
}

impl Drop for HttpOrigin {
    fn drop(&mut self) {
        self.signal_shutdown();
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

async fn run<H, F>(listener: TcpListener, handler: Arc<H>, mut shutdown_rx: oneshot::Receiver<()>)
where
    H: Fn(Request<Bytes>) -> F + Send + Sync + 'static,
    F: Future<Output = Response<Bytes>> + Send + 'static,
{
    let mut connections = JoinSet::new();
    let mut connection_panic = None;
    let mut accept_error = None;

    loop {
        tokio::select! {
            _ = &mut shutdown_rx => break,
            accepted = listener.accept() => {
                let (stream, _) = match accepted {
                    Ok(accepted) => accepted,
                    Err(error) => {
                        accept_error = Some(error);
                        break;
                    }
                };
                let handler = Arc::clone(&handler);
                connections.spawn(serve_connection(stream, handler));
            }
            result = connections.join_next(), if !connections.is_empty() => {
                if let Some(Err(error)) = result {
                    if error.is_panic() {
                        connection_panic = Some(error.into_panic());
                        break;
                    }
                }
            }
        }
    }

    connections.abort_all();
    while let Some(result) = connections.join_next().await {
        if let Err(error) = result {
            if error.is_panic() && connection_panic.is_none() {
                connection_panic = Some(error.into_panic());
            }
        }
    }

    if let Some(connection_panic) = connection_panic {
        panic::resume_unwind(connection_panic);
    }
    if let Some(accept_error) = accept_error {
        panic!("HTTP test origin failed to accept a connection: {accept_error}");
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
