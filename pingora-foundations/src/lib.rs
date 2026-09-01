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

//! [Foundations](https://docs.rs/foundations) telemetry integration for
//! [pingora](https://docs.rs/pingora) services.
//!
//! This crate provides a [`BackgroundService`]
//! that initializes foundations telemetry (logging, metrics, telemetry HTTP
//! server) and runs the [`TelemetryDriver`](foundations::telemetry::TelemetryDriver)
//! for the lifetime of the server.
//!
//! # Features
//!
//! - **`logging`**: Enables foundations' slog-based logging and bridges the
//!   Rust [`log`] crate so that existing `log::info!()` (etc.) call sites
//!   in pingora and your application route through foundations' logging
//!   pipeline (with rate limiting, redaction, JSON/text formatting, etc.).
//!
//! - **`sentry`**: Enables foundations' sentry integration. Use
//!   [`foundations::sentry::install_hook`] on your `ClientOptions` before
//!   passing them to [`Server::set_sentry_config`](pingora_core::server::Server::set_sentry_config)
//!   to add a `sentry_events_total` counter.
//!
//! - **`tracing`**: Enables foundations' distributed tracing via
//!   `cf-rustracing-jaeger`. Trace spans are reported to a Jaeger agent
//!   (UDP) or an OpenTelemetry collector (gRPC), configured through
//!   [`TracingSettings`](foundations::telemetry::settings::TracingSettings).
//!
//! - **`metrics`**: Enables foundations' metrics collection.
//!
//! - **`memory-profiling`**: Enables foundations' jemalloc allocator and
//!   memory profiler. Together with `telemetry-server`, this serves
//!   `/pprof/heap`, `/pprof/heap_stats`, and `/pprof/symbol`.
//!
//! - **`service`**: Enables the Pingora [`BackgroundService`]
//!   adapter. This is enabled by default.
//!
//! - **`telemetry-server`**: Runs foundations' built-in telemetry HTTP server
//!   (serving `/health`, `/metrics`, and feature-dependent debugging routes)
//!   as part of the background service. This is enabled by default.
//!
//! - **`full`**: Enables all of the above.
//!
//! # Usage
//!
//! ```rust,ignore
//! use pingora_core::prelude::background_service;
//! use pingora_core::server::Server;
//! use pingora_foundations::foundations;
//! use pingora_foundations::{FoundationsTelemetryConfig, FoundationsTelemetryService};
//!
//! let mut server = Server::new(None).unwrap();
//!
//! // Configure and add the telemetry service
//! let config = FoundationsTelemetryConfig::new(
//!     foundations::service_info!(),
//!     foundations::telemetry::settings::TelemetrySettings::default(),
//! );
//! let telemetry = FoundationsTelemetryService::new(config);
//! let telemetry_handle = server.add_service(
//!     background_service("Foundations Telemetry", telemetry),
//! );
//!
//! // Other services can depend on telemetry being ready:
//! // my_service_handle.add_dependency(&telemetry_handle);
//!
//! server.run_forever();
//! ```

#[cfg(not(any(
    feature = "logging",
    feature = "memory-profiling",
    feature = "metrics",
    feature = "sentry",
    feature = "tracing",
    feature = "telemetry-server",
)))]
compile_error!(
    "At least one pingora-foundations feature must be enabled: \
     \"logging\", \"memory-profiling\", \"metrics\", \"sentry\", \"tracing\", \
     or \"telemetry-server\""
);

#[cfg(feature = "service")]
use async_trait::async_trait;
#[cfg(feature = "service")]
use log::error;
#[cfg(feature = "service")]
use parking_lot::Mutex;

#[cfg(feature = "service")]
use pingora_core::server::ShutdownWatch;
#[cfg(feature = "service")]
use pingora_core::services::background::BackgroundService;
#[cfg(feature = "service")]
use pingora_core::services::ServiceReadyNotifier;

/// Re-export of the [`foundations`] crate for use with
/// [`foundations::service_info!`] and direct access to settings types.
pub use foundations;

/// Configuration for foundations telemetry integration.
#[non_exhaustive]
pub struct FoundationsTelemetryConfig {
    /// Service metadata used by foundations for metric prefixes, log context,
    /// and the telemetry server's `/health` endpoint.
    ///
    /// Use [`foundations::service_info!`] to populate this from `Cargo.toml`.
    pub service_info: foundations::ServiceInfo,

    /// Telemetry settings controlling logging, metrics, tracing, and the
    /// telemetry server. Which fields are present depends on which features
    /// are enabled.
    pub settings: foundations::telemetry::settings::TelemetrySettings,

    /// Additional metrics producers invoked for each `/metrics` scrape.
    ///
    /// Producers are registered after foundations initializes its metrics
    /// registry, preserving the configured service name.
    #[cfg(feature = "metrics")]
    pub metrics_producers: Vec<Box<dyn foundations::telemetry::metrics::ExtraProducer + 'static>>,

