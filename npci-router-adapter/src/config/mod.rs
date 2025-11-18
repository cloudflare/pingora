// Configuration module for NPCI Router
// Supports dynamic loading from TOML/JSON files

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

pub mod loader;
pub mod validator;

/// Main configuration structure
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouterConfig {
    /// Server configuration
    pub server: ServerConfig,

    /// Tenant configurations
    pub tenants: HashMap<String, TenantConfig>,

    /// Product configurations
    pub products: HashMap<String, ProductConfig>,

    /// Global rate limiting settings
    pub rate_limiting: RateLimitConfig,

    /// Bandwidth tracking configuration
    pub bandwidth: BandwidthConfig,

    /// Health check configuration
    pub health_check: HealthCheckConfig,

    /// Monitoring and metrics
    pub monitoring: MonitoringConfig,

    /// TLS configuration
    pub tls: TlsConfig,
}

/// Server configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    /// Server name
    pub name: String,

    /// Number of worker threads
    #[serde(default = "default_threads")]
    pub threads: usize,

    /// Ingress IP address (exposed to NPCI)
    pub ingress_ip: IpAddr,

    /// Ingress port
    #[serde(default = "default_https_port")]
    pub ingress_port: u16,

    /// Egress IP address (exposed to NPCI)
    pub egress_ip: IpAddr,

    /// Egress port
    #[serde(default = "default_https_port")]
    pub egress_port: u16,

    /// Enable HTTP/2
    #[serde(default = "default_true")]
    pub enable_h2: bool,

    /// Connection timeout in seconds
    #[serde(default = "default_timeout")]
    pub connection_timeout_secs: u64,

    /// Request timeout in seconds
    #[serde(default = "default_timeout")]
    pub request_timeout_secs: u64,

    /// Keepalive timeout in seconds
    #[serde(default = "default_keepalive")]
    pub keepalive_timeout_secs: u64,

    /// Maximum concurrent connections
    #[serde(default = "default_max_connections")]
    pub max_connections: usize,
}

/// Tenant configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TenantConfig {
    /// Tenant ID (unique identifier)
    pub id: String,

    /// Tenant name
    pub name: String,

    /// NAT IP assigned to this tenant
    pub nat_ip: IpAddr,

    /// Client certificate path (for IP-based TLS)
    pub certificate_path: PathBuf,

    /// Private key path
    pub key_path: PathBuf,

    /// SNI (Server Name Indication) configuration
    #[serde(default)]
    pub sni: SniConfig,

    /// Backend services for this tenant
    pub backends: Vec<BackendConfig>,

    /// Routing strategy
    #[serde(default)]
    pub routing: RoutingConfig,

    /// Tenant-specific rate limits
    pub rate_limit: Option<TenantRateLimit>,

    /// Tenant-specific bandwidth limits
    pub bandwidth_limit: Option<BandwidthLimit>,

    /// Header manipulation rules
    #[serde(default)]
    pub headers: HeaderRules,

    /// Enable/disable this tenant
    #[serde(default = "default_true")]
    pub enabled: bool,
}

/// SNI (Server Name Indication) configuration for a tenant
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SniConfig {
    /// Enable SNI for this tenant
    #[serde(default)]
    pub enabled: bool,

    /// SNI hostname(s) for this tenant
    /// Multiple hostnames can be specified for the same certificate
    #[serde(default)]
    pub hostnames: Vec<String>,

    /// Fallback to IP-based routing if SNI doesn't match
    #[serde(default = "default_true")]
    pub fallback_to_ip: bool,
}

/// Backend service configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackendConfig {
    /// Backend ID
    pub id: String,

    /// Backend address (IP:port or hostname:port)
    pub address: String,

    /// Backend weight for load balancing
    #[serde(default = "default_weight")]
    pub weight: u32,

    /// Use TLS for upstream connection
    #[serde(default)]
    pub tls: bool,

    /// SNI hostname for TLS
    pub sni: Option<String>,

    /// Connection pool size
    #[serde(default = "default_pool_size")]
    pub pool_size: usize,

    /// Health check override
    pub health_check: Option<HealthCheckConfig>,
}

/// Product configuration (multiple IPs per product)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProductConfig {
    /// Product ID
    pub id: String,

    /// Product name
    pub name: String,

    /// List of IP addresses for this product
    pub ips: Vec<IpAddr>,

    /// DNS endpoints (optional)
    pub dns: Vec<String>,

    /// Backend services
    pub backends: Vec<BackendConfig>,

    /// Product-specific rate limits
    pub rate_limit: Option<TenantRateLimit>,
}

