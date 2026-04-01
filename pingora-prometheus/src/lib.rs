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

#![warn(clippy::all)]

//! A Prometheus metrics HTTP server for [pingora](https://docs.rs/pingora) services.
//!
//! This crate provides [`PrometheusHttpApp`] and [`PrometheusServer`], which serve
//! all [static metrics](https://docs.rs/prometheus/latest/prometheus/index.html#static-metrics)
//! collected via the [`prometheus`] crate as an HTTP endpoint.
//!
//! # Example
//!
//! ```rust,ignore
//! use pingora_core::services::listening::Service;
//! use pingora_prometheus::new_prometheus_server;
//!
//! let mut prometheus_service = Service::new(
//!     "Prometheus HTTP".to_string(),
//!     new_prometheus_server(),
//! );
//! prometheus_service.add_tcp("127.0.0.1:6150");
//! server.add_service(prometheus_service);
//! ```
//!
//! Or use the convenience function:
//!
//! ```rust,ignore
//! let mut prometheus_service = pingora_prometheus::prometheus_http_service();
//! prometheus_service.add_tcp("127.0.0.1:6150");
//! server.add_service(prometheus_service);
//! ```

use async_trait::async_trait;
use http::Response;
use prometheus::{Encoder, TextEncoder};

use pingora_core::apps::http_app::{HttpServer, ServeHttp};
use pingora_core::modules::http::compression::ResponseCompressionBuilder;
use pingora_core::protocols::http::ServerSession;
use pingora_core::services::listening::Service;

/// Re-export of the [`prometheus`] crate.
///
/// Use this re-export to ensure your metrics are registered in the same
/// global registry that [`PrometheusHttpApp`] gathers from, avoiding
/// version mismatches that would cause metrics to silently not appear.
///
/// # Example
///
/// ```rust,ignore
/// use pingora_prometheus::prometheus::{self, register_int_counter, IntCounter};
/// use once_cell::sync::Lazy;
///
/// static REQUESTS: Lazy<IntCounter> = Lazy::new(|| {
///     register_int_counter!("requests_total", "Total requests").unwrap()
/// });
/// ```
pub use prometheus;

/// An HTTP application that reports Prometheus metrics.
///
/// This application will report all the [static metrics](https://docs.rs/prometheus/latest/prometheus/index.html#static-metrics)
/// collected via the [Prometheus](https://docs.rs/prometheus/) crate.
///
/// Currently serves metrics on all request paths. By convention, Prometheus
/// scrapers expect metrics at `/metrics`. Since this app is typically bound
/// to a dedicated listener address, this works in practice, but callers
/// should be aware of this if sharing the listener with other routes.
// TODO: consider restricting to `/metrics` and returning 404 for other paths
pub struct PrometheusHttpApp;

#[async_trait]
impl ServeHttp for PrometheusHttpApp {
    async fn response(&self, _http_session: &mut ServerSession) -> Response<Vec<u8>> {
        let encoder = TextEncoder::new();
        let metric_families = prometheus::gather();
        let mut buffer = vec![];
        encoder.encode(&metric_families, &mut buffer).unwrap();
        Response::builder()
            .status(200)
            .header(http::header::CONTENT_TYPE, encoder.format_type())
            .header(http::header::CONTENT_LENGTH, buffer.len())
            .body(buffer)
            .unwrap()
    }
}

/// The [`HttpServer`] for [`PrometheusHttpApp`].
///
/// This type provides the functionality of [`PrometheusHttpApp`] with gzip
/// compression enabled (level 7).
pub type PrometheusServer = HttpServer<PrometheusHttpApp>;

/// Create a new [`PrometheusServer`] with compression enabled.
pub fn new_prometheus_server() -> PrometheusServer {
    let mut server = PrometheusServer::new_app(PrometheusHttpApp);
    // enable gzip level 7 compression
    server.add_module(ResponseCompressionBuilder::enable(7));
    server
}

/// Create a Prometheus HTTP [`Service`] ready to have endpoints added.
///
/// This is a convenience function that creates a [`Service`] wrapping a
/// [`PrometheusServer`] with compression enabled.
///
/// # Example
///
/// ```rust,ignore
/// let mut prometheus_service = pingora_prometheus::prometheus_http_service();
/// prometheus_service.add_tcp("127.0.0.1:6150");
/// server.add_service(prometheus_service);
/// ```
pub fn prometheus_http_service() -> Service<PrometheusServer> {
    Service::new(
        "Prometheus metric HTTP".to_string(),
        new_prometheus_server(),
    )
}
