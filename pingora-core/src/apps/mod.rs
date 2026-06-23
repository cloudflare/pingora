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

//! The abstraction and implementation interface for service application logic

pub mod http_app;

use crate::server::ShutdownWatch;
use async_trait::async_trait;
use bytes::BytesMut;
use log::{debug, error};
use std::any::Any;
use std::sync::Arc;

use crate::protocols::http::v2::server;
use crate::protocols::http::{ReusableHttpStream, ServerSession};
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
#[derive(Clone, Debug, Default)]
/// HTTP Server options that control how the server handles some transport types.
pub struct HttpServerOptions {
    /// Allow HTTP/2 for plaintext.
    pub h2c: bool,

    /// Allow proxying CONNECT requests when handling HTTP traffic.
    ///
    /// When disabled, CONNECT requests are rejected with 405 by proxy services.
    pub allow_connect_method_proxying: bool,

    #[doc(hidden)]
    pub force_custom: bool,

    /// Maximum number of requests that this connection will handle. This is
    /// equivalent to [Nginx's keepalive requests](https://nginx.org/en/docs/http/ngx_http_upstream_module.html#keepalive_requests)
    /// which says:
    ///
    /// > Closing connections periodically is necessary to free per-connection
    /// > memory allocations. Therefore, using too high maximum number of
    /// > requests could result in excessive memory usage and not recommended.
    ///
    /// Unlike nginx, the default behavior here is _no limit_.
    pub keepalive_request_limit: Option<u32>,
}

/// Settings persisted across HTTP/1.x keepalive requests on the same downstream connection.
///
/// In addition to framework-managed keepalive parameters, this struct can carry an optional
/// user-defined context via [`set_user_context`](Self::set_user_context). The proxy layer
/// populates this through `ProxyHttp::persist_connection_context`
/// and delivers it to the next request through `ProxyHttp::on_connection_reuse`.
///
/// Also carries pipelined-prefix bytes when the caller has opted into HTTP/1.1
/// pipelining on the previous session. See
/// [`Self::set_pipelined_prefix`] and the
/// [`HttpSession::set_pipelining_enabled`](crate::protocols::http::v1::server::HttpSession::set_pipelining_enabled)
/// docs for the RFC 9112 §9.3.2 semantics.
#[derive(Debug)]
pub struct HttpPersistentSettings {
    keepalive_timeout: Option<u64>,
    keepalive_reuses_remaining: Option<u32>,
    /// User-defined context to carry to the next request on this connection.
    user_context: Option<Box<dyn Any + Send + Sync>>,
    /// Bytes read past the end of the previous request's body, to be parsed
    /// as the next pipelined request on the reused connection.
    pipelined_prefix: Option<BytesMut>,
    /// Whether HTTP/1.1 pipelining was enabled on the previous session;
    /// propagates to the next session so the proxy-level opt-in sticks
    /// across keepalive reuses without the adopter having to re-enable
    /// it on every request.
    pipelining_enabled: bool,
}

impl HttpPersistentSettings {
    pub fn for_session(session: &ServerSession) -> Self {
        HttpPersistentSettings {
            keepalive_timeout: session.get_keepalive(),
            keepalive_reuses_remaining: session.get_keepalive_reuses_remaining(),
            user_context: None,
            pipelined_prefix: None,
            pipelining_enabled: session.pipelining_enabled(),
        }
    }

    /// Set a user-defined context to be carried to the next request on this connection.
    pub fn set_user_context(&mut self, ctx: Box<dyn Any + Send + Sync>) {
        self.user_context = Some(ctx);
    }

    /// Take the user-defined context, if any.
    pub fn take_user_context(&mut self) -> Option<Box<dyn Any + Send + Sync>> {
        self.user_context.take()
    }

    /// Set pipelined-prefix bytes to be fed to the next session on this
    /// connection. Called by the proxy layer when HTTP/1.1 pipelining is
    /// enabled on the current session and overread bytes were present at
    /// reuse time.
    pub fn set_pipelined_prefix(&mut self, prefix: BytesMut) {
        self.pipelined_prefix = Some(prefix);
    }

    pub fn apply_to_session(self, session: &mut ServerSession) {
        let Self {
            keepalive_timeout,
            mut keepalive_reuses_remaining,
            user_context,
            pipelined_prefix,
            pipelining_enabled,
        } = self;

        // Reduce the number of times the connection for this session can be
        // reused by one. A session with reuse count of zero won't be reused
        if let Some(reuses) = keepalive_reuses_remaining.as_mut() {
            *reuses = reuses.saturating_sub(1);
        }

        session.set_keepalive(keepalive_timeout);
        session.set_keepalive_reuses_remaining(keepalive_reuses_remaining);

        // Carry user context into the session for the proxy layer to consume
        session.set_connection_user_context(user_context);

        // Replay pipelining opt-in so it stays on across keepalive reuses.
        session.set_pipelining_enabled(pipelining_enabled);

        // Feed any pipelined prefix bytes to the new session's request parser
        // so they are treated as the start of the next request.
        if let Some(prefix) = pipelined_prefix {
            session.set_pipelined_prefix(prefix);
        }
    }
}

#[derive(Debug)]
pub struct ReusedHttpStream {
    stream: Stream,
    persistent_settings: Option<HttpPersistentSettings>,
}

impl ReusedHttpStream {
    pub fn new(stream: Stream, persistent_settings: Option<HttpPersistentSettings>) -> Self {
        ReusedHttpStream {
            stream,
            persistent_settings,
        }
    }

