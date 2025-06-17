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

use async_trait::async_trait;
use clap::Parser;

use bytes::Bytes;
use http::header::{CONTENT_LENGTH, CONTENT_TYPE, TRANSFER_ENCODING};
use pingora_core::{
    prelude::Opt, protocols::ALPN, server::Server, upstreams::peer::HttpPeer, Error, ErrorSource,
    ErrorType, Result,
};
use pingora_http::ResponseHeader;
use pingora_proxy::{ProxyHttp, Session};

const GRPC: &str = "application/grpc";
const GRPC_WEB_TEXT: &str = "application/grpc-web-text";

pub struct GrpcWebTextProxy;

#[async_trait]
impl ProxyHttp for GrpcWebTextProxy {
    type CTX = ();

    fn new_ctx(&self) -> Self::CTX {}

    async fn request_filter(&self, session: &mut Session, _ctx: &mut Self::CTX) -> Result<bool> {
        let header = session.req_header();
        let content_type = header
            .headers
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();

        if content_type != GRPC_WEB_TEXT {
            let _ = session
                .respond_error_with_body(400, "Not a grpc-web-text request".into())
                .await?;
            return Ok(true);
        }

        Ok(false)
    }

    async fn upstream_request_filter(
        &self,
        _session: &mut Session,
        req: &mut pingora_http::RequestHeader,
        _ctx: &mut Self::CTX,
    ) -> Result<()> {
        req.insert_header(CONTENT_TYPE, GRPC)
            .expect("insert header");

        // The 'te' request header is used to detect incompatible proxies
        // which are supposed to remove 'te' if it is unsupported.
        // This header is required by gRPC over h2 protocol.
        // https://github.com/grpc/grpc/blob/master/doc/PROTOCOL-HTTP2.md
        req.insert_header("te", "trailers").expect("insert header");

        req.remove_header(&CONTENT_LENGTH);

        // For gRPC requests, EOS (end-of-stream) is indicated by the presence of the
        // END_STREAM flag on the last received DATA frame.
        // In scenarios where the Request stream needs to be closed
        // but no data remains to be sent implementations
        // MUST send an empty DATA frame with this flag set.
        req.set_send_end_stream(false);

        Ok(())
    }

    async fn request_body_filter(
        &self,
        _session: &mut Session,
        body: &mut Option<Bytes>,
        _end_of_stream: bool,
        _ctx: &mut Self::CTX,
    ) -> Result<()> {
        if let Some(b) = body {
            let decoded = base64::decode(&*b).map_err(|_| {
                Error::create(
                    ErrorType::Custom("Request body is not base64 encoding"),
                    ErrorSource::Downstream,
                    None,
                    None,
                )
            })?;

            *body = Some(Bytes::from(decoded));
        }

        Ok(())
    }

    async fn upstream_peer(
        &self,
        _session: &mut Session,
        _ctx: &mut Self::CTX,
    ) -> Result<Box<HttpPeer>> {
        let mut grpc_peer = Box::new(HttpPeer::new(
            ("127.0.0.1", 50051),
            false,
            "localhost".to_string(),
        ));

        grpc_peer.options.set_http_version(2, 0);
        grpc_peer.options.alpn = ALPN::H2;

        Ok(grpc_peer)
    }

    fn upstream_response_filter(
        &self,
        _session: &mut Session,
        headers: &mut ResponseHeader,
        _ctx: &mut Self::CTX,
    ) -> Result<()> {
        headers.remove_header(&CONTENT_TYPE);
        headers
            .insert_header(&CONTENT_TYPE, GRPC_WEB_TEXT)
            .expect("insert header");
        headers.remove_header(&TRANSFER_ENCODING);

        Ok(())
    }

    fn upstream_response_body_filter(
        &self,
        _session: &mut Session,
        body: &mut Option<Bytes>,
        _end_of_stream: bool,
        _ctx: &mut Self::CTX,
    ) -> Result<()> {
        if let Some(b) = body {
            let encoded_body = base64::encode(b);
            *body = Some(Bytes::from(encoded_body));
        }

        Ok(())
    }
}

fn main() {
    std::env::set_var("RUST_LOG", "debug,pingora_core=debug,pingora_proxy=debug");
    env_logger::init();

    // read command line arguments
    let opt = Opt::parse();
    let mut my_server = Server::new(Some(opt)).unwrap();
    my_server.bootstrap();

    let mut my_proxy =
        pingora_proxy::http_proxy_service(&my_server.configuration, GrpcWebTextProxy);

    my_proxy.add_tcp("0.0.0.0:6194");

    my_server.add_service(my_proxy);
    my_server.run_forever();
}
