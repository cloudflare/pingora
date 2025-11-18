// TLS certificate management module (IP-based)

use crate::config::{TenantConfig, TlsConfig};
use crate::error::{RouterError, RouterResult};
use dashmap::DashMap;
use std::net::IpAddr;
use std::path::Path;
use std::sync::Arc;

/// Certificate information
#[derive(Debug, Clone)]
pub struct CertificateInfo {
    /// Certificate file path
    pub cert_path: String,

    /// Private key file path
    pub key_path: String,

    /// Associated IP address
    pub ip_address: IpAddr,

    /// SNI hostnames (if SNI is enabled)
    pub sni_hostnames: Vec<String>,

    /// Tenant ID
    pub tenant_id: String,

    /// SNI enabled for this certificate
    pub sni_enabled: bool,
}

/// TLS certificate manager for IP-based and SNI-based certificate selection
pub struct TlsCertificateManager {
    /// Map IP addresses to certificates
    ip_to_cert: Arc<DashMap<IpAddr, CertificateInfo>>,

    /// Map SNI hostnames to certificates
    sni_to_cert: Arc<DashMap<String, CertificateInfo>>,

    /// TLS configuration
    config: TlsConfig,
}

impl TlsCertificateManager {
    /// Create a new TLS certificate manager
    pub fn new(config: TlsConfig) -> Self {
        Self {
            ip_to_cert: Arc::new(DashMap::new()),
            sni_to_cert: Arc::new(DashMap::new()),
            config,
        }
    }

    /// Register a certificate for an IP address with optional SNI hostnames
    pub fn register_certificate(
        &self,
        ip: IpAddr,
        cert_path: String,
        key_path: String,
        tenant_id: String,
    ) -> RouterResult<()> {
        self.register_certificate_with_sni(ip, cert_path, key_path, tenant_id, vec![], false)
    }

    /// Register a certificate with SNI support
    pub fn register_certificate_with_sni(
        &self,
        ip: IpAddr,
        cert_path: String,
        key_path: String,
        tenant_id: String,
        sni_hostnames: Vec<String>,
        sni_enabled: bool,
    ) -> RouterResult<()> {
        // Validate certificate files exist
        if !Path::new(&cert_path).exists() {
            return Err(RouterError::Tls(format!(
                "Certificate file not found: {}",
                cert_path
            )));
        }

        if !Path::new(&key_path).exists() {
            return Err(RouterError::Tls(format!(
                "Key file not found: {}",
                key_path
            )));
        }

        let cert_info = CertificateInfo {
            cert_path,
            key_path,
            ip_address: ip,
            sni_hostnames: sni_hostnames.clone(),
            tenant_id,
            sni_enabled,
        };

        // Always register by IP
        self.ip_to_cert.insert(ip, cert_info.clone());
        log::info!("Registered certificate for IP: {}", ip);

        // Also register by SNI if enabled
        if sni_enabled {
            for hostname in sni_hostnames {
                self.sni_to_cert.insert(hostname.clone(), cert_info.clone());
                log::info!("Registered certificate for SNI hostname: {}", hostname);
            }
        }

        Ok(())
    }

    /// Get certificate for an IP address
    pub fn get_certificate(&self, ip: &IpAddr) -> Option<CertificateInfo> {
        self.ip_to_cert.get(ip).map(|cert| cert.clone())
    }

    /// Get certificate for an SNI hostname
    pub fn get_certificate_by_sni(&self, hostname: &str) -> Option<CertificateInfo> {
        self.sni_to_cert.get(hostname).map(|cert| cert.clone())
    }

    /// Get certificate by SNI or fallback to IP
    pub fn get_certificate_hybrid(&self, sni_hostname: Option<&str>, ip: &IpAddr) -> Option<CertificateInfo> {
        use crate::config::SniMode;

        match self.config.sni_mode {
            SniMode::Disabled => {
                // IP-only mode
                self.get_certificate(ip)
            }
            SniMode::Enabled | SniMode::Hybrid => {
                // Try SNI first if provided and prefer_sni is true
                if self.config.prefer_sni {
                    if let Some(hostname) = sni_hostname {
                        if let Some(cert) = self.get_certificate_by_sni(hostname) {
                            log::debug!("Selected certificate using SNI: {}", hostname);
                            return Some(cert);
                        }
                    }
                    // Fallback to IP
                    log::debug!("Falling back to IP-based certificate selection");
                    self.get_certificate(ip)
                } else {
                    // Try IP first, then SNI
                    if let Some(cert) = self.get_certificate(ip) {
                        return Some(cert);
                    }
                    if let Some(hostname) = sni_hostname {
                        self.get_certificate_by_sni(hostname)
                    } else {
                        None
                    }
                }
            }
            SniMode::Strict => {
                // SNI is required
                if let Some(hostname) = sni_hostname {
                    self.get_certificate_by_sni(hostname)
                } else {
                    log::warn!("Strict SNI mode: connection rejected without SNI");
                    None
                }
            }
        }
    }

