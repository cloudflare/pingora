// Core router module - integrates all components

use crate::bandwidth::BandwidthTracker;
use crate::config::{BackendConfig, ProductConfig, RouterConfig, RoutingStrategy, TenantConfig};
use crate::error::{RouterError, RouterResult};
use crate::headers::HeaderManipulator;
use crate::health::{HealthChecker, HealthStatus};
use crate::metrics::MetricsCollector;
use crate::ratelimit::{RateLimiter, RateLimitAlgorithm};
use crate::tls::TlsCertificateManager;
use async_trait::async_trait;
use pingora_core::upstreams::peer::HttpPeer;
use pingora_http::{RequestHeader, ResponseHeader};
use pingora_load_balancing::{selection::RoundRobin, LoadBalancer};
use pingora_proxy::{ProxyHttp, Session};
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Instant;

/// Context for each request
pub struct RequestContext {
    /// Tenant ID
    pub tenant_id: Option<String>,

    /// Product ID
    pub product_id: Option<String>,

    /// Backend ID selected
    pub backend_id: Option<String>,

    /// Request start time
    pub start_time: Instant,

    /// Bytes received
    pub bytes_in: u64,

    /// Bytes sent
    pub bytes_out: u64,

    /// Whether rate limit was checked
    pub rate_limit_checked: bool,

    /// Request ID for tracing
    pub request_id: String,
}

impl RequestContext {
    fn new() -> Self {
        Self {
            tenant_id: None,
            product_id: None,
            backend_id: None,
            start_time: Instant::now(),
            bytes_in: 0,
            bytes_out: 0,
            rate_limit_checked: false,
            request_id: uuid::Uuid::new_v4().to_string(),
        }
    }
}

/// NPCI Router - main proxy implementation
pub struct NpciRouter {
    /// Configuration
    config: Arc<RouterConfig>,

    /// Load balancers per tenant
    load_balancers: Arc<parking_lot::RwLock<HashMap<String, Arc<LoadBalancer<RoundRobin>>>>>,

    /// TLS certificate manager
    tls_manager: Arc<TlsCertificateManager>,

    /// Rate limiter
    rate_limiter: Arc<RateLimiter>,

    /// Bandwidth tracker
    bandwidth_tracker: Arc<BandwidthTracker>,

    /// Health checker
    health_checker: Arc<HealthChecker>,
}

impl NpciRouter {
    /// Create a new NPCI router
    pub fn new(config: RouterConfig) -> RouterResult<Self> {
        let config = Arc::new(config);

        // Initialize TLS manager
        let tls_manager = Arc::new(TlsCertificateManager::new(config.tls.clone()));

        // Load certificates from tenants
        let tenants: Vec<_> = config.tenants.values().cloned().collect();
        tls_manager
            .load_from_tenants(&tenants)
            .map_err(|e| RouterError::Tls(e.to_string()))?;

        // Initialize rate limiter
        let rate_limiter = Arc::new(RateLimiter::new(
            config.rate_limiting.default_rps,
            config.rate_limiting.window_secs,
            match config.rate_limiting.algorithm {
                crate::config::RateLimitAlgorithm::TokenBucket => RateLimitAlgorithm::TokenBucket,
                crate::config::RateLimitAlgorithm::SlidingWindow => RateLimitAlgorithm::SlidingWindow,
                crate::config::RateLimitAlgorithm::FixedWindow => RateLimitAlgorithm::FixedWindow,
            },
        ));

        // Initialize bandwidth tracker
        let bandwidth_tracker = Arc::new(BandwidthTracker::new(
            config.bandwidth.report_interval_secs,
        ));

        // Initialize health checker
        let health_checker = Arc::new(HealthChecker::new(config.health_check.clone()));

        // Create load balancers for each tenant
        let load_balancers = Self::create_load_balancers(&config)?;

        // Start health checks
        if config.health_check.enabled {
            Self::start_health_checks(
                health_checker.clone(),
                &config.tenants,
                &config.products,
            );
        }

        // Start bandwidth tracking background task
        if config.bandwidth.enabled {
            bandwidth_tracker.clone().start_background_updater();
        }

        Ok(Self {
            config,
            load_balancers: Arc::new(parking_lot::RwLock::new(load_balancers)),
            tls_manager,
            rate_limiter,
            bandwidth_tracker,
            health_checker,
        })
    }

