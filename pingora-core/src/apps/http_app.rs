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

//! A simple HTTP application trait that maps a request to a response

use async_trait::async_trait;
use http::Response;
use log::{debug, error, trace};
use pingora_http::ResponseHeader;
use std::sync::Arc;

use crate::apps::HttpServerApp;
use crate::modules::http::{HttpModules, ModuleBuilder};
use crate::protocols::http::HttpTask;
use crate::protocols::http::ServerSession;
use crate::protocols::Stream;
use crate::server::ShutdownWatch;

/// This trait defines how to map a request to a response
#[async_trait]
pub trait ServeHttp {
    /// Define the mapping from a request to a response.
    /// Note that the request header is already read, but the implementation needs to read the
    /// request body if any.
    ///
    /// # Limitation
    /// In this API, the entire response has to be generated before the end of this call.
    /// So it is not suitable for streaming response or interactive communications.
    /// Users need to implement their own [`super::HttpServerApp`] for those use cases.
    async fn response(&self, http_session: &mut ServerSession) -> Response<Vec<u8>>;
}

// TODO: remove this in favor of HttpServer?
#[async_trait]
impl<SV> HttpServerApp for SV
where
    SV: ServeHttp + Send + Sync,
{
    async fn process_new_http(
        self: &Arc<Self>,
        mut http: ServerSession,
        shutdown: &ShutdownWatch,
    ) -> Option<Stream> {
        match http.read_request().await {
            Ok(res) => match res {
                false => {
                    debug!("Failed to read request header");
                    return None;
                }
                true => {
                    debug!("Successfully get a new request");
                }
            },
            Err(e) => {
                error!("HTTP server fails to read from downstream: {e}");
                return None;
            }
        }
        trace!("{:?}", http.req_header());
        if *shutdown.borrow() {
            http.set_keepalive(None);
        } else {
            http.set_keepalive(Some(60));
        }
        let new_response = self.response(&mut http).await;
        let (parts, body) = new_response.into_parts();
        let resp_header: ResponseHeader = parts.into();
        match http.write_response_header(Box::new(resp_header)).await {
            Ok(()) => {
                debug!("HTTP response header done.");
            }
            Err(e) => {
                error!(
                    "HTTP server fails to write to downstream: {e}, {}",
                    http.request_summary()
                );
            }
        }
        if !body.is_empty() {
            // TODO: check if chunked encoding is needed
            match http.write_response_body(body.into(), true).await {
                Ok(_) => debug!("HTTP response written."),
                Err(e) => error!(
                    "HTTP server fails to write to downstream: {e}, {}",
                    http.request_summary()
                ),
            }
        }
        match http.finish().await {
            Ok(c) => c,
            Err(e) => {
                error!("HTTP server fails to finish the request: {e}");
                None
            }
        }
    }
}

/// A helper struct for HTTP server with http modules embedded
pub struct HttpServer<SV> {
    app: SV,
    modules: HttpModules,
}

impl<SV> HttpServer<SV> {
    /// Create a new [HttpServer] with the given app which implements [ServeHttp]
    pub fn new_app(app: SV) -> Self {
        HttpServer {
            app,
            modules: HttpModules::new(),
        }
    }

    /// Add [ModuleBuilder] to this [HttpServer]
    pub fn add_module(&mut self, module: ModuleBuilder) {
        self.modules.add_module(module)
    }
}

#[async_trait]
impl<SV> HttpServerApp for HttpServer<SV>
where
    SV: ServeHttp + Send + Sync,
{
    async fn process_new_http(
        self: &Arc<Self>,
        mut http: ServerSession,
        shutdown: &ShutdownWatch,
    ) -> Option<Stream> {
        match http.read_request().await {
            Ok(res) => match res {
                false => {
                    debug!("Failed to read request header");
                    return None;
                }
                true => {
                    debug!("Successfully get a new request");
                }
            },
            Err(e) => {
                error!("HTTP server fails to read from downstream: {e}");
                return None;
            }
        }
        trace!("{:?}", http.req_header());
        if *shutdown.borrow() {
            http.set_keepalive(None);
        } else {
            http.set_keepalive(Some(60));
        }
        let mut module_ctx = self.modules.build_ctx();
        let req = http.req_header_mut();
        module_ctx.request_header_filter(req).await.ok()?;
        let new_response = self.app.response(&mut http).await;
        let (parts, body) = new_response.into_parts();
        let mut resp_header: ResponseHeader = parts.into();
        module_ctx
            .response_header_filter(&mut resp_header, body.is_empty())
            .await
            .ok()?;

        let task = HttpTask::Header(Box::new(resp_header), body.is_empty());
        trace!("{task:?}");

        match http.response_duplex_vec(vec![task]).await {
            Ok(_) => {
                debug!("HTTP response header done.");
            }
            Err(e) => {
                error!(
                    "HTTP server fails to write to downstream: {e}, {}",
                    http.request_summary()
                );
            }
        }

        let mut body = Some(body.into());
        module_ctx.response_body_filter(&mut body, true).ok()?;

        let task = HttpTask::Body(body, true);

        trace!("{task:?}");

        // TODO: check if chunked encoding is needed
        match http.response_duplex_vec(vec![task]).await {
            Ok(_) => debug!("HTTP response written."),
            Err(e) => error!(
                "HTTP server fails to write to downstream: {e}, {}",
                http.request_summary()
            ),
        }
        match http.finish().await {
            Ok(c) => c,
            Err(e) => {
                error!("HTTP server fails to finish the request: {e}");
                None
            }
        }
    }
}
