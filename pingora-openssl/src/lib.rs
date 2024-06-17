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

//! The OpenSSL API compatibility layer.
//!
//! This crate aims at making [openssl] APIs interchangeable with [boring](https://docs.rs/boring/latest/boring/).
//! In other words, this crate and [`pingora-boringssl`](https://docs.rs/pingora-boringssl) expose identical rust APIs.

#![warn(clippy::all)]

use openssl as ssl_lib;
pub use openssl_sys as ssl_sys;
pub use tokio_openssl as tokio_ssl;
pub mod ext;

// export commonly used libs
pub use ssl_lib::dh;
pub use ssl_lib::error;
pub use ssl_lib::hash;
pub use ssl_lib::nid;
pub use ssl_lib::pkey;
pub use ssl_lib::ssl;
pub use ssl_lib::x509;