    /// Create load balancers for all tenants
    fn create_load_balancers(
        config: &RouterConfig,
    ) -> RouterResult<HashMap<String, Arc<LoadBalancer<RoundRobin>>>> {
        let mut load_balancers = HashMap::new();

        for (tenant_id, tenant) in &config.tenants {
            if !tenant.enabled {
                continue;
            }

            let backends: Vec<_> = tenant
                .backends
                .iter()
                .map(|b| (b.address.clone(), b.weight as usize))
                .collect();

            if backends.is_empty() {
                return Err(RouterError::Config(format!(
                    "No backends configured for tenant: {}",
                    tenant_id
                )));
            }

            let lb = LoadBalancer::try_from_iter(backends).map_err(|e| {
                RouterError::Config(format!("Failed to create load balancer: {}", e))
            })?;

            load_balancers.insert(tenant_id.clone(), Arc::new(lb));
        }

        for (product_id, product) in &config.products {
            let backends: Vec<_> = product
                .backends
                .iter()
                .map(|b| (b.address.clone(), b.weight as usize))
                .collect();

            if backends.is_empty() {
                continue;
            }

            let lb = LoadBalancer::try_from_iter(backends).map_err(|e| {
                RouterError::Config(format!("Failed to create load balancer: {}", e))
            })?;

            load_balancers.insert(product_id.clone(), Arc::new(lb));
        }

        Ok(load_balancers)
    }

    /// Start health checks for all backends
    fn start_health_checks(
        health_checker: Arc<HealthChecker>,
        tenants: &HashMap<String, TenantConfig>,
        products: &HashMap<String, ProductConfig>,
    ) {
        // Start health checks for tenant backends
        for tenant in tenants.values() {
            if !tenant.enabled {
                continue;
            }

            for backend in &tenant.backends {
                health_checker.clone().start_health_check(backend.clone());
            }
        }

        // Start health checks for product backends
        for product in products.values() {
            for backend in &product.backends {
                health_checker.clone().start_health_check(backend.clone());
            }
        }
    }

    /// Get tenant by IP address
    fn get_tenant_by_ip(&self, ip: &IpAddr) -> Option<&TenantConfig> {
        self.config.get_tenant_by_ip(ip)
    }

    /// Get tenant by routing strategy
    fn get_tenant_by_routing(&self, session: &Session, headers: &RequestHeader) -> Option<String> {
        // Try to find tenant based on different routing strategies
        for tenant in self.config.tenants.values() {
            if !tenant.enabled {
                continue;
            }

            match tenant.routing.strategy {
                RoutingStrategy::Ip => {
                    // Get client IP from session
                    if let Some(peer_addr) = session.client_addr() {
                        if let Some(ip) = peer_addr.as_inet() {
                            let client_ip: IpAddr = ip.ip().into();
                            if let Some(found_tenant) = self.get_tenant_by_ip(&client_ip) {
                                return Some(found_tenant.id.clone());
                            }
                        }
                    }
                }
                RoutingStrategy::Header => {
                    if let Some(header_name) = &tenant.routing.header_name {
                        let manipulator = HeaderManipulator::default();
                        if let Some(header_value) = manipulator.get_routing_header(headers, header_name) {
                            if header_value == tenant.id || header_value == tenant.name {
                                return Some(tenant.id.clone());
                            }
                        }
                    }
                }
                RoutingStrategy::Path => {
                    if let Some(path_prefix) = &tenant.routing.path_prefix {
                        let path = std::str::from_utf8(headers.uri.path().as_bytes()).unwrap_or("");
                        if path.starts_with(path_prefix) {
                            return Some(tenant.id.clone());
                        }
                    }
                }
                RoutingStrategy::Combined => {
                    // Try header first, then path, then IP
                    if let Some(header_name) = &tenant.routing.header_name {
                        let manipulator = HeaderManipulator::default();
                        if let Some(header_value) = manipulator.get_routing_header(headers, header_name) {
                            if header_value == tenant.id || header_value == tenant.name {
                                return Some(tenant.id.clone());
                            }
                        }
                    }

                    if let Some(path_prefix) = &tenant.routing.path_prefix {
                        let path = std::str::from_utf8(headers.uri.path().as_bytes()).unwrap_or("");
                        if path.starts_with(path_prefix) {
                            return Some(tenant.id.clone());
                        }
                    }
                }
            }
        }

        None
    }

