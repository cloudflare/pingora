// Metrics and monitoring module

use once_cell::sync::Lazy;
use prometheus::{
    Counter, CounterVec, Gauge, GaugeVec, Histogram, HistogramOpts, HistogramVec, IntCounter,
    IntCounterVec, IntGauge, IntGaugeVec, Opts, Registry,
};
use std::sync::Arc;

/// Global metrics registry
pub static METRICS_REGISTRY: Lazy<Registry> = Lazy::new(Registry::new);

/// Request metrics
pub static REQUEST_TOTAL: Lazy<IntCounterVec> = Lazy::new(|| {
    let opts = Opts::new("npci_requests_total", "Total number of requests");
    let counter = IntCounterVec::new(opts, &["tenant", "method", "status"]).unwrap();
    METRICS_REGISTRY.register(Box::new(counter.clone())).unwrap();
    counter
});

/// Request duration histogram
pub static REQUEST_DURATION: Lazy<HistogramVec> = Lazy::new(|| {
    let opts = HistogramOpts::new("npci_request_duration_seconds", "Request duration in seconds");
    let histogram = HistogramVec::new(opts, &["tenant", "method"]).unwrap();
    METRICS_REGISTRY
        .register(Box::new(histogram.clone()))
        .unwrap();
    histogram
});

/// Bandwidth metrics (bytes)
pub static BANDWIDTH_IN: Lazy<IntCounterVec> = Lazy::new(|| {
    let opts = Opts::new("npci_bandwidth_in_bytes_total", "Total ingress bandwidth in bytes");
    let counter = IntCounterVec::new(opts, &["tenant"]).unwrap();
    METRICS_REGISTRY.register(Box::new(counter.clone())).unwrap();
    counter
});

pub static BANDWIDTH_OUT: Lazy<IntCounterVec> = Lazy::new(|| {
    let opts = Opts::new("npci_bandwidth_out_bytes_total", "Total egress bandwidth in bytes");
    let counter = IntCounterVec::new(opts, &["tenant"]).unwrap();
    METRICS_REGISTRY.register(Box::new(counter.clone())).unwrap();
    counter
});

/// Rate limit metrics
pub static RATE_LIMIT_EXCEEDED: Lazy<IntCounterVec> = Lazy::new(|| {
    let opts = Opts::new("npci_rate_limit_exceeded_total", "Total rate limit violations");
    let counter = IntCounterVec::new(opts, &["tenant"]).unwrap();
    METRICS_REGISTRY.register(Box::new(counter.clone())).unwrap();
    counter
});

/// Backend health metrics
pub static BACKEND_HEALTH: Lazy<IntGaugeVec> = Lazy::new(|| {
    let opts = Opts::new("npci_backend_health", "Backend health status (1=healthy, 0=unhealthy)");
    let gauge = IntGaugeVec::new(opts, &["backend", "tenant"]).unwrap();
    METRICS_REGISTRY.register(Box::new(gauge.clone())).unwrap();
    gauge
});

/// Backend request metrics
pub static BACKEND_REQUESTS: Lazy<IntCounterVec> = Lazy::new(|| {
    let opts = Opts::new("npci_backend_requests_total", "Total requests to backends");
    let counter = IntCounterVec::new(opts, &["backend", "tenant", "status"]).unwrap();
    METRICS_REGISTRY.register(Box::new(counter.clone())).unwrap();
    counter
});

/// Connection pool metrics
pub static CONNECTION_POOL_ACTIVE: Lazy<IntGaugeVec> = Lazy::new(|| {
    let opts = Opts::new("npci_connection_pool_active", "Active connections in pool");
    let gauge = IntGaugeVec::new(opts, &["backend"]).unwrap();
    METRICS_REGISTRY.register(Box::new(gauge.clone())).unwrap();
    gauge
});

pub static CONNECTION_POOL_IDLE: Lazy<IntGaugeVec> = Lazy::new(|| {
    let opts = Opts::new("npci_connection_pool_idle", "Idle connections in pool");
    let gauge = IntGaugeVec::new(opts, &["backend"]).unwrap();
    METRICS_REGISTRY.register(Box::new(gauge.clone())).unwrap();
    gauge
});

/// TLS handshake metrics
pub static TLS_HANDSHAKE_DURATION: Lazy<HistogramVec> = Lazy::new(|| {
    let opts = HistogramOpts::new("npci_tls_handshake_duration_seconds", "TLS handshake duration");
    let histogram = HistogramVec::new(opts, &["tenant"]).unwrap();
    METRICS_REGISTRY
        .register(Box::new(histogram.clone()))
        .unwrap();
    histogram
});

