// Header manipulation module

use crate::config::HeaderRules;
use crate::error::{RouterError, RouterResult};
use pingora_http::{RequestHeader, ResponseHeader};
use std::collections::HashMap;

/// Header manipulator for request/response processing
pub struct HeaderManipulator {
    rules: HeaderRules,
}

impl HeaderManipulator {
    /// Create a new header manipulator with rules
    pub fn new(rules: HeaderRules) -> Self {
        Self { rules }
    }

    /// Apply header rules to a request
    pub fn apply_request_headers(&self, headers: &mut RequestHeader) -> RouterResult<()> {
        // Remove headers
        for header_name in &self.rules.remove {
            headers.remove_header(header_name);
            log::debug!("Removed request header: {}", header_name);
        }

        // Add headers (only if not exists)
        for (name, value) in &self.rules.add {
            if headers.headers.get(name).is_none() {
                headers
                    .insert_header(name, value)
                    .map_err(|e| RouterError::InvalidRequest(format!("Failed to add header: {}", e)))?;
                log::debug!("Added request header: {} = {}", name, value);
            }
        }

        // Set headers (overwrite if exists)
        for (name, value) in &self.rules.set {
            headers.insert_header(name, value)
                .map_err(|e| RouterError::InvalidRequest(format!("Failed to set header: {}", e)))?;
            log::debug!("Set request header: {} = {}", name, value);
        }

        Ok(())
    }

    /// Apply header rules to a response
    pub fn apply_response_headers(&self, headers: &mut ResponseHeader) -> RouterResult<()> {
        // Remove response headers
        for header_name in &self.rules.response_remove {
            headers.remove_header(header_name);
            log::debug!("Removed response header: {}", header_name);
        }

        // Add response headers (only if not exists)
        for (name, value) in &self.rules.response_add {
            if headers.headers.get(name).is_none() {
                headers
                    .insert_header(name, value)
                    .map_err(|e| RouterError::Internal(format!("Failed to add response header: {}", e)))?;
                log::debug!("Added response header: {} = {}", name, value);
            }
        }

        Ok(())
    }

    /// Extract routing header value from request
    pub fn get_routing_header(&self, headers: &RequestHeader, header_name: &str) -> Option<String> {
        headers
            .headers
            .get(header_name)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string())
    }

    /// Add standard headers for NPCI compliance
    pub fn add_npci_headers(&self, headers: &mut RequestHeader, tenant_id: &str) -> RouterResult<()> {
        // Add X-Tenant-ID header
        headers
            .insert_header("X-Tenant-ID", tenant_id)
            .map_err(|e| RouterError::InvalidRequest(format!("Failed to add tenant ID header: {}", e)))?;

        // Add X-Forwarded-For if not present
        if headers.headers.get("X-Forwarded-For").is_none() {
            if let Some(client_ip) = self.extract_client_ip(headers) {
                headers
                    .insert_header("X-Forwarded-For", &client_ip)
                    .map_err(|e| RouterError::InvalidRequest(format!("Failed to add X-Forwarded-For: {}", e)))?;
            }
        }

        // Add X-Request-ID for tracing
        if headers.headers.get("X-Request-ID").is_none() {
            let request_id = uuid::Uuid::new_v4().to_string();
            headers
                .insert_header("X-Request-ID", &request_id)
                .map_err(|e| RouterError::InvalidRequest(format!("Failed to add X-Request-ID: {}", e)))?;
        }

        Ok(())
    }

    /// Add standard response headers
    pub fn add_standard_response_headers(&self, headers: &mut ResponseHeader) -> RouterResult<()> {
        // Add Server header
        if headers.headers.get("Server").is_none() {
            headers
                .insert_header("Server", "NPCI-Router/1.0")
                .map_err(|e| RouterError::Internal(format!("Failed to add Server header: {}", e)))?;
        }

        // Add security headers
        headers
            .insert_header("X-Content-Type-Options", "nosniff")
            .map_err(|e| RouterError::Internal(format!("Failed to add security header: {}", e)))?;

        headers
            .insert_header("X-Frame-Options", "DENY")
            .map_err(|e| RouterError::Internal(format!("Failed to add security header: {}", e)))?;

        headers
            .insert_header("X-XSS-Protection", "1; mode=block")
            .map_err(|e| RouterError::Internal(format!("Failed to add security header: {}", e)))?;

        Ok(())
    }

    /// Extract client IP from request
    fn extract_client_ip(&self, headers: &RequestHeader) -> Option<String> {
        // Try X-Real-IP first
        if let Some(ip) = headers.headers.get("X-Real-IP") {
            if let Ok(ip_str) = ip.to_str() {
                return Some(ip_str.to_string());
            }
        }

        // Try X-Forwarded-For
        if let Some(forwarded) = headers.headers.get("X-Forwarded-For") {
            if let Ok(forwarded_str) = forwarded.to_str() {
                // Get first IP in the list
                return forwarded_str.split(',').next().map(|s| s.trim().to_string());
            }
        }

        None
    }

    /// Validate required headers
    pub fn validate_required_headers(
        &self,
        headers: &RequestHeader,
        required: &[&str],
    ) -> RouterResult<()> {
        for header_name in required {
            if headers.headers.get(*header_name).is_none() {
                return Err(RouterError::InvalidRequest(format!(
                    "Missing required header: {}",
                    header_name
                )));
            }
        }
        Ok(())
    }

    /// Sanitize headers for security
    pub fn sanitize_headers(&self, headers: &mut RequestHeader) -> RouterResult<()> {
        // Remove potentially dangerous headers
        let dangerous_headers = vec![
            "X-Forwarded-Host",
            "X-Original-URL",
            "X-Rewrite-URL",
        ];

        for header in dangerous_headers {
            headers.remove_header(header);
        }

        Ok(())
    }
}