    /// Select upstream backend for a tenant
    fn select_backend(&self, tenant_id: &str) -> RouterResult<(String, u16, bool, Option<String>)> {
        let load_balancers = self.load_balancers.read();
        let lb = load_balancers
            .get(tenant_id)
            .ok_or_else(|| RouterError::TenantNotFound(tenant_id.to_string()))?;

        let upstream = lb
            .select(tenant_id.as_bytes(), 256)
            .ok_or_else(|| RouterError::BackendNotFound(format!("No healthy backend for tenant: {}", tenant_id)))?;

        // Get backend config for additional details
        let tenant = self
            .config
            .get_tenant(tenant_id)
            .ok_or_else(|| RouterError::TenantNotFound(tenant_id.to_string()))?;

        let backend_config = tenant
            .backends
            .iter()
            .find(|b| b.address == upstream.to_string())
            .ok_or_else(|| RouterError::BackendNotFound(upstream.to_string()))?;

        // Parse address
        let parts: Vec<&str> = upstream.to_string().split(':').collect();
        let host = parts[0].to_string();
        let port = parts.get(1).and_then(|p| p.parse().ok()).unwrap_or(443);

        Ok((host, port, backend_config.tls, backend_config.sni.clone()))
    }

    /// Check if backend is healthy
    fn is_backend_healthy(&self, backend_id: &str) -> bool {
        self.health_checker.is_healthy(backend_id)
    }
}

#[async_trait]
impl ProxyHttp for NpciRouter {
    type CTX = RequestContext;

    fn new_ctx(&self) -> Self::CTX {
        RequestContext::new()
    }

    async fn request_filter(
        &self,
        session: &mut Session,
        ctx: &mut Self::CTX,
    ) -> pingora_core::Result<bool> {
        let headers = session.req_header();

        // Try to identify tenant
        let tenant_id = self.get_tenant_by_routing(session, headers);

        if let Some(tid) = &tenant_id {
            ctx.tenant_id = Some(tid.clone());

            // Get tenant config
            if let Some(tenant) = self.config.get_tenant(tid) {
                // Apply rate limiting
                if self.config.rate_limiting.enabled {
                    let custom_rps = tenant.rate_limit.as_ref().map(|rl| rl.rps);
                    if !self.rate_limiter.check_rate_limit(tid, custom_rps) {
                        ctx.rate_limit_checked = true;

                        // Record metrics
                        MetricsCollector::record_rate_limit_exceeded(tid);

                        // Return 429 Too Many Requests
                        let mut response = ResponseHeader::build(429, None).unwrap();
                        response
                            .insert_header("X-Rate-Limit-Retry-After", "1")
                            .unwrap();
                        response
                            .insert_header("X-Tenant-ID", tid)
                            .unwrap();

                        session.set_keepalive(None);
                        session
                            .write_response_header(Box::new(response), true)
                            .await?;

                        return Ok(true); // Request handled
                    }
                }

                // Apply header manipulation
                let manipulator = HeaderManipulator::new(tenant.headers.clone());
                let mut req_header = session.req_header_mut();
                manipulator
                    .apply_request_headers(req_header)
                    .map_err(|e| pingora_error::Error::because(pingora_error::ErrorType::InternalError, e.to_string(), e))?;

                manipulator
                    .add_npci_headers(req_header, tid)
                    .map_err(|e| pingora_error::Error::because(pingora_error::ErrorType::InternalError, e.to_string(), e))?;
            }
        }

        Ok(false) // Continue processing
    }

