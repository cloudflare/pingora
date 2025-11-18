// NPCI Router Adapter - Production-grade multi-tenant router built on Pingora

#![doc = include_str!("../README.md")]
#![warn(missing_docs)]

pub mod bandwidth;
pub mod config;
pub mod error;
pub mod headers;
pub mod health;
pub mod metrics;
pub mod ratelimit;
pub mod router;
pub mod tls;

pub use config::RouterConfig;
pub use error::{RouterError, RouterResult};
pub use router::NpciRouter;

/// Prelude module for common imports
pub mod prelude {
    pub use crate::config::RouterConfig;
    pub use crate::error::{RouterError, RouterResult};
    pub use crate::router::builder::NpciRouterBuilder;
    pub use crate::router::NpciRouter;
}

/// Get version of the router
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Get router name
pub fn name() -> &'static str {
    env!("CARGO_PKG_NAME")
}
