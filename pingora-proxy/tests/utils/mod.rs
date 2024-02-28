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

#![allow(unused)]

pub mod cert;
pub mod mock_origin;
pub mod server_utils;
pub mod websocket;

use once_cell::sync::Lazy;
use tokio::runtime::{Builder, Runtime};

// for tests with a static connection pool, if we use tokio::test the reactor
// will no longer be associated with the backing pool fds since it's dropped per test
pub static GLOBAL_RUNTIME: Lazy<Runtime> =
    Lazy::new(|| Builder::new_multi_thread().enable_all().build().unwrap());

pub fn conf_dir() -> String {
    format!("{}/tests/utils/conf", env!("CARGO_MANIFEST_DIR"))
}
