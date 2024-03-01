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
// This enables the feature that labels modules that are only available with
// certain pingora features
#![cfg_attr(docsrs, feature(doc_cfg))]

//! # Pingora
//!
//! Pingora is a framework to build fast, reliable and programmable networked systems at Internet scale.
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
//! # features
//! * `openssl`: Using OpenSSL as the internal TLS backend. This feature is default on.
//! * `boringssl`: Switch the internal TLS library from OpenSSL to BoringSSL. This feature will disable `openssl`.
//! * `proxy`: This feature will include and export `pingora_proxy::prelude::*`.
//! * `lb`: This feature will include and export `pingora_load_balancing::prelude::*`.
//! * `cache`: This feature will include and export `pingora_cache::prelude::*`.

pub use pingora_core::*;

/// HTTP header objects that preserve http header cases
pub mod http {
    pub use pingora_http::*;
}

#[cfg(feature = "cache")]
#[cfg_attr(docsrs, doc(cfg(feature = "cache")))]
/// Caching services and tooling
pub mod cache {
    pub use pingora_cache::*;
}

#[cfg(feature = "lb")]
#[cfg_attr(docsrs, doc(cfg(feature = "lb")))]
/// Load balancing recipes
pub mod lb {
    pub use pingora_load_balancing::*;
}

#[cfg(feature = "proxy")]
#[cfg_attr(docsrs, doc(cfg(feature = "proxy")))]
/// Proxying recipes
pub mod proxy {
    pub use pingora_proxy::*;
}

#[cfg(feature = "time")]
#[cfg_attr(docsrs, doc(cfg(feature = "time")))]
/// Timeouts and other useful time utilities
pub mod time {
    pub use pingora_timeout::*;
}

/// A useful set of types for getting started
pub mod prelude {
    pub use pingora_core::prelude::*;
    pub use pingora_http::prelude::*;
    pub use pingora_timeout::*;

    #[cfg(feature = "cache")]
    #[cfg_attr(docsrs, doc(cfg(feature = "cache")))]
    pub use pingora_cache::prelude::*;

    #[cfg(feature = "lb")]
    #[cfg_attr(docsrs, doc(cfg(feature = "lb")))]
    pub use pingora_load_balancing::prelude::*;

    #[cfg(feature = "proxy")]
    #[cfg_attr(docsrs, doc(cfg(feature = "proxy")))]
    pub use pingora_proxy::prelude::*;

    #[cfg(feature = "time")]
    #[cfg_attr(docsrs, doc(cfg(feature = "time")))]
    pub use pingora_timeout::*;
}
