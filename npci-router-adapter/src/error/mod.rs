// Error handling module for NPCI Router

use std::fmt;
use thiserror::Error;

/// Router error types
#[derive(Error, Debug)]
pub enum RouterError {
    /// Configuration error
    #[error("Configuration error: {0}")]
    Config(String),

    /// TLS error
    #[error("TLS error: {0}")]
    Tls(String),

    /// Routing error
    #[error("Routing error: {0}")]
    Routing(String),

    /// Backend error
    #[error("Backend error: {0}")]
    Backend(String),

    /// Rate limit exceeded
    #[error("Rate limit exceeded for {0}")]
    RateLimitExceeded(String),

    /// Bandwidth limit exceeded
    #[error("Bandwidth limit exceeded for {0}")]
    BandwidthLimitExceeded(String),

    /// Tenant not found
    #[error("Tenant not found: {0}")]
    TenantNotFound(String),

    /// Product not found
    #[error("Product not found: {0}")]
    ProductNotFound(String),

    /// Backend not found
    #[error("Backend not found: {0}")]
    BackendNotFound(String),

    /// Health check failed
    #[error("Health check failed for {0}: {1}")]
    HealthCheckFailed(String, String),

    /// Connection error
    #[error("Connection error: {0}")]
    Connection(String),

    /// Timeout error
    #[error("Timeout error: {0}")]
    Timeout(String),

    /// Invalid request
    #[error("Invalid request: {0}")]
    InvalidRequest(String),

    /// Internal error
    #[error("Internal error: {0}")]
    Internal(String),

    /// IO error
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    /// Pingora error
    #[error("Pingora error: {0}")]
    Pingora(#[from] pingora_error::Error),
}

impl RouterError {
    /// Get HTTP status code for this error
    pub fn status_code(&self) -> u16 {
        match self {
            RouterError::Config(_) => 500,
            RouterError::Tls(_) => 500,
            RouterError::Routing(_) => 502,
            RouterError::Backend(_) => 502,
            RouterError::RateLimitExceeded(_) => 429,
            RouterError::BandwidthLimitExceeded(_) => 429,
            RouterError::TenantNotFound(_) => 404,
            RouterError::ProductNotFound(_) => 404,
            RouterError::BackendNotFound(_) => 502,
            RouterError::HealthCheckFailed(_, _) => 503,
            RouterError::Connection(_) => 502,
            RouterError::Timeout(_) => 504,
            RouterError::InvalidRequest(_) => 400,
            RouterError::Internal(_) => 500,
            RouterError::Io(_) => 500,
            RouterError::Pingora(_) => 500,
        }
    }

    /// Get error code (for logging and monitoring)
    pub fn error_code(&self) -> &'static str {
        match self {
            RouterError::Config(_) => "CONFIG_ERROR",
            RouterError::Tls(_) => "TLS_ERROR",
            RouterError::Routing(_) => "ROUTING_ERROR",
            RouterError::Backend(_) => "BACKEND_ERROR",
            RouterError::RateLimitExceeded(_) => "RATE_LIMIT_EXCEEDED",
            RouterError::BandwidthLimitExceeded(_) => "BANDWIDTH_LIMIT_EXCEEDED",
            RouterError::TenantNotFound(_) => "TENANT_NOT_FOUND",
            RouterError::ProductNotFound(_) => "PRODUCT_NOT_FOUND",
            RouterError::BackendNotFound(_) => "BACKEND_NOT_FOUND",
            RouterError::HealthCheckFailed(_, _) => "HEALTH_CHECK_FAILED",
            RouterError::Connection(_) => "CONNECTION_ERROR",
            RouterError::Timeout(_) => "TIMEOUT_ERROR",
            RouterError::InvalidRequest(_) => "INVALID_REQUEST",
            RouterError::Internal(_) => "INTERNAL_ERROR",
            RouterError::Io(_) => "IO_ERROR",
            RouterError::Pingora(_) => "PINGORA_ERROR",
        }
    }

    /// Check if error is retryable
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            RouterError::Backend(_)
                | RouterError::Connection(_)
                | RouterError::Timeout(_)
                | RouterError::HealthCheckFailed(_, _)
        )
    }
}

/// Result type for router operations
pub type RouterResult<T> = Result<T, RouterError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_error_status_codes() {
        assert_eq!(RouterError::RateLimitExceeded("test".to_string()).status_code(), 429);
        assert_eq!(RouterError::TenantNotFound("test".to_string()).status_code(), 404);
        assert_eq!(RouterError::Backend("test".to_string()).status_code(), 502);
    }

    #[test]
    fn test_error_codes() {
        assert_eq!(
            RouterError::RateLimitExceeded("test".to_string()).error_code(),
            "RATE_LIMIT_EXCEEDED"
        );
    }

    #[test]
    fn test_is_retryable() {
        assert!(RouterError::Backend("test".to_string()).is_retryable());
        assert!(!RouterError::RateLimitExceeded("test".to_string()).is_retryable());
    }
}