    /// Build a reusable HTTP stream from a finished session, preserving any
    /// pipelined prefix bytes in the persistent settings for the next request.
    pub fn from_reusable_stream(
        reusable: ReusableHttpStream,
        mut persistent_settings: HttpPersistentSettings,
    ) -> Self {
        let (stream, pipelined_prefix) = reusable.into_parts();
        if let Some(prefix) = pipelined_prefix {
            persistent_settings.set_pipelined_prefix(prefix);
        }
        Self::new(stream, Some(persistent_settings))
    }

    pub fn consume(self) -> (Stream, Option<HttpPersistentSettings>) {
        (self.stream, self.persistent_settings)
    }
}

/// This trait defines the interface of an HTTP application.
#[async_trait]
pub trait HttpServerApp {
    /// Similar to the [`ServerApp`], this function is called whenever a new HTTP session is established.
    ///
    /// After successful processing, [`ServerSession::finish()`] can be
    /// called to return an optionally reusable connection back to the service.
    /// The caller needs to make sure that the connection is in a reusable state
    /// i.e., no error or incomplete read or write headers or bodies. Otherwise
    /// a `None` should be returned.
    async fn process_new_http(
        self: &Arc<Self>,
        mut session: ServerSession,
        // TODO: make this ShutdownWatch so that all task can await on this event
        shutdown: &ShutdownWatch,
    ) -> Option<ReusedHttpStream>;

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

    #[doc(hidden)]
    async fn process_custom_session(
        self: Arc<Self>,
        _stream: Stream,
        _shutdown: &ShutdownWatch,
    ) -> Option<Stream> {
        None
    }
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
        let mut h2c = self.server_options().as_ref().map_or(false, |o| o.h2c);
        let custom = self
            .server_options()
            .as_ref()
            .map_or(false, |o| o.force_custom);

        // h2c is for cleartext connections; on TLS, ALPN handles protocol negotiation.
        // Otherwise, h2c stays true on TLS streams, forcing HTTP/1.1 clients into HTTP/2
        if stream.get_ssl_digest().is_some() {
            h2c = false;
        }
        // try to read h2 preface
        else if h2c && !custom {
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
                h2c = buf == H2_PREFACE;
            }
        }
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
            let h2_conn = match server::handshake(stream, h2_options).await {
                Err(e) => {
                    error!("H2 handshake error {e}");
                    return None;
                }
                Ok(c) => c,
            };

            // The accept-loop body — including the graceful-shutdown state
            // machine — lives in `server::accept_downstream_sessions` so that
            // the same code path is exercised by tests in `protocols::http::v2`.
            let app = self.clone();
            let shutdown_for_session = shutdown.clone();
            server::accept_downstream_sessions(h2_conn, digest, shutdown.clone(), |h2_stream| {
                let app = app.clone();
                let shutdown = shutdown_for_session.clone();
                pingora_runtime::current_handle().spawn(async move {
                    // Note, `PersistentSettings` not currently relevant for h2
                    app.process_new_http(ServerSession::new_http2(h2_stream), &shutdown)
                        .await;
                });
            })
            .await;
        } else if custom || matches!(stream.selected_alpn_proto(), Some(ALPN::Custom(_))) {
            return self.clone().process_custom_session(stream, shutdown).await;
        } else {
            // No ALPN or ALPN::H1 and h2c was not configured, fallback to HTTP/1.1
            let mut session = ServerSession::new_http1(stream);
            if *shutdown.borrow() {
                // stop downstream from reusing if this service is shutting down soon
                session.set_keepalive(None);
            } else {
                // default 60s
                session.set_keepalive(Some(60));
            }
            session.set_keepalive_reuses_remaining(
                self.server_options()
                    .and_then(|opts| opts.keepalive_request_limit),
            );

            let mut result = self.process_new_http(session, shutdown).await;
            while let Some((stream, persistent_settings)) = result.map(|r| r.consume()) {
                let mut session = ServerSession::new_http1(stream);
                if let Some(persistent_settings) = persistent_settings {
                    persistent_settings.apply_to_session(&mut session);
                }

                result = self.process_new_http(session, shutdown).await;
            }
        }
        None
    }

    async fn cleanup(&self) {
        self.http_cleanup().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_test::io::Builder;

    #[test]
    fn test_persistent_settings_user_context_roundtrip() {
        // Create a mock H1 session
        let mock_io = Builder::new().build();
        let mut session = ServerSession::new_http1(Box::new(mock_io));
        session.set_keepalive(Some(60));

        // Snapshot settings (no user context yet)
        let mut settings = HttpPersistentSettings::for_session(&session);
        assert!(settings.take_user_context().is_none());

        // Set user context
        settings.set_user_context(Box::new(123u64));

        // Apply to a fresh session -- user context should transfer
        let mock_io2 = Builder::new().build();
        let mut session2 = ServerSession::new_http1(Box::new(mock_io2));
        settings.apply_to_session(&mut session2);

        // The user context should now be on the session
        let ctx = session2.take_connection_user_context();
        assert!(ctx.is_some());
        let val = ctx.unwrap().downcast::<u64>().unwrap();
        assert_eq!(*val, 123u64);

        // Keepalive should also have been applied
        assert_eq!(session2.get_keepalive(), Some(60));
    }

    #[test]
    fn test_persistent_settings_no_user_context_by_default() {
        let mock_io = Builder::new().build();
        let mut session = ServerSession::new_http1(Box::new(mock_io));
        session.set_keepalive(Some(30));

        let settings = HttpPersistentSettings::for_session(&session);

        let mock_io2 = Builder::new().build();
        let mut session2 = ServerSession::new_http1(Box::new(mock_io2));
        settings.apply_to_session(&mut session2);

        // No user context should be present
        assert!(session2.take_connection_user_context().is_none());
        // Keepalive should still work
        assert_eq!(session2.get_keepalive(), Some(30));
    }
}