    /// Remove certificate for an IP address
    pub fn remove_certificate(&self, ip: &IpAddr) {
        if let Some(cert) = self.ip_to_cert.get(ip) {
            // Remove SNI entries too
            for hostname in &cert.sni_hostnames {
                self.sni_to_cert.remove(hostname);
            }
        }
        self.ip_to_cert.remove(ip);
        log::info!("Removed certificate for IP: {}", ip);
    }

    /// Get all registered certificates
    pub fn get_all_certificates(&self) -> Vec<CertificateInfo> {
        self.ip_to_cert
            .iter()
            .map(|entry| entry.value().clone())
            .collect()
    }

    /// Load certificates from tenant configurations
    pub fn load_from_tenants(&self, tenants: &[TenantConfig]) -> RouterResult<()> {
        for tenant in tenants {
            self.register_certificate_with_sni(
                tenant.nat_ip,
                tenant.certificate_path.to_string_lossy().to_string(),
                tenant.key_path.to_string_lossy().to_string(),
                tenant.id.clone(),
                tenant.sni.hostnames.clone(),
                tenant.sni.enabled,
            )?;
        }

        Ok(())
    }

    /// Create TLS settings for a specific IP
    pub fn create_tls_settings(
        &self,
        ip: &IpAddr,
    ) -> RouterResult<pingora_core::listeners::tls::TlsSettings> {
        let cert_info = self
            .get_certificate(ip)
            .ok_or_else(|| RouterError::Tls(format!("No certificate found for IP: {}", ip)))?;

        // Create TLS settings based on min version
        let settings = match self.config.min_version.as_str() {
            "1.3" => {
                pingora_core::listeners::tls::TlsSettings::with_callbacks(
                    &cert_info.cert_path,
                    &cert_info.key_path,
                )
                .map_err(|e| RouterError::Tls(format!("Failed to create TLS settings: {}", e)))?
            }
            "1.2" => {
                pingora_core::listeners::tls::TlsSettings::intermediate(
                    &cert_info.cert_path,
                    &cert_info.key_path,
                )
                .map_err(|e| RouterError::Tls(format!("Failed to create TLS settings: {}", e)))?
            }
            _ => {
                pingora_core::listeners::tls::TlsSettings::with_callbacks(
                    &cert_info.cert_path,
                    &cert_info.key_path,
                )
                .map_err(|e| RouterError::Tls(format!("Failed to create TLS settings: {}", e)))?
            }
        };

        Ok(settings)
    }

    /// Validate certificate for an IP
    pub fn validate_certificate(&self, ip: &IpAddr) -> RouterResult<()> {
        let cert_info = self
            .get_certificate(ip)
            .ok_or_else(|| RouterError::Tls(format!("No certificate found for IP: {}", ip)))?;

        // Check if files exist
        if !Path::new(&cert_info.cert_path).exists() {
            return Err(RouterError::Tls(format!(
                "Certificate file not found: {}",
                cert_info.cert_path
            )));
        }

        if !Path::new(&cert_info.key_path).exists() {
            return Err(RouterError::Tls(format!(
                "Key file not found: {}",
                cert_info.key_path
            )));
        }

        // TODO: Add more validation (certificate expiry, IP SAN verification, etc.)

        Ok(())
    }
}

/// TLS session information
#[derive(Debug, Clone)]
pub struct TlsSessionInfo {
    /// Client IP
    pub client_ip: Option<IpAddr>,

    /// Server IP (local IP)
    pub server_ip: Option<IpAddr>,

    /// SNI hostname (if provided)
    pub sni_hostname: Option<String>,

    /// TLS version
    pub tls_version: Option<String>,

    /// Cipher suite
    pub cipher_suite: Option<String>,

    /// Client certificate DN (for mTLS)
    pub client_cert_dn: Option<String>,
}

