// Copyright 2025 Cloudflare, Inc.
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
use std::future::poll_fn;
use std::sync::Arc;

use crate::protocols::http::v2::server as h2_server;
use crate::protocols::http::v3::server as h3_server;
use crate::protocols::http::{HttpVersion, ServerSession};
use crate::protocols::Digest;
use crate::protocols::Stream;
use crate::protocols::ALPN;

// https://datatracker.ietf.org/doc/html/rfc9113#section-3.4
const H2_PREFACE: &[u8] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";

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
    /// HTTP version to use.
    pub http_version: HttpVersion,
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
    fn h2_options(&self) -> Option<h2_server::H2Options> {
        None
    }

    /// Provide options on how HTTP/3 connection should be established. This function will be called
    /// every time a new HTTP/3 **connection** needs to be established.
    ///
    /// A `None` means to use the built-in default options. See [`server::H2Options`] for more details.
    fn h3_options(&self) -> Option<&h3_server::Http3Options> {
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
        mut stream: Stream,
        shutdown: &ShutdownWatch,
    ) -> Option<Stream> {
        let mut http_version = self
            .server_options()
            .as_ref()
            .map_or(HttpVersion::V1, |o| o.http_version);

        if stream.quic_connection_state().is_some() {
            http_version = HttpVersion::V3;
        }

        // try to read h2 preface
        if matches!(http_version, HttpVersion::V2) {
            let mut buf = [0u8; H2_PREFACE.len()];
            let peeked = stream
                .try_peek(&mut buf)
                .await
                .map_err(|e| {
                    // this error is normal when h1 reuse and close the connection
                    debug!("Read error while peeking h2c preface {e}");
                    e
                })
                .ok()?;
            // not all streams support peeking
            if peeked {
                // turn off h2c (use h1) if h2 preface doesn't exist
                http_version = match buf == H2_PREFACE {
                    true => HttpVersion::V2,
                    false => HttpVersion::V1,
                };
            }
        }

        // TODO: logic for Http3 to Http2/1 fallback. Requires Http2/1 listener being present.
        if matches!(http_version, HttpVersion::V3)
            && (matches!(stream.selected_alpn_proto(), Some(ALPN::H1))
                || matches!(stream.selected_alpn_proto(), Some(ALPN::H2))
                || matches!(stream.selected_alpn_proto(), Some(ALPN::H2H1)))
        {
            error!(
                "Server is configured for {:?}. Received ALPN: {}. \
             Fallback from Http3 to Http2/1 is currently not supported.",
                http_version,
                stream.selected_alpn_proto().unwrap()
            )
        }

        if matches!(http_version, HttpVersion::V2)
            || matches!(stream.selected_alpn_proto(), Some(ALPN::H2))
        {
            // create a shared connection digest
            let digest = Arc::new(Digest {
                ssl_digest: stream.get_ssl_digest(),
                // TODO: log h2 handshake time
                timing_digest: stream.get_timing_digest(),
                proxy_digest: stream.get_proxy_digest(),
                socket_digest: stream.get_socket_digest(),
            });

            let h2_options = self.h2_options();
            let h2_conn = h2_server::handshake(stream, h2_options).await;
            let mut h2_conn = match h2_conn {
                Err(e) => {
                    error!("H2 handshake error {e}");
                    return None;
                }
                Ok(c) => c,
            };

            let mut shutdown = shutdown.clone();
            loop {
                // this loop ends when the client decides to close the h2 conn
                // TODO: add a timeout?
                let h2_stream = tokio::select! {
                    _ = shutdown.changed() => {
                        h2_conn.graceful_shutdown();
                        let _ = poll_fn(|cx| h2_conn.poll_closed(cx))
                            .await.map_err(|e| error!("H2 error waiting for shutdown {e}"));
                        return None;
                    }
                    h2_stream = h2_server::HttpSession::from_h2_conn(&mut h2_conn, digest.clone()) => h2_stream
                };
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
        } else if matches!(http_version, HttpVersion::V3) {
            // create a shared connection digest
            let digest = Arc::new(Digest {
                ssl_digest: stream.get_ssl_digest(),
                // TODO: log h3 handshake time
                timing_digest: stream.get_timing_digest(),
                proxy_digest: stream.get_proxy_digest(),
                socket_digest: stream.get_socket_digest(),
            });

            let h3_options = self.h3_options();
            let h3_conn = h3_server::handshake(stream, h3_options).await;
            let mut h3_conn = match h3_conn {
                Err(e) => {
                    error!("H3 handshake error {e}");
                    return None;
                }
                Ok(c) => c,
            };

            let mut shutdown = shutdown.clone();
            loop {
                // this loop ends when the client decides to close the h3 conn
                let h3_stream = tokio::select! {
                    _ = shutdown.changed() => {
                        match h3_conn.graceful_shutdown().await {
                            Ok(()) => {}
                            Err(e) => { error!("H3 error waiting for shutdown {e}") }
                        };
                        return None;
                    }
                    h3_stream = h3_server::Http3Session::from_h3_conn(&mut h3_conn, digest.clone()) => h3_stream
                };

                let h3_stream = match h3_stream {
                    Err(e) => {
                        // It is common for the client to just disconnect TCP without properly
                        // closing H2. So we don't log the errors here
                        debug!("H3 error when accepting new stream {e}");
                        return None;
                    }
                    Ok(s) => s?, // None means the connection is ready to be closed
                };

                let app = self.clone();
                let shutdown = shutdown.clone();
                pingora_runtime::current_handle().spawn(async move {
                    app.process_new_http(ServerSession::new_http3(h3_stream), &shutdown)
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
