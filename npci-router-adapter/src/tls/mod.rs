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

    /// Tenant ID
    pub tenant_id: String,
}

/// TLS certificate manager for IP-based certificate selection
pub struct TlsCertificateManager {
    /// Map IP addresses to certificates
    ip_to_cert: Arc<DashMap<IpAddr, CertificateInfo>>,

    /// TLS configuration
    config: TlsConfig,
}

impl TlsCertificateManager {
    /// Create a new TLS certificate manager
    pub fn new(config: TlsConfig) -> Self {
        Self {
            ip_to_cert: Arc::new(DashMap::new()),
            config,
        }
    }

    /// Register a certificate for an IP address
    pub fn register_certificate(
        &self,
        ip: IpAddr,
        cert_path: String,
        key_path: String,
        tenant_id: String,
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
            tenant_id,
        };

        self.ip_to_cert.insert(ip, cert_info);
        log::info!("Registered certificate for IP: {}", ip);

        Ok(())
    }

    /// Get certificate for an IP address
    pub fn get_certificate(&self, ip: &IpAddr) -> Option<CertificateInfo> {
        self.ip_to_cert.get(ip).map(|cert| cert.clone())
    }

    /// Remove certificate for an IP address
    pub fn remove_certificate(&self, ip: &IpAddr) {
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
            self.register_certificate(
                tenant.nat_ip,
                tenant.certificate_path.to_string_lossy().to_string(),
                tenant.key_path.to_string_lossy().to_string(),
                tenant.id.clone(),
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
        let config = TlsConfig {
            min_version: "1.2".to_string(),
            cipher_suites: vec![],
            enable_alpn: true,
            alpn_protocols: vec!["h2".to_string()],
            mtls: false,
            ca_cert_path: None,
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
    fn test_certificate_removal() {
        let config = TlsConfig {
            min_version: "1.2".to_string(),
            cipher_suites: vec![],
            enable_alpn: true,
            alpn_protocols: vec!["h2".to_string()],
            mtls: false,
            ca_cert_path: None,
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
