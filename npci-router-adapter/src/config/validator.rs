// Configuration validator module

use super::{RouterConfig, RoutingStrategy};
use anyhow::{anyhow, Result};
use std::collections::HashSet;

/// Validate the entire configuration
pub fn validate(config: &RouterConfig) -> Result<()> {
    validate_server(&config.server)?;
    validate_tenants(config)?;
    validate_products(config)?;
    validate_rate_limiting(&config.rate_limiting)?;
    validate_bandwidth(&config.bandwidth)?;
    validate_health_check(&config.health_check)?;
    validate_monitoring(&config.monitoring)?;
    validate_tls(&config.tls)?;

    Ok(())
}

/// Validate server configuration
fn validate_server(server: &super::ServerConfig) -> Result<()> {
    if server.name.is_empty() {
        return Err(anyhow!("Server name cannot be empty"));
    }

    if server.threads == 0 {
        return Err(anyhow!("Number of threads must be greater than 0"));
    }

    if server.connection_timeout_secs == 0 {
        return Err(anyhow!("Connection timeout must be greater than 0"));
    }

    if server.max_connections == 0 {
        return Err(anyhow!("Max connections must be greater than 0"));
    }

    Ok(())
}

/// Validate tenant configurations
fn validate_tenants(config: &RouterConfig) -> Result<()> {
    let mut nat_ips = HashSet::new();
    let mut tenant_ids = HashSet::new();

    for (key, tenant) in &config.tenants {
        // Check tenant ID matches key
        if &tenant.id != key {
            return Err(anyhow!(
                "Tenant ID '{}' does not match key '{}'",
                tenant.id,
                key
            ));
        }

        // Check for duplicate tenant IDs
        if !tenant_ids.insert(&tenant.id) {
            return Err(anyhow!("Duplicate tenant ID: {}", tenant.id));
        }

        // Check for duplicate NAT IPs
        if !nat_ips.insert(&tenant.nat_ip) {
            return Err(anyhow!("Duplicate NAT IP: {}", tenant.nat_ip));
        }

        // Validate tenant name
        if tenant.name.is_empty() {
            return Err(anyhow!("Tenant name cannot be empty for tenant: {}", tenant.id));
        }

        // Validate certificate paths
        if !tenant.certificate_path.exists() {
            return Err(anyhow!(
                "Certificate file not found for tenant {}: {:?}",
                tenant.id,
                tenant.certificate_path
            ));
        }

        if !tenant.key_path.exists() {
            return Err(anyhow!(
                "Key file not found for tenant {}: {:?}",
                tenant.id,
                tenant.key_path
            ));
        }

        // Validate backends
        if tenant.backends.is_empty() {
            return Err(anyhow!("Tenant {} has no backends configured", tenant.id));
        }

        for backend in &tenant.backends {
            validate_backend(backend, &tenant.id)?;
        }

        // Validate routing configuration
        validate_routing(&tenant.routing, &tenant.id)?;

        // Validate SNI configuration
        validate_sni_config(&tenant.sni, &tenant.id)?;
    }

    Ok(())
}

/// Validate product configurations
fn validate_products(config: &RouterConfig) -> Result<()> {
    let mut product_ids = HashSet::new();

    for (key, product) in &config.products {
        // Check product ID matches key
        if &product.id != key {
            return Err(anyhow!(
                "Product ID '{}' does not match key '{}'",
                product.id,
                key
            ));
        }

        // Check for duplicate product IDs
        if !product_ids.insert(&product.id) {
            return Err(anyhow!("Duplicate product ID: {}", product.id));
        }

        // Validate product name
        if product.name.is_empty() {
            return Err(anyhow!("Product name cannot be empty for product: {}", product.id));
        }

        // Validate IPs
        if product.ips.is_empty() && product.dns.is_empty() {
            return Err(anyhow!(
                "Product {} has no IPs or DNS entries configured",
                product.id
            ));
        }

        // Validate backends
        if product.backends.is_empty() {
            return Err(anyhow!("Product {} has no backends configured", product.id));
        }

        for backend in &product.backends {
            validate_backend(backend, &product.id)?;
        }
    }

    Ok(())
}

/// Validate backend configuration
fn validate_backend(backend: &super::BackendConfig, parent: &str) -> Result<()> {
    if backend.id.is_empty() {
        return Err(anyhow!("Backend ID cannot be empty for parent: {}", parent));
    }

    if backend.address.is_empty() {
        return Err(anyhow!(
            "Backend address cannot be empty for backend: {}",
            backend.id
        ));
    }

    // Validate address format (should be IP:port or hostname:port)
    if !backend.address.contains(':') {
        return Err(anyhow!(
            "Backend address must include port for backend: {}",
            backend.id
        ));
    }

    if backend.weight == 0 {
        return Err(anyhow!(
            "Backend weight must be greater than 0 for backend: {}",
            backend.id
        ));
    }

    if backend.pool_size == 0 {
        return Err(anyhow!(
            "Backend pool size must be greater than 0 for backend: {}",
            backend.id
        ));
    }

    Ok(())
}

