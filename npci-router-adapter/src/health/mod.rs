// Health check module

use crate::config::{BackendConfig, HealthCheckConfig, HealthCheckType};
use crate::error::{RouterError, RouterResult};
use dashmap::DashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::time::sleep;

/// Health status of a backend
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum HealthStatus {
    /// Backend is healthy
    Healthy,

    /// Backend is unhealthy
    Unhealthy,

    /// Health status is unknown (not checked yet)
    Unknown,
}

/// Backend health information
#[derive(Debug, Clone)]
pub struct BackendHealth {
    /// Backend ID
    pub backend_id: String,

    /// Current health status
    pub status: HealthStatus,

    /// Last check time
    pub last_check: Option<Instant>,

    /// Consecutive successes
    pub consecutive_successes: u32,

    /// Consecutive failures
    pub consecutive_failures: u32,

    /// Total checks performed
    pub total_checks: u64,

    /// Total failures
    pub total_failures: u64,

    /// Last error message
    pub last_error: Option<String>,
}

impl BackendHealth {
    fn new(backend_id: String) -> Self {
        Self {
            backend_id,
            status: HealthStatus::Unknown,
            last_check: None,
            consecutive_successes: 0,
            consecutive_failures: 0,
            total_checks: 0,
            total_failures: 0,
            last_error: None,
        }
    }

    fn record_success(&mut self, success_threshold: u32) {
        self.consecutive_successes += 1;
        self.consecutive_failures = 0;
        self.total_checks += 1;
        self.last_check = Some(Instant::now());
        self.last_error = None;

        if self.consecutive_successes >= success_threshold {
            self.status = HealthStatus::Healthy;
        }
    }

    fn record_failure(&mut self, failure_threshold: u32, error: String) {
        self.consecutive_failures += 1;
        self.consecutive_successes = 0;
        self.total_checks += 1;
        self.total_failures += 1;
        self.last_check = Some(Instant::now());
        self.last_error = Some(error);

        if self.consecutive_failures >= failure_threshold {
            self.status = HealthStatus::Unhealthy;
        }
    }
}

/// Health checker for backends
pub struct HealthChecker {
    /// Health status for all backends
    health_status: Arc<DashMap<String, BackendHealth>>,

    /// Health check configuration
    config: HealthCheckConfig,
}

impl HealthChecker {
    /// Create a new health checker
    pub fn new(config: HealthCheckConfig) -> Self {
        Self {
            health_status: Arc::new(DashMap::new()),
            config,
        }
    }

    /// Check if a backend is healthy
    pub fn is_healthy(&self, backend_id: &str) -> bool {
        self.health_status
            .get(backend_id)
            .map(|health| health.status == HealthStatus::Healthy)
            .unwrap_or(true) // Assume healthy if not checked yet
    }

    /// Get health status for a backend
    pub fn get_health(&self, backend_id: &str) -> Option<BackendHealth> {
        self.health_status.get(backend_id).map(|h| h.clone())
    }

    /// Get health status for all backends
    pub fn get_all_health(&self) -> Vec<BackendHealth> {
        self.health_status
            .iter()
            .map(|entry| entry.value().clone())
            .collect()
    }

    /// Perform health check on a backend
    async fn check_backend(&self, backend: &BackendConfig) -> Result<(), String> {
        let timeout = Duration::from_secs(self.config.timeout_secs);

        match self.config.check_type {
            HealthCheckType::Tcp => self.check_tcp(&backend.address, timeout).await,
            HealthCheckType::Http => {
                self.check_http(&backend.address, &self.config.http_path, false, timeout)
                    .await
            }
            HealthCheckType::Https => {
                self.check_http(&backend.address, &self.config.http_path, true, timeout)
                    .await
            }
        }
    }

    /// Perform TCP health check
    async fn check_tcp(&self, address: &str, timeout: Duration) -> Result<(), String> {
        match tokio::time::timeout(timeout, tokio::net::TcpStream::connect(address)).await {
            Ok(Ok(_)) => Ok(()),
            Ok(Err(e)) => Err(format!("TCP connection failed: {}", e)),
            Err(_) => Err("TCP connection timeout".to_string()),
        }
    }

    /// Perform HTTP/HTTPS health check
    async fn check_http(
        &self,
        address: &str,
        path: &str,
        use_https: bool,
        timeout: Duration,
    ) -> Result<(), String> {
        let scheme = if use_https { "https" } else { "http" };
        let url = format!("{}://{}{}", scheme, address, path);

        let client = reqwest::Client::builder()
            .timeout(timeout)
            .danger_accept_invalid_certs(true) // For self-signed certs
            .build()
            .map_err(|e| format!("Failed to build HTTP client: {}", e))?;

        match client.get(&url).send().await {
            Ok(response) => {
                if response.status().is_success() {
                    Ok(())
                } else {
                    Err(format!("HTTP health check failed with status: {}", response.status()))
                }
            }
            Err(e) => Err(format!("HTTP request failed: {}", e)),
        }
    }