    async fn upstream_peer(
        &self,
        session: &mut Session,
        ctx: &mut Self::CTX,
    ) -> pingora_core::Result<Box<HttpPeer>> {
        let tenant_id = ctx
            .tenant_id
            .as_ref()
            .ok_or_else(|| {
                pingora_error::Error::new(pingora_error::ErrorType::InternalError)
            })?;

        let (host, port, use_tls, sni) = self
            .select_backend(tenant_id)
            .map_err(|e| pingora_error::Error::because(pingora_error::ErrorType::InternalError, e.to_string(), e))?;

        let sni_name = sni.unwrap_or_else(|| host.clone());
        let peer = Box::new(HttpPeer::new(
            (host.as_str(), port),
            use_tls,
            sni_name,
        ));

        Ok(peer)
    }

    async fn upstream_request_filter(
        &self,
        _session: &mut Session,
        upstream_request: &mut RequestHeader,
        ctx: &mut Self::CTX,
    ) -> pingora_core::Result<()> {
        // Add request ID for tracing
        upstream_request
            .insert_header("X-Request-ID", &ctx.request_id)
            .ok();

        Ok(())
    }

    async fn response_filter(
        &self,
        _session: &mut Session,
        upstream_response: &mut ResponseHeader,
        ctx: &mut Self::CTX,
    ) -> pingora_core::Result<()> {
        if let Some(tenant_id) = &ctx.tenant_id {
            if let Some(tenant) = self.config.get_tenant(tenant_id) {
                let manipulator = HeaderManipulator::new(tenant.headers.clone());
                manipulator
                    .apply_response_headers(upstream_response)
                    .map_err(|e| pingora_error::Error::because(pingora_error::ErrorType::InternalError, e.to_string(), e))?;

                manipulator
                    .add_standard_response_headers(upstream_response)
                    .map_err(|e| pingora_error::Error::because(pingora_error::ErrorType::InternalError, e.to_string(), e))?;
            }
        }

        Ok(())
    }

    async fn logging(
        &self,
        session: &mut Session,
        _e: Option<&pingora_error::Error>,
        ctx: &mut Self::CTX,
    ) {
        let duration = ctx.start_time.elapsed().as_secs_f64();
        let status = session
            .response_written()
            .map(|r| r.status.as_u16())
            .unwrap_or(0);

        let method = std::str::from_utf8(session.req_header().method.as_str().as_bytes())
            .unwrap_or("UNKNOWN");

        if let Some(tenant_id) = &ctx.tenant_id {
            // Record request metrics
            MetricsCollector::record_request(tenant_id, method, status, duration);

            // Record bandwidth
            if self.config.bandwidth.enabled {
                self.bandwidth_tracker
                    .record_ingress(tenant_id, ctx.bytes_in);
                self.bandwidth_tracker
                    .record_egress(tenant_id, ctx.bytes_out);

                MetricsCollector::record_bandwidth(tenant_id, ctx.bytes_in, ctx.bytes_out);
            }

            log::info!(
                "request_id={} tenant={} method={} status={} duration={:.3}s bytes_in={} bytes_out={}",
                ctx.request_id,
                tenant_id,
                method,
                status,
                duration,
                ctx.bytes_in,
                ctx.bytes_out
            );
        }
    }
}

pub mod builder;