pub static TLS_HANDSHAKE_ERRORS: Lazy<IntCounterVec> = Lazy::new(|| {
    let opts = Opts::new("npci_tls_handshake_errors_total", "TLS handshake errors");
    let counter = IntCounterVec::new(opts, &["tenant", "error_type"]).unwrap();
    METRICS_REGISTRY.register(Box::new(counter.clone())).unwrap();
    counter
});

/// Error metrics
pub static ERRORS_TOTAL: Lazy<IntCounterVec> = Lazy::new(|| {
    let opts = Opts::new("npci_errors_total", "Total errors by type");
    let counter = IntCounterVec::new(opts, &["tenant", "error_type"]).unwrap();
    METRICS_REGISTRY.register(Box::new(counter.clone())).unwrap();
    counter
});

/// Metrics collector for gathering statistics
pub struct MetricsCollector;

impl MetricsCollector {
    /// Record a request
    pub fn record_request(tenant: &str, method: &str, status: u16, duration_secs: f64) {
        REQUEST_TOTAL
            .with_label_values(&[tenant, method, &status.to_string()])
            .inc();

        REQUEST_DURATION
            .with_label_values(&[tenant, method])
            .observe(duration_secs);
    }

    /// Record bandwidth
    pub fn record_bandwidth(tenant: &str, bytes_in: u64, bytes_out: u64) {
        BANDWIDTH_IN
            .with_label_values(&[tenant])
            .inc_by(bytes_in);

        BANDWIDTH_OUT
            .with_label_values(&[tenant])
            .inc_by(bytes_out);
    }

    /// Record rate limit violation
    pub fn record_rate_limit_exceeded(tenant: &str) {
        RATE_LIMIT_EXCEEDED.with_label_values(&[tenant]).inc();
    }

    /// Update backend health
    pub fn update_backend_health(backend: &str, tenant: &str, is_healthy: bool) {
        let value = if is_healthy { 1 } else { 0 };
        BACKEND_HEALTH
            .with_label_values(&[backend, tenant])
            .set(value);
    }

    /// Record backend request
    pub fn record_backend_request(backend: &str, tenant: &str, status: u16) {
        BACKEND_REQUESTS
            .with_label_values(&[backend, tenant, &status.to_string()])
            .inc();
    }

    /// Update connection pool metrics
    pub fn update_connection_pool(backend: &str, active: i64, idle: i64) {
        CONNECTION_POOL_ACTIVE
            .with_label_values(&[backend])
            .set(active);

        CONNECTION_POOL_IDLE.with_label_values(&[backend]).set(idle);
    }

    /// Record TLS handshake
    pub fn record_tls_handshake(tenant: &str, duration_secs: f64) {
        TLS_HANDSHAKE_DURATION
            .with_label_values(&[tenant])
            .observe(duration_secs);
    }

    /// Record TLS error
    pub fn record_tls_error(tenant: &str, error_type: &str) {
        TLS_HANDSHAKE_ERRORS
            .with_label_values(&[tenant, error_type])
            .inc();
    }

    /// Record error
    pub fn record_error(tenant: &str, error_type: &str) {
        ERRORS_TOTAL
            .with_label_values(&[tenant, error_type])
            .inc();
    }

    /// Get metrics in Prometheus format
    pub fn gather() -> String {
        use prometheus::Encoder;
        let encoder = prometheus::TextEncoder::new();
        let metric_families = METRICS_REGISTRY.gather();
        let mut buffer = Vec::new();
        encoder.encode(&metric_families, &mut buffer).unwrap();
        String::from_utf8(buffer).unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_record_request() {
        MetricsCollector::record_request("tenant1", "GET", 200, 0.5);

        let metrics = MetricsCollector::gather();
        assert!(metrics.contains("npci_requests_total"));
        assert!(metrics.contains("tenant1"));
    }

    #[test]
    fn test_record_bandwidth() {
        MetricsCollector::record_bandwidth("tenant1", 1000, 500);

        let metrics = MetricsCollector::gather();
        assert!(metrics.contains("npci_bandwidth_in_bytes_total"));
        assert!(metrics.contains("npci_bandwidth_out_bytes_total"));
    }

    #[test]
    fn test_backend_health() {
        MetricsCollector::update_backend_health("backend1", "tenant1", true);
        MetricsCollector::update_backend_health("backend2", "tenant1", false);

        let metrics = MetricsCollector::gather();
        assert!(metrics.contains("npci_backend_health"));
    }
}
