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

//! An HTTP application that reports Prometheus metrics.

use async_trait::async_trait;
use http::Response;
use prometheus::{Encoder, TextEncoder};

use super::http_app::HttpServer;
use crate::apps::http_app::ServeHttp;
use crate::modules::http::compression::ResponseCompressionBuilder;
use crate::protocols::http::ServerSession;

/// An HTTP application that reports Prometheus metrics.
///
/// This application will report all the [static metrics](https://docs.rs/prometheus/latest/prometheus/index.html#static-metrics)
/// collected via the [Prometheus](https://docs.rs/prometheus/) crate;
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

/// The [HttpServer] for [PrometheusHttpApp]
///
/// This type provides the functionality of [PrometheusHttpApp] with compression enabled
pub type PrometheusServer = HttpServer<PrometheusHttpApp>;

impl PrometheusServer {
    pub fn new() -> Self {
        let mut server = Self::new_app(PrometheusHttpApp);
        // enable gzip level 7 compression
        server.add_module(ResponseCompressionBuilder::enable(7));
        server
    }
}