/// Routing configuration
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RoutingConfig {
    /// Routing strategy: header, path, ip, or combination
    #[serde(default = "default_routing_strategy")]
    pub strategy: RoutingStrategy,

    /// Header name to use for routing (when strategy includes header)
    pub header_name: Option<String>,

    /// Path prefix for routing (when strategy includes path)
    pub path_prefix: Option<String>,

    /// Fallback backend ID
    pub fallback_backend: Option<String>,
}

/// Routing strategy
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum RoutingStrategy {
    /// Route based on source IP
    #[default]
    Ip,

    /// Route based on HTTP header
    Header,

    /// Route based on path
    Path,

    /// Combination of strategies
    Combined,
}

/// Rate limiting configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RateLimitConfig {
    /// Enable global rate limiting
    #[serde(default = "default_true")]
    pub enabled: bool,

    /// Default requests per second
    #[serde(default = "default_rps")]
    pub default_rps: isize,

    /// Window duration in seconds
    #[serde(default = "default_window")]
    pub window_secs: u64,

    /// Rate limiting algorithm
    #[serde(default)]
    pub algorithm: RateLimitAlgorithm,
}

/// Rate limiting algorithm
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum RateLimitAlgorithm {
    /// Token bucket algorithm
    #[default]
    TokenBucket,

    /// Sliding window
    SlidingWindow,

    /// Fixed window
    FixedWindow,
}

/// Tenant-specific rate limit
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TenantRateLimit {
    /// Requests per second
    pub rps: isize,

    /// Burst size
    #[serde(default)]
    pub burst: Option<isize>,
}

/// Bandwidth configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BandwidthConfig {
    /// Enable bandwidth tracking
    #[serde(default = "default_true")]
    pub enabled: bool,

    /// Report interval in seconds
    #[serde(default = "default_report_interval")]
    pub report_interval_secs: u64,

    /// Store bandwidth stats in file
    pub stats_file: Option<PathBuf>,
}

/// Bandwidth limit per tenant
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BandwidthLimit {
    /// Maximum bytes per second (ingress)
    pub max_bps_ingress: Option<u64>,

    /// Maximum bytes per second (egress)
    pub max_bps_egress: Option<u64>,
}

/// Health check configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthCheckConfig {
    /// Enable health checks
    #[serde(default = "default_true")]
    pub enabled: bool,

    /// Health check interval in seconds
    #[serde(default = "default_health_interval")]
    pub interval_secs: u64,

    /// Health check timeout in seconds
    #[serde(default = "default_health_timeout")]
    pub timeout_secs: u64,

    /// Health check type
    #[serde(default)]
    pub check_type: HealthCheckType,

    /// HTTP health check path
    #[serde(default = "default_health_path")]
    pub http_path: String,

    /// Number of consecutive failures before marking unhealthy
    #[serde(default = "default_failure_threshold")]
    pub failure_threshold: u32,

    /// Number of consecutive successes before marking healthy
    #[serde(default = "default_success_threshold")]
    pub success_threshold: u32,
}

/// Health check type
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum HealthCheckType {
    /// TCP connection check
    #[default]
    Tcp,

    /// HTTP GET request
    Http,

    /// HTTPS GET request
    Https,
}

/// Monitoring configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MonitoringConfig {
    /// Enable Prometheus metrics
    #[serde(default = "default_true")]
    pub enabled: bool,

    /// Metrics endpoint address
    #[serde(default = "default_metrics_addr")]
    pub metrics_addr: String,

    /// Metrics endpoint port
    #[serde(default = "default_metrics_port")]
    pub metrics_port: u16,

    /// Enable access logs
    #[serde(default = "default_true")]
    pub access_logs: bool,

    /// Access log file path
    pub access_log_file: Option<PathBuf>,

    /// Enable tracing
    #[serde(default)]
    pub tracing: bool,
}

/// TLS configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TlsConfig {
    /// Minimum TLS version
    #[serde(default = "default_tls_version")]
    pub min_version: String,

    /// Cipher suites (empty = use defaults)
    #[serde(default)]
    pub cipher_suites: Vec<String>,

    /// Enable ALPN
    #[serde(default = "default_true")]
    pub enable_alpn: bool,

    /// ALPN protocols
    #[serde(default = "default_alpn_protocols")]
    pub alpn_protocols: Vec<String>,

    /// Enable mutual TLS
    #[serde(default)]
    pub mtls: bool,

    /// CA certificate path for mTLS
    pub ca_cert_path: Option<PathBuf>,

    /// Global SNI (Server Name Indication) settings
    #[serde(default)]
    pub sni_mode: SniMode,

    /// Prefer SNI over IP-based routing (when both are available)
    #[serde(default)]
    pub prefer_sni: bool,

    /// Strict SNI mode: reject connections without SNI when SNI is enabled
    #[serde(default)]
    pub strict_sni: bool,
}

