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

use super::*;
use crate::protocols::http::bridge::grpc_web::GrpcWebCtx;
use std::ops::{Deref, DerefMut};

/// gRPC-web bridge module, this will convert
/// HTTP/1.1 gRPC-web requests to H2 gRPC requests
#[derive(Default)]
pub struct GrpcWebBridge(GrpcWebCtx);

impl Deref for GrpcWebBridge {
    type Target = GrpcWebCtx;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for GrpcWebBridge {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

#[async_trait]
impl HttpModule for GrpcWebBridge {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }

    async fn request_header_filter(&mut self, req: &mut RequestHeader) -> Result<()> {
        self.0.request_header_filter(req);
        Ok(())
    }

    async fn response_header_filter(
        &mut self,
        resp: &mut ResponseHeader,
        _end_of_stream: bool,
    ) -> Result<()> {
        self.0.response_header_filter(resp);
        Ok(())
    }

    fn response_trailer_filter(
        &mut self,
        trailers: &mut Option<Box<HeaderMap>>,
    ) -> Result<Option<Bytes>> {
        if let Some(trailers) = trailers {
            return self.0.response_trailer_filter(trailers);
        }
        Ok(None)
    }
}

/// The builder for gRPC-web bridge module
pub struct GrpcWeb;

impl HttpModuleBuilder for GrpcWeb {
    fn init(&self) -> Module {
        Box::new(GrpcWebBridge::default())
    }
}
