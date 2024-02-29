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

//! HTTP compression filter

use super::*;
use crate::protocols::http::compression::ResponseCompressionCtx;

/// HTTP response compression module
pub struct ResponseCompression(ResponseCompressionCtx);

impl HttpModule for ResponseCompression {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }

    fn request_header_filter(&mut self, req: &mut RequestHeader) -> Result<()> {
        self.0.request_filter(req);
        Ok(())
    }

    fn response_filter(&mut self, t: &mut HttpTask) -> Result<()> {
        self.0.response_filter(t);
        Ok(())
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
            self.level, false,
        )))
    }

    fn order(&self) -> i16 {
        // run the response filter later than most others filters
        i16::MIN / 2
    }
}
