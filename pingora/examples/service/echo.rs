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

use crate::app::echo::{EchoApp, HttpEchoApp};
use pingora::services::listening::Service;

pub fn echo_service() -> Service<EchoApp> {
    Service::new("Echo Service".to_string(), EchoApp)
}

pub fn echo_service_http() -> Service<HttpEchoApp> {
    Service::new("Echo Service HTTP".to_string(), HttpEchoApp)
}