    /// Additional routes served by the foundations telemetry server.
    #[cfg(feature = "telemetry-server")]
    pub custom_server_routes: Vec<foundations::telemetry::TelemetryServerRoute>,
}

impl FoundationsTelemetryConfig {
    /// Create a telemetry configuration with no extra metrics producers or
    /// custom server routes.
    pub fn new(
        service_info: foundations::ServiceInfo,
        settings: foundations::telemetry::settings::TelemetrySettings,
    ) -> Self {
        Self {
            service_info,
            settings,
            #[cfg(feature = "metrics")]
            metrics_producers: Vec::new(),
            #[cfg(feature = "telemetry-server")]
            custom_server_routes: Vec::new(),
        }
    }
}

/// Initialize foundations telemetry subsystems.
///
/// Calls [`foundations::telemetry::init`] to set up the configured telemetry
/// subsystems (logging, metrics, telemetry server, etc.).
///
/// Returns the [`TelemetryDriver`](foundations::telemetry::TelemetryDriver)
/// which must be polled for the telemetry HTTP server to function.
pub fn init_telemetry(
    config: FoundationsTelemetryConfig,
) -> foundations::BootstrapResult<foundations::telemetry::TelemetryDriver> {
    let FoundationsTelemetryConfig {
        service_info,
        settings,
        #[cfg(feature = "metrics")]
        metrics_producers,
        #[cfg(feature = "telemetry-server")]
        custom_server_routes,
    } = config;

    let driver = foundations::telemetry::init(foundations::telemetry::TelemetryConfig {
        service_info: &service_info,
        settings: &settings,
        #[cfg(feature = "telemetry-server")]
        custom_server_routes,
    })?;

    #[cfg(feature = "metrics")]
    for producer in metrics_producers {
        foundations::telemetry::metrics::add_extra_producer(move |buffer: &mut Vec<u8>| {
            producer.produce(buffer);
        });
    }

    Ok(driver)
}

/// Set up the `slog-stdlog` bridge so that `log::info!()` etc. are routed
/// through foundations' slog logging pipeline.
///
/// Returns a guard that keeps the global logger active. The guard must be
/// held for as long as logging is needed — dropping it unsets the global
/// logger.
#[cfg(feature = "logging")]
#[must_use = "dropping this guard disables the log bridge"]
pub fn init_log_bridge() -> slog_scope::GlobalLoggerGuard {
    use std::ops::Deref;

    let logger_lock = foundations::telemetry::log::slog_logger();
    let logger: slog::Logger = (*logger_lock.read()).deref().clone();

    let guard = slog_scope::set_global_logger(logger);

    if let Err(e) = slog_stdlog::init() {
        eprintln!(
            "foundations: failed to set slog-stdlog as the log backend: {e}. \
             Another log backend may already be initialized."
        );
    }

    guard
}

/// A background service that initializes foundations telemetry and runs the
/// [`TelemetryDriver`](foundations::telemetry::TelemetryDriver).
///
/// Add this to a [`Server`](pingora_core::server::Server) as a service.
/// Other services that depend on telemetry being ready should declare a
/// dependency on this service's handle.
///
/// The service:
///
/// 1. Calls [`foundations::telemetry::init`] to set up logging, metrics, and
///    the telemetry HTTP server.
/// 2. When the `logging` feature is enabled, bridges the `log` crate to
///    foundations' slog pipeline via `slog-stdlog`.
/// 3. Signals readiness so dependent services can start.
/// 4. Polls the `TelemetryDriver` future until server shutdown.
///
/// If telemetry initialization fails, the service reports the error to Sentry
/// in release builds when enabled, logs it to stderr, and intentionally does
/// not signal readiness. Services that depend on this service remain blocked
/// until shutdown rather than starting without their required telemetry.
#[cfg(feature = "service")]
pub struct FoundationsTelemetryService {
    config: Mutex<Option<FoundationsTelemetryConfig>>,
}

#[cfg(feature = "service")]
impl FoundationsTelemetryService {
    /// Creates a telemetry service from the provided one-shot configuration.
    pub fn new(config: FoundationsTelemetryConfig) -> Self {
        Self {
            config: Mutex::new(Some(config)),
        }
    }
}