    /// Update health status for a backend
    fn update_health(&self, backend_id: &str, result: Result<(), String>) {
        let mut health = self
            .health_status
            .entry(backend_id.to_string())
            .or_insert_with(|| BackendHealth::new(backend_id.to_string()));

        match result {
            Ok(()) => {
                health.record_success(self.config.success_threshold);
                log::debug!("Health check succeeded for backend: {}", backend_id);
            }
            Err(error) => {
                health.record_failure(self.config.failure_threshold, error.clone());
                log::warn!("Health check failed for backend {}: {}", backend_id, error);
            }
        }
    }

    /// Start background health checking for a backend
    pub fn start_health_check(
        self: Arc<Self>,
        backend: BackendConfig,
    ) -> tokio::task::JoinHandle<()> {
        let interval = Duration::from_secs(self.config.interval_secs);
        let backend_id = backend.id.clone();

        tokio::spawn(async move {
            let mut interval_timer = tokio::time::interval(interval);
            loop {
                interval_timer.tick().await;

                let result = self.check_backend(&backend).await;
                self.update_health(&backend_id, result);
            }
        })
    }

    /// Start health checking for multiple backends
    pub fn start_health_checks(
        self: Arc<Self>,
        backends: Vec<BackendConfig>,
    ) -> Vec<tokio::task::JoinHandle<()>> {
        backends
            .into_iter()
            .map(|backend| self.clone().start_health_check(backend))
            .collect()
    }

    /// Get health statistics
    pub fn get_stats(&self) -> HealthStats {
        let all_health = self.get_all_health();
        let total = all_health.len();
        let healthy = all_health
            .iter()
            .filter(|h| h.status == HealthStatus::Healthy)
            .count();
        let unhealthy = all_health
            .iter()
            .filter(|h| h.status == HealthStatus::Unhealthy)
            .count();
        let unknown = all_health
            .iter()
            .filter(|h| h.status == HealthStatus::Unknown)
            .count();

        HealthStats {
            total,
            healthy,
            unhealthy,
            unknown,
        }
    }
}

/// Health statistics
#[derive(Debug, Clone)]
pub struct HealthStats {
    pub total: usize,
    pub healthy: usize,
    pub unhealthy: usize,
    pub unknown: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_backend_health() {
        let mut health = BackendHealth::new("backend1".to_string());

        assert_eq!(health.status, HealthStatus::Unknown);

        health.record_success(2);
        assert_eq!(health.consecutive_successes, 1);
        assert_eq!(health.status, HealthStatus::Unknown);

        health.record_success(2);
        assert_eq!(health.consecutive_successes, 2);
        assert_eq!(health.status, HealthStatus::Healthy);

        health.record_failure(2, "error".to_string());
        assert_eq!(health.consecutive_failures, 1);
        assert_eq!(health.consecutive_successes, 0);
        assert_eq!(health.status, HealthStatus::Healthy);

        health.record_failure(2, "error".to_string());
        assert_eq!(health.status, HealthStatus::Unhealthy);
    }

    #[tokio::test]
    async fn test_tcp_health_check() {
        let config = HealthCheckConfig {
            enabled: true,
            interval_secs: 5,
            timeout_secs: 2,
            check_type: HealthCheckType::Tcp,
            http_path: "/health".to_string(),
            failure_threshold: 3,
            success_threshold: 2,
        };

        let checker = HealthChecker::new(config);

        // This should fail since there's no server listening
        let result = checker.check_tcp("127.0.0.1:9999", Duration::from_secs(1)).await;
        assert!(result.is_err());
    }

    #[test]
    fn test_health_stats() {
        let config = HealthCheckConfig {
            enabled: true,
            interval_secs: 5,
            timeout_secs: 2,
            check_type: HealthCheckType::Tcp,
            http_path: "/health".to_string(),
            failure_threshold: 3,
            success_threshold: 2,
        };

        let checker = HealthChecker::new(config);

        checker.update_health("backend1", Ok(()));
        checker.update_health("backend1", Ok(()));
        checker.update_health("backend2", Err("error".to_string()));
        checker.update_health("backend2", Err("error".to_string()));
        checker.update_health("backend2", Err("error".to_string()));

        let stats = checker.get_stats();
        assert_eq!(stats.total, 2);
        assert_eq!(stats.healthy, 1);
        assert_eq!(stats.unhealthy, 1);
    }
}
