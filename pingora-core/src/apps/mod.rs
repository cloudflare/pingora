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

//! The abstraction and implementation interface for service application logic

pub mod http_app;
pub mod prometheus_http_app;

use crate::server::ShutdownWatch;
use async_trait::async_trait;
use log::{debug, error};
use std::sync::Arc;

use crate::protocols::http::v2::server;
use crate::protocols::http::ServerSession;
use crate::protocols::Digest;
use crate::protocols::Stream;
use crate::protocols::ALPN;

#[async_trait]
/// This trait defines the interface of a transport layer (TCP or TLS) application.
pub trait ServerApp {
    /// Whenever a new connection is established, this function will be called with the established
    /// [`Stream`] object provided.
    ///
    /// The application can do whatever it wants with the `session`.
    ///
    /// After processing the `session`, if the `session`'s connection is reusable, This function
    /// can return it to the service by returning `Some(session)`. The returned `session` will be
    /// fed to another [`Self::process_new()`] for another round of processing.
    /// If not reusable, `None` should be returned.
    ///
    /// The `shutdown` argument will change from `false` to `true` when the server receives a
    /// signal to shutdown. This argument allows the application to react accordingly.
    async fn process_new(
        self: &Arc<Self>,
        mut session: Stream,
        // TODO: make this ShutdownWatch so that all task can await on this event
        shutdown: &ShutdownWatch,
    ) -> Option<Stream>;

    /// This callback will be called once after the service stops listening to its endpoints.
    async fn cleanup(&self) {}
}
#[non_exhaustive]
#[derive(Default)]
/// HTTP Server options that control how the server handles some transport types.
pub struct HttpServerOptions {
    /// Use HTTP/2 for plaintext.
    pub h2c: bool,
}

/// This trait defines the interface of an HTTP application.
#[async_trait]
pub trait HttpServerApp {
    /// Similar to the [`ServerApp`], this function is called whenever a new HTTP session is established.
    ///
    /// After successful processing, [`ServerSession::finish()`] can be called to return an optionally reusable
    /// connection back to the service. The caller needs to make sure that the connection is in a reusable state
    /// i.e., no error or incomplete read or write headers or bodies. Otherwise a `None` should be returned.
    async fn process_new_http(
        self: &Arc<Self>,
        mut session: ServerSession,
        // TODO: make this ShutdownWatch so that all task can await on this event
        shutdown: &ShutdownWatch,
    ) -> Option<Stream>;

    /// Provide options on how HTTP/2 connection should be established. This function will be called
    /// every time a new HTTP/2 **connection** needs to be established.
    ///
    /// A `None` means to use the built-in default options. See [`server::H2Options`] for more details.
    fn h2_options(&self) -> Option<server::H2Options> {
        None
    }

    /// Provide HTTP server options used to override default behavior. This function will be called
    /// every time a new connection is processed.
    ///
    /// A `None` means no server options will be applied.
    fn server_options(&self) -> Option<&HttpServerOptions> {
        None
    }

    async fn http_cleanup(&self) {}
}

#[async_trait]
impl<T> ServerApp for T
where
    T: HttpServerApp + Send + Sync + 'static,
{
    async fn process_new(
        self: &Arc<Self>,
        stream: Stream,
        shutdown: &ShutdownWatch,
    ) -> Option<Stream> {
        let h2c = self.server_options().as_ref().map_or(false, |o| o.h2c);
        // TODO: allow h2c and http/1.1 to co-exist
        if h2c || matches!(stream.selected_alpn_proto(), Some(ALPN::H2)) {
            // create a shared connection digest
            let digest = Arc::new(Digest {
                ssl_digest: stream.get_ssl_digest(),
                // TODO: log h2 handshake time
                timing_digest: stream.get_timing_digest(),
                proxy_digest: stream.get_proxy_digest(),
                socket_digest: stream.get_socket_digest(),
            });

            let h2_options = self.h2_options();
            let h2_conn = server::handshake(stream, h2_options).await;
            let mut h2_conn = match h2_conn {
                Err(e) => {
                    error!("H2 handshake error {e}");
                    return None;
                }
                Ok(c) => c,
            };

            loop {
                // this loop ends when the client decides to close the h2 conn
                // TODO: add a timeout?
                let h2_stream =
                    server::HttpSession::from_h2_conn(&mut h2_conn, digest.clone()).await;
                let h2_stream = match h2_stream {
                    Err(e) => {
                        // It is common for the client to just disconnect TCP without properly
                        // closing H2. So we don't log the errors here
                        debug!("H2 error when accepting new stream {e}");
                        return None;
                    }
                    Ok(s) => s?, // None means the connection is ready to be closed
                };
                let app = self.clone();
                let shutdown = shutdown.clone();
                pingora_runtime::current_handle().spawn(async move {
                    app.process_new_http(ServerSession::new_http2(h2_stream), &shutdown)
                        .await;
                });
            }
        } else {
            // No ALPN or ALPN::H1 and h2c was not configured, fallback to HTTP/1.1
            self.process_new_http(ServerSession::new_http1(stream), shutdown)
                .await
        }
    }

    async fn cleanup(&self) {
        self.http_cleanup().await;
    }
}