impl TlsSessionInfo {
    pub fn new() -> Self {
        Self {
            client_ip: None,
            server_ip: None,
            sni_hostname: None,
            tls_version: None,
            cipher_suite: None,
            client_cert_dn: None,
        }
    }
}

impl Default for TlsSessionInfo {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use tempfile::tempdir;

    #[test]
    fn test_certificate_registration() {
        use crate::config::SniMode;

        let config = TlsConfig {
            min_version: "1.2".to_string(),
            cipher_suites: vec![],
            enable_alpn: true,
            alpn_protocols: vec!["h2".to_string()],
            mtls: false,
            ca_cert_path: None,
            sni_mode: SniMode::Disabled,
            prefer_sni: false,
            strict_sni: false,
        };

        let manager = TlsCertificateManager::new(config);

        let dir = tempdir().unwrap();
        let cert_path = dir.path().join("cert.pem");
        let key_path = dir.path().join("key.pem");

        File::create(&cert_path).unwrap();
        File::create(&key_path).unwrap();

        let ip: IpAddr = "192.168.1.1".parse().unwrap();
        let result = manager.register_certificate(
            ip,
            cert_path.to_string_lossy().to_string(),
            key_path.to_string_lossy().to_string(),
            "tenant1".to_string(),
        );

        assert!(result.is_ok());

        let cert_info = manager.get_certificate(&ip).unwrap();
        assert_eq!(cert_info.tenant_id, "tenant1");
        assert_eq!(cert_info.ip_address, ip);
    }

    #[test]
    fn test_sni_registration() {
        use crate::config::SniMode;

        let config = TlsConfig {
            min_version: "1.2".to_string(),
            cipher_suites: vec![],
            enable_alpn: true,
            alpn_protocols: vec!["h2".to_string()],
            mtls: false,
            ca_cert_path: None,
            sni_mode: SniMode::Enabled,
            prefer_sni: true,
            strict_sni: false,
        };

        let manager = TlsCertificateManager::new(config);

        let dir = tempdir().unwrap();
        let cert_path = dir.path().join("cert.pem");
        let key_path = dir.path().join("key.pem");

        File::create(&cert_path).unwrap();
        File::create(&key_path).unwrap();

        let ip: IpAddr = "192.168.1.1".parse().unwrap();
        let hostnames = vec!["example.com".to_string(), "www.example.com".to_string()];

        let result = manager.register_certificate_with_sni(
            ip,
            cert_path.to_string_lossy().to_string(),
            key_path.to_string_lossy().to_string(),
            "tenant1".to_string(),
            hostnames.clone(),
            true,
        );

        assert!(result.is_ok());

        // Test SNI lookup
        let cert_info = manager.get_certificate_by_sni("example.com").unwrap();
        assert_eq!(cert_info.tenant_id, "tenant1");
        assert!(cert_info.sni_enabled);

        // Test IP lookup still works
        let cert_info = manager.get_certificate(&ip).unwrap();
        assert_eq!(cert_info.tenant_id, "tenant1");

        // Test hybrid lookup
        let cert_info = manager.get_certificate_hybrid(Some("example.com"), &ip).unwrap();
        assert_eq!(cert_info.tenant_id, "tenant1");
    }

    #[test]
    fn test_certificate_removal() {
        use crate::config::SniMode;

        let config = TlsConfig {
            min_version: "1.2".to_string(),
            cipher_suites: vec![],
            enable_alpn: true,
            alpn_protocols: vec!["h2".to_string()],
            mtls: false,
            ca_cert_path: None,
            sni_mode: SniMode::Disabled,
            prefer_sni: false,
            strict_sni: false,
        };

        let manager = TlsCertificateManager::new(config);

        let dir = tempdir().unwrap();
        let cert_path = dir.path().join("cert.pem");
        let key_path = dir.path().join("key.pem");

        File::create(&cert_path).unwrap();
        File::create(&key_path).unwrap();

        let ip: IpAddr = "192.168.1.1".parse().unwrap();
        manager
            .register_certificate(
                ip,
                cert_path.to_string_lossy().to_string(),
                key_path.to_string_lossy().to_string(),
                "tenant1".to_string(),
            )
            .unwrap();

        assert!(manager.get_certificate(&ip).is_some());

        manager.remove_certificate(&ip);
        assert!(manager.get_certificate(&ip).is_none());
    }
}