#[cfg(feature = "service")]
#[async_trait]
impl BackgroundService for FoundationsTelemetryService {
    async fn start_with_ready_notifier(
        &self,
        mut shutdown: ShutdownWatch,
        notifier: ServiceReadyNotifier,
    ) {
        let config = self.config.lock().take();
        let Some(config) = config else {
            eprintln!("foundations telemetry service cannot be started more than once");
            wait_for_shutdown(&mut shutdown).await;
            return;
        };

        let mut driver = match init_telemetry(config) {
            Ok(driver) => driver,
            Err(e) => {
                #[cfg(all(not(debug_assertions), feature = "sentry"))]
                {
                    let error: &(dyn std::error::Error + Send + Sync + 'static) = e.as_ref();
                    sentry::capture_error(error);
                }

                eprintln!("failed to initialize foundations telemetry: {e}");
                // Keep the notifier alive so dependent services remain blocked.
                wait_for_shutdown(&mut shutdown).await;
                return;
            }
        };

        // Bridge the log crate to foundations' slog pipeline. The guard must
        // live until shutdown to keep the bridge active.
        #[cfg(feature = "logging")]
        let _log_guard = init_log_bridge();

        // Telemetry is initialized; dependent services can start.
        notifier.notify_ready();

        // Wire the server's shutdown signal to the telemetry driver for
        // graceful shutdown of the telemetry HTTP server and trace reporter.
        let shutdown_signal = async move { wait_for_shutdown(&mut shutdown).await };
        driver.with_graceful_shutdown(shutdown_signal);

        // Run the driver until shutdown completes.
        if let Err(e) = driver.await {
            error!("Foundations telemetry driver error: {e}");
        }

        // _log_guard dropped here — process is shutting down.
    }
}

#[cfg(feature = "service")]
async fn wait_for_shutdown(shutdown: &mut ShutdownWatch) {
    if *shutdown.borrow_and_update() {
        return;
    }

    while shutdown.changed().await.is_ok() {
        if *shutdown.borrow_and_update() {
            return;
        }
    }
}

#[cfg(all(test, feature = "telemetry-server"))]
mod tests {
    use super::*;
    use foundations::telemetry::reexports::{
        http_body_util::{BodyExt, Full},
        hyper::{Method, Response},
    };
    use foundations::telemetry::settings::{TelemetryServerSettings, TelemetrySettings};
    use foundations::telemetry::{TelemetryRouteBody, TelemetryServerRoute};
    use std::net::{Ipv4Addr, SocketAddr};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;
    use tokio::sync::oneshot;
    use tokio::time::{timeout, Duration};

    #[tokio::test]
    async fn custom_routes_are_served_and_driver_shuts_down() {
        let settings = TelemetrySettings {
            server: TelemetryServerSettings {
                enabled: true,
                addr: SocketAddr::from((Ipv4Addr::LOCALHOST, 0)).into(),
            },
            ..Default::default()
        };
        let mut config = FoundationsTelemetryConfig::new(foundations::service_info!(), settings);
        config.metrics_producers = vec![Box::new(|buffer: &mut Vec<u8>| {
            buffer.extend_from_slice(b"custom_metric 1\n# EOF\n");
        })];
        config.custom_server_routes = vec![TelemetryServerRoute {
            path: "/custom".into(),
            methods: vec![Method::GET],
            handler: Box::new(|_, _| {
                Box::pin(async {
                    Ok(Response::new(TelemetryRouteBody::new(
                        Full::from("custom response").map_err(Into::into),
                    )))
                })
            }),
        }];

        let mut driver = init_telemetry(config).expect("telemetry should initialize");
        let addr = driver
            .server_addr()
            .expect("telemetry server should be enabled")
            .to_string()
            .parse::<SocketAddr>()
            .expect("test server should use a TCP address");
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        driver.with_graceful_shutdown(async move {
            let _ = shutdown_rx.await;
        });
        let driver_task = tokio::spawn(driver);

        let mut stream = TcpStream::connect(addr)
            .await
            .expect("telemetry server should accept connections");
        stream
            .write_all(b"GET /custom HTTP/1.0\r\nHost: localhost\r\n\r\n")
            .await
            .expect("request should be written");
        let mut response = Vec::new();
        stream
            .read_to_end(&mut response)
            .await
            .expect("response should be read");
        let response = String::from_utf8(response).expect("response should be UTF-8");
        assert!(response.starts_with("HTTP/1.0 200"));
        assert!(response.ends_with("custom response"));

        let mut stream = TcpStream::connect(addr)
            .await
            .expect("telemetry server should accept metrics connections");
        stream
            .write_all(b"GET /metrics HTTP/1.0\r\nHost: localhost\r\n\r\n")
            .await
            .expect("metrics request should be written");
        let mut response = Vec::new();
        stream
            .read_to_end(&mut response)
            .await
            .expect("metrics response should be read");
        let response = String::from_utf8(response).expect("metrics response should be UTF-8");
        assert!(response.contains("custom_metric 1\n"));
        assert_eq!(response.matches("# EOF\n").count(), 1);

        shutdown_tx
            .send(())
            .expect("driver should still be running");
        timeout(Duration::from_secs(5), driver_task)
            .await
            .expect("driver should shut down")
            .expect("driver task should not panic")
            .expect("driver should exit cleanly");
    }
}
