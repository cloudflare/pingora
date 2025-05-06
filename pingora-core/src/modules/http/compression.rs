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

//! HTTP compression filter

use super::*;
use crate::protocols::http::compression::ResponseCompressionCtx;
use std::ops::{Deref, DerefMut};

/// HTTP response compression module
pub struct ResponseCompression(ResponseCompressionCtx);

impl Deref for ResponseCompression {
    type Target = ResponseCompressionCtx;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for ResponseCompression {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

#[async_trait]
impl HttpModule for ResponseCompression {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }

    async fn request_header_filter(&mut self, req: &mut RequestHeader) -> Result<()> {
        self.0.request_filter(req);
        Ok(())
    }

    async fn response_header_filter(
        &mut self,
        resp: &mut ResponseHeader,
        end_of_stream: bool,
    ) -> Result<()> {
        self.0.response_header_filter(resp, end_of_stream);
        Ok(())
    }

    fn response_body_filter(
        &mut self,
        body: &mut Option<Bytes>,
        end_of_stream: bool,
    ) -> Result<()> {
        if !self.0.is_enabled() {
            return Ok(());
        }
        let compressed = self.0.response_body_filter(body.as_ref(), end_of_stream);
        if compressed.is_some() {
            *body = compressed;
        }
        Ok(())
    }

    fn response_done_filter(&mut self) -> Result<Option<Bytes>> {
        if !self.0.is_enabled() {
            return Ok(None);
        }
        // Flush or finish any remaining encoded bytes upon HTTP response completion
        // (if it was not already ended in the body filter).
        Ok(self.0.response_body_filter(None, true))
    }
}

/// The builder for HTTP response compression module
pub struct ResponseCompressionBuilder {
    level: u32,
}

impl ResponseCompressionBuilder {
    /// Return a [ModuleBuilder] for [ResponseCompression] with the given compression level
    pub fn enable(level: u32) -> ModuleBuilder {
        Box::new(ResponseCompressionBuilder { level })
    }
}

impl HttpModuleBuilder for ResponseCompressionBuilder {
    fn init(&self) -> Module {
        Box::new(ResponseCompression(ResponseCompressionCtx::new(
            self.level, false, false,
        )))
    }

    fn order(&self) -> i16 {
        // run the response filter later than most others filters
        i16::MIN / 2
    }
}
