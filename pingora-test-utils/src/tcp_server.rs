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

use std::future::Future;
use std::io;
use std::net::{Ipv4Addr, SocketAddr};
use std::panic;
use std::sync::Arc;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;
use tokio::task::{JoinHandle, JoinSet};

/// A TCP test server that owns its listener and all active connection tasks.
pub(crate) struct TestTcpServer {
    addr: SocketAddr,
    shutdown_tx: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<()>>,
}

impl TestTcpServer {
    /// Bind an origin to an ephemeral IPv4 localhost port.
    pub(crate) async fn bind<H, F>(origin_name: &'static str, handler: H) -> io::Result<Self>
    where
        H: Fn(TcpStream, SocketAddr) -> F + Send + Sync + 'static,
        F: Future<Output = ()> + Send + 'static,
    {
        Self::bind_to(
            SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
            origin_name,
            handler,
        )
        .await
    }

    /// Bind an origin to `addr`.
    pub(crate) async fn bind_to<H, F>(
        addr: SocketAddr,
        origin_name: &'static str,
        handler: H,
    ) -> io::Result<Self>
    where
        H: Fn(TcpStream, SocketAddr) -> F + Send + Sync + 'static,
        F: Future<Output = ()> + Send + 'static,
    {
        let listener = TcpListener::bind(addr).await?;
        let addr = listener.local_addr()?;
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let task = tokio::spawn(run(listener, Arc::new(handler), shutdown_rx, origin_name));

        Ok(Self {
            addr,
            shutdown_tx: Some(shutdown_tx),
            task: Some(task),
        })
    }

    /// Return the address on which the server is listening.
    pub(crate) fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Return the server's base URL without a trailing slash.
    pub(crate) fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// Stop the listener and active connections, then wait for cleanup.
    ///
    /// Panics from connection handlers and listener accept failures are rethrown here.
    pub(crate) async fn shutdown(mut self) {
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

impl Drop for TestTcpServer {
    fn drop(&mut self) {
        self.signal_shutdown();
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

async fn run<H, F>(
    listener: TcpListener,
    handler: Arc<H>,
    mut shutdown_rx: oneshot::Receiver<()>,
    origin_name: &'static str,
) where
    H: Fn(TcpStream, SocketAddr) -> F + Send + Sync + 'static,
    F: Future<Output = ()> + Send + 'static,
{
    let mut connections = JoinSet::new();
    let mut connection_panic = None;
    let mut accept_error = None;

    loop {
        tokio::select! {
            _ = &mut shutdown_rx => break,
            accepted = listener.accept() => {
                let (stream, peer) = match accepted {
                    Ok(accepted) => accepted,
                    Err(error) => {
                        accept_error = Some(error);
                        break;
                    }
                };
                let handler = Arc::clone(&handler);
                connections.spawn(async move {
                    handler(stream, peer).await;
                });
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
        panic!("{origin_name} failed to accept a connection: {accept_error}");
    }
}
