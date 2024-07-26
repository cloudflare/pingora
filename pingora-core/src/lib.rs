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

#![warn(clippy::all)]
#![allow(clippy::new_without_default)]
#![allow(clippy::type_complexity)]
#![allow(clippy::match_wild_err_arm)]
#![allow(clippy::missing_safety_doc)]
#![allow(clippy::upper_case_acronyms)]

//! # Pingora
//!
//! Pingora is a collection of service frameworks and network libraries battle-tested by the Internet.
//! It is to build robust, scalable and secure network infrastructures and services at Internet scale.
//!
//! # Features
//! - Http 1.x and Http 2
//! - Modern TLS with OpenSSL or BoringSSL (FIPS compatible)
//! - Zero downtime upgrade
//!
//! # Usage
//! This crate provides low level service and protocol implementation and abstraction.
//!
//! If looking to build a (reverse) proxy, see [`pingora-proxy`](https://docs.rs/pingora-proxy) crate.
//!
//! # Optional features
//! `boringssl`: Switch the internal TLS library from OpenSSL to BoringSSL.

pub mod apps;
pub mod connectors;
pub mod listeners;
pub mod modules;
pub mod protocols;
pub mod server;
pub mod services;
pub mod upstreams;
pub mod utils;

pub use pingora_error::{ErrorType::*, *};

// If both openssl and boringssl are enabled, prefer boringssl.
// This is to make sure that boringssl can override the default openssl feature
// when this crate is used indirectly by other crates.
#[cfg(feature = "boringssl")]
pub use pingora_boringssl as tls;

#[cfg(all(not(feature = "boringssl"), feature = "openssl"))]
pub use pingora_openssl as tls;

pub mod prelude {
    pub use crate::server::configuration::Opt;
    pub use crate::server::Server;
    pub use crate::services::background::background_service;
    pub use crate::upstreams::peer::HttpPeer;
    pub use pingora_error::{ErrorType::*, *};
}