impl Default for HeaderManipulator {
    fn default() -> Self {
        Self {
            rules: HeaderRules::default(),
        }
    }
}

/// Header-based router for routing decisions
pub struct HeaderRouter {
    /// Routing rules: header value -> backend ID
    routes: HashMap<String, String>,

    /// Header name to use for routing
    header_name: String,

    /// Default backend ID
    default_backend: Option<String>,
}

impl HeaderRouter {
    /// Create a new header router
    pub fn new(header_name: String, routes: HashMap<String, String>, default_backend: Option<String>) -> Self {
        Self {
            header_name,
            routes,
            default_backend,
        }
    }

    /// Get backend ID based on header value
    pub fn route(&self, headers: &RequestHeader) -> Option<String> {
        // Extract header value
        let header_value = headers
            .headers
            .get(&self.header_name)
            .and_then(|v| v.to_str().ok())?;

        // Look up backend ID
        self.routes
            .get(header_value)
            .cloned()
            .or_else(|| self.default_backend.clone())
    }

    /// Add or update a route
    pub fn add_route(&mut self, header_value: String, backend_id: String) {
        self.routes.insert(header_value, backend_id);
    }

    /// Remove a route
    pub fn remove_route(&mut self, header_value: &str) {
        self.routes.remove(header_value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pingora_http::RequestHeader;

    fn create_test_header() -> RequestHeader {
        RequestHeader::build("GET", b"/test", None).unwrap()
    }

    #[test]
    fn test_add_headers() {
        let mut rules = HeaderRules::default();
        rules.add.insert("X-Custom".to_string(), "value".to_string());

        let manipulator = HeaderManipulator::new(rules);
        let mut headers = create_test_header();

        manipulator.apply_request_headers(&mut headers).unwrap();

        assert_eq!(
            headers.headers.get("X-Custom").unwrap().to_str().unwrap(),
            "value"
        );
    }

    #[test]
    fn test_remove_headers() {
        let mut rules = HeaderRules::default();
        rules.remove.push("User-Agent".to_string());

        let manipulator = HeaderManipulator::new(rules);
        let mut headers = create_test_header();
        headers.insert_header("User-Agent", "test").unwrap();

        manipulator.apply_request_headers(&mut headers).unwrap();

        assert!(headers.headers.get("User-Agent").is_none());
    }

    #[test]
    fn test_set_headers() {
        let mut rules = HeaderRules::default();
        rules.set.insert("Host".to_string(), "new-host".to_string());

        let manipulator = HeaderManipulator::new(rules);
        let mut headers = create_test_header();
        headers.insert_header("Host", "old-host").unwrap();

        manipulator.apply_request_headers(&mut headers).unwrap();

        assert_eq!(
            headers.headers.get("Host").unwrap().to_str().unwrap(),
            "new-host"
        );
    }

    #[test]
    fn test_header_router() {
        let mut routes = HashMap::new();
        routes.insert("api-key-1".to_string(), "backend1".to_string());
        routes.insert("api-key-2".to_string(), "backend2".to_string());

        let router = HeaderRouter::new(
            "X-API-Key".to_string(),
            routes,
            Some("default-backend".to_string()),
        );

        let mut headers = create_test_header();
        headers.insert_header("X-API-Key", "api-key-1").unwrap();

        let backend = router.route(&headers).unwrap();
        assert_eq!(backend, "backend1");

        // Test default backend
        let mut headers2 = create_test_header();
        headers2.insert_header("X-API-Key", "unknown-key").unwrap();

        let backend2 = router.route(&headers2).unwrap();
        assert_eq!(backend2, "default-backend");
    }
}