/// Validate routing configuration
fn validate_routing(routing: &super::RoutingConfig, tenant_id: &str) -> Result<()> {
    match routing.strategy {
        RoutingStrategy::Header => {
            if routing.header_name.is_none() {
                return Err(anyhow!(
                    "Header name must be specified when using header routing strategy for tenant: {}",
                    tenant_id
                ));
            }
        }
        RoutingStrategy::Path => {
            if routing.path_prefix.is_none() {
                return Err(anyhow!(
                    "Path prefix must be specified when using path routing strategy for tenant: {}",
                    tenant_id
                ));
            }
        }
        RoutingStrategy::Combined => {
            if routing.header_name.is_none() && routing.path_prefix.is_none() {
                return Err(anyhow!(
                    "At least one routing criterion must be specified for combined strategy for tenant: {}",
                    tenant_id
                ));
            }
        }
        RoutingStrategy::Ip => {
            // IP routing doesn't require additional configuration
        }
    }

    Ok(())
}

/// Validate rate limiting configuration
fn validate_rate_limiting(config: &super::RateLimitConfig) -> Result<()> {
    if config.enabled && config.default_rps <= 0 {
        return Err(anyhow!("Default RPS must be greater than 0 when rate limiting is enabled"));
    }

    if config.window_secs == 0 {
        return Err(anyhow!("Rate limit window must be greater than 0"));
    }

    Ok(())
}

/// Validate bandwidth configuration
fn validate_bandwidth(config: &super::BandwidthConfig) -> Result<()> {
    if config.enabled && config.report_interval_secs == 0 {
        return Err(anyhow!("Bandwidth report interval must be greater than 0"));
    }

    Ok(())
}

/// Validate health check configuration
fn validate_health_check(config: &super::HealthCheckConfig) -> Result<()> {
    if config.enabled {
        if config.interval_secs == 0 {
            return Err(anyhow!("Health check interval must be greater than 0"));
        }

        if config.timeout_secs == 0 {
            return Err(anyhow!("Health check timeout must be greater than 0"));
        }

        if config.timeout_secs >= config.interval_secs {
            return Err(anyhow!("Health check timeout must be less than interval"));
        }

        if config.failure_threshold == 0 {
            return Err(anyhow!("Health check failure threshold must be greater than 0"));
        }

        if config.success_threshold == 0 {
            return Err(anyhow!("Health check success threshold must be greater than 0"));
        }
    }

    Ok(())
}

/// Validate monitoring configuration
fn validate_monitoring(config: &super::MonitoringConfig) -> Result<()> {
    if config.enabled && config.metrics_port == 0 {
        return Err(anyhow!("Metrics port must be greater than 0"));
    }

    Ok(())
}

/// Validate TLS configuration
fn validate_tls(config: &super::TlsConfig) -> Result<()> {
    use super::SniMode;

    // Validate TLS version
    let valid_versions = vec!["1.0", "1.1", "1.2", "1.3"];
    if !valid_versions.contains(&config.min_version.as_str()) {
        return Err(anyhow!(
            "Invalid TLS version: {}. Must be one of: {:?}",
            config.min_version,
            valid_versions
        ));
    }

    // Validate mTLS configuration
    if config.mtls && config.ca_cert_path.is_none() {
        return Err(anyhow!("CA certificate path must be specified when mTLS is enabled"));
    }

    if let Some(ca_path) = &config.ca_cert_path {
        if !ca_path.exists() {
            return Err(anyhow!("CA certificate file not found: {:?}", ca_path));
        }
    }

    // Validate SNI configuration
    if config.sni_mode == SniMode::Strict && !config.prefer_sni {
        return Err(anyhow!("Strict SNI mode requires prefer_sni to be enabled"));
    }

    Ok(())
}

/// Validate SNI configuration for a tenant
fn validate_sni_config(sni: &super::SniConfig, tenant_id: &str) -> Result<()> {
    if sni.enabled && sni.hostnames.is_empty() {
        return Err(anyhow!(
            "SNI is enabled but no hostnames specified for tenant: {}",
            tenant_id
        ));
    }

    // Validate hostname format
    for hostname in &sni.hostnames {
        if hostname.is_empty() {
            return Err(anyhow!(
                "Empty hostname in SNI configuration for tenant: {}",
                tenant_id
            ));
        }

        // Basic hostname validation (could be more strict)
        if hostname.contains(' ') || hostname.starts_with('.') || hostname.ends_with('.') {
            return Err(anyhow!(
                "Invalid hostname format '{}' for tenant: {}",
                hostname,
                tenant_id
            ));
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::net::IpAddr;

    #[test]
    fn test_validate_server() {
        let mut server = super::super::ServerConfig {
            name: "test".to_string(),
            threads: 4,
            ingress_ip: "0.0.0.0".parse().unwrap(),
            ingress_port: 443,
            egress_ip: "0.0.0.0".parse().unwrap(),
            egress_port: 443,
            enable_h2: true,
            connection_timeout_secs: 30,
            request_timeout_secs: 30,
            keepalive_timeout_secs: 60,
            max_connections: 10000,
        };

        assert!(validate_server(&server).is_ok());

        server.threads = 0;
        assert!(validate_server(&server).is_err());
    }
}