/// SNI mode for certificate selection
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum SniMode {
    /// Disabled: Use only IP-based certificate selection
    #[default]
    Disabled,

    /// Enabled: Use SNI when available, fallback to IP
    Enabled,

    /// Strict: Require SNI for all connections
    Strict,

    /// Hybrid: Support both IP and SNI simultaneously
    Hybrid,
}

/// Header manipulation rules
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct HeaderRules {
    /// Headers to add to requests
    #[serde(default)]
    pub add: HashMap<String, String>,

    /// Headers to remove from requests
    #[serde(default)]
    pub remove: Vec<String>,

    /// Headers to set (overwrite if exists)
    #[serde(default)]
    pub set: HashMap<String, String>,

    /// Response headers to add
    #[serde(default)]
    pub response_add: HashMap<String, String>,

    /// Response headers to remove
    #[serde(default)]
    pub response_remove: Vec<String>,
}

// Default value functions
fn default_threads() -> usize {
    num_cpus::get()
}

fn default_https_port() -> u16 {
    443
}

fn default_true() -> bool {
    true
}

fn default_timeout() -> u64 {
    30
}

fn default_keepalive() -> u64 {
    60
}

fn default_max_connections() -> usize {
    10000
}

fn default_weight() -> u32 {
    100
}

fn default_pool_size() -> usize {
    32
}

fn default_routing_strategy() -> RoutingStrategy {
    RoutingStrategy::Ip
}

fn default_rps() -> isize {
    1000
}

fn default_window() -> u64 {
    1
}

fn default_report_interval() -> u64 {
    60
}

fn default_health_interval() -> u64 {
    5
}

fn default_health_timeout() -> u64 {
    2
}

fn default_health_path() -> String {
    "/health".to_string()
}

fn default_failure_threshold() -> u32 {
    3
}

fn default_success_threshold() -> u32 {
    2
}

fn default_metrics_addr() -> String {
    "0.0.0.0".to_string()
}

fn default_metrics_port() -> u16 {
    9090
}

fn default_tls_version() -> String {
    "1.2".to_string()
}

fn default_alpn_protocols() -> Vec<String> {
    vec!["h2".to_string(), "http/1.1".to_string()]
}

impl RouterConfig {
    /// Load configuration from TOML file
    pub fn from_toml_file(path: &str) -> anyhow::Result<Self> {
        loader::load_toml(path)
    }

    /// Load configuration from JSON file
    pub fn from_json_file(path: &str) -> anyhow::Result<Self> {
        loader::load_json(path)
    }

    /// Validate configuration
    pub fn validate(&self) -> anyhow::Result<()> {
        validator::validate(self)
    }

    /// Get tenant by ID
    pub fn get_tenant(&self, id: &str) -> Option<&TenantConfig> {
        self.tenants.get(id)
    }

    /// Get tenant by NAT IP
    pub fn get_tenant_by_ip(&self, ip: &IpAddr) -> Option<&TenantConfig> {
        self.tenants.values().find(|t| &t.nat_ip == ip)
    }

    /// Get product by ID
    pub fn get_product(&self, id: &str) -> Option<&ProductConfig> {
        self.products.get(id)
    }
}

impl Default for RouterConfig {
    fn default() -> Self {
        Self {
            server: ServerConfig {
                name: "npci-router".to_string(),
                threads: default_threads(),
                ingress_ip: "0.0.0.0".parse().unwrap(),
                ingress_port: default_https_port(),
                egress_ip: "0.0.0.0".parse().unwrap(),
                egress_port: default_https_port(),
                enable_h2: true,
                connection_timeout_secs: default_timeout(),
                request_timeout_secs: default_timeout(),
                keepalive_timeout_secs: default_keepalive(),
                max_connections: default_max_connections(),
            },
            tenants: HashMap::new(),
            products: HashMap::new(),
            rate_limiting: RateLimitConfig {
                enabled: true,
                default_rps: default_rps(),
                window_secs: default_window(),
                algorithm: RateLimitAlgorithm::default(),
            },
            bandwidth: BandwidthConfig {
                enabled: true,
                report_interval_secs: default_report_interval(),
                stats_file: None,
            },
            health_check: HealthCheckConfig {
                enabled: true,
                interval_secs: default_health_interval(),
                timeout_secs: default_health_timeout(),
                check_type: HealthCheckType::default(),
                http_path: default_health_path(),
                failure_threshold: default_failure_threshold(),
                success_threshold: default_success_threshold(),
            },
            monitoring: MonitoringConfig {
                enabled: true,
                metrics_addr: default_metrics_addr(),
                metrics_port: default_metrics_port(),
                access_logs: true,
                access_log_file: None,
                tracing: false,
            },
            tls: TlsConfig {
                min_version: default_tls_version(),
                cipher_suites: vec![],
                enable_alpn: true,
                alpn_protocols: default_alpn_protocols(),
                mtls: false,
                ca_cert_path: None,
            },
        }
    }
}
