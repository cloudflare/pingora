// Configuration loader module

use super::RouterConfig;
use anyhow::{Context, Result};
use std::fs;

/// Load configuration from TOML file
pub fn load_toml(path: &str) -> Result<RouterConfig> {
    let content = fs::read_to_string(path)
        .with_context(|| format!("Failed to read config file: {}", path))?;

    let config: RouterConfig = toml::from_str(&content)
        .with_context(|| format!("Failed to parse TOML config: {}", path))?;

    config.validate()?;
    Ok(config)
}

/// Load configuration from JSON file
pub fn load_json(path: &str) -> Result<RouterConfig> {
    let content = fs::read_to_string(path)
        .with_context(|| format!("Failed to read config file: {}", path))?;

    let config: RouterConfig = serde_json::from_str(&content)
        .with_context(|| format!("Failed to parse JSON config: {}", path))?;

    config.validate()?;
    Ok(config)
}

/// Watch configuration file for changes and hot-reload
#[cfg(feature = "hot-reload")]
pub async fn watch_config(path: String, on_reload: impl Fn(RouterConfig) + Send + 'static) -> Result<()> {
    use notify::{Watcher, RecursiveMode, watcher};
    use std::sync::mpsc::channel;
    use std::time::Duration;

    let (tx, rx) = channel();
    let mut watcher = watcher(tx, Duration::from_secs(2))?;
    watcher.watch(&path, RecursiveMode::NonRecursive)?;

    tokio::task::spawn_blocking(move || {
        loop {
            match rx.recv() {
                Ok(event) => {
                    log::info!("Config file changed: {:?}", event);
                    match load_toml(&path) {
                        Ok(config) => {
                            log::info!("Configuration reloaded successfully");
                            on_reload(config);
                        }
                        Err(e) => {
                            log::error!("Failed to reload configuration: {}", e);
                        }
                    }
                }
                Err(e) => {
                    log::error!("Watch error: {}", e);
                    break;
                }
            }
        }
    });

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn test_load_toml() {
        let toml_content = r#"
            [server]
            name = "test-router"
            ingress_ip = "192.168.1.1"
            egress_ip = "192.168.1.2"

            [rate_limiting]
            enabled = true
            default_rps = 100

            [bandwidth]
            enabled = true

            [health_check]
            enabled = true

            [monitoring]
            enabled = true

            [tls]
            min_version = "1.2"
        "#;

        let mut temp_file = NamedTempFile::new().unwrap();
        temp_file.write_all(toml_content.as_bytes()).unwrap();

        let config = load_toml(temp_file.path().to_str().unwrap()).unwrap();
        assert_eq!(config.server.name, "test-router");
    }

    #[test]
    fn test_load_json() {
        let json_content = r#"
        {
            "server": {
                "name": "test-router",
                "ingress_ip": "192.168.1.1",
                "egress_ip": "192.168.1.2"
            },
            "tenants": {},
            "products": {},
            "rate_limiting": {
                "enabled": true,
                "default_rps": 100
            },
            "bandwidth": {
                "enabled": true
            },
            "health_check": {
                "enabled": true
            },
            "monitoring": {
                "enabled": true
            },
            "tls": {
                "min_version": "1.2"
            }
        }
        "#;

        let mut temp_file = NamedTempFile::new().unwrap();
        temp_file.write_all(json_content.as_bytes()).unwrap();

        let config = load_json(temp_file.path().to_str().unwrap()).unwrap();
        assert_eq!(config.server.name, "test-router");
    }
}
