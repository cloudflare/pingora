// Copyright 2026 Cloudflare, Inc.
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
//!
//! ## TLS backends (mutually exclusive)
//! - `openssl`: Use OpenSSL as the TLS library (default if no TLS feature is specified)
//! - `boringssl`: Use BoringSSL as the TLS library (FIPS compatible)
//! - `rustls`: Use Rustls as the TLS library
//!
//! ## Additional features
//! - `connection_filter`: Enable early TCP connection filtering before TLS handshake.
//!   This allows implementing custom logic to accept/reject connections based on peer address
//!   with zero overhead when disabled.
//! - `sentry`: Enable Sentry error reporting integration
//! - `patched_http1`: Enable patched HTTP/1 parser
//!
//! # Connection Filtering
//!
//! With the `connection_filter` feature enabled, you can implement early connection filtering
//! at the TCP level, before any TLS handshake or HTTP processing occurs. This is useful for:
//! - IP-based access control
//! - Rate limiting at the connection level
//! - Geographic restrictions
//! - DDoS mitigation
//!
//! ## Example
//!
//! ```rust,ignore
//! # #[cfg(feature = "connection_filter")]
//! # {
//! use async_trait::async_trait;
//! use pingora_core::listeners::ConnectionFilter;
//! use std::net::SocketAddr;
//! use std::sync::Arc;
//!
//! #[derive(Debug)]
//! struct MyFilter;
//!
//! #[async_trait]
//! impl ConnectionFilter for MyFilter {
//!     async fn should_accept(&self, addr: &SocketAddr) -> bool {
//!         // Custom logic to filter connections
//!         !is_blocked_ip(addr.ip())
//!     }
//! }
//!
//! // Apply the filter to a service
//! let mut service = my_service();
//! service.set_connection_filter(Arc::new(MyFilter));
//! # }
//! ```
//!
//! When the `connection_filter` feature is disabled, the filter API remains available
//! but becomes a no-op, ensuring zero overhead for users who don't need this functionality.

// This enables the feature that labels modules that are only available with
// certain pingora features
#![cfg_attr(docsrs, feature(doc_cfg))]

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

#[cfg(feature = "openssl")]
pub use pingora_openssl as tls;

#[cfg(feature = "rustls")]
pub use pingora_rustls as tls;

#[cfg(feature = "s2n")]
pub use pingora_s2n as tls;

#[cfg(not(feature = "any_tls"))]
pub use protocols::tls::noop_tls as tls;

pub mod prelude {
    pub use crate::server::configuration::Opt;
    pub use crate::server::Server;
    pub use crate::services::background::background_service;
    pub use crate::upstreams::peer::HttpPeer;
    pub use pingora_error::{ErrorType::*, *};
}
