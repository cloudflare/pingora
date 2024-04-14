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

//! The BoringSSL API compatibility layer.
//!
//! This crate aims at making [boring] APIs exchangeable with [openssl-rs](https://docs.rs/openssl/latest/openssl/).
//! In other words, this crate and [`pingora-openssl`](https://docs.rs/pingora-openssl) expose identical rust APIs.

#![warn(clippy::all)]

use boring as ssl_lib;
pub use boring_sys as ssl_sys;
pub mod boring_tokio;
pub use boring_tokio as tokio_ssl;
pub mod ext;

// export commonly used libs
pub use ssl_lib::error;
pub use ssl_lib::hash;
pub use ssl_lib::nid;
pub use ssl_lib::pkey;
pub use ssl_lib::ssl;
pub use ssl_lib::x509;
