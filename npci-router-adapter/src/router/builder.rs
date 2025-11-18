// Router builder for easier construction

use super::NpciRouter;
use crate::config::RouterConfig;
use crate::error::RouterResult;

/// Builder for NPCI Router
pub struct NpciRouterBuilder {
    config: Option<RouterConfig>,
}

impl NpciRouterBuilder {
    /// Create a new builder
    pub fn new() -> Self {
        Self { config: None }
    }

    /// Set configuration from TOML file
    pub fn with_toml_config(mut self, path: &str) -> RouterResult<Self> {
        self.config = Some(RouterConfig::from_toml_file(path)?);
        Ok(self)
    }

    /// Set configuration from JSON file
    pub fn with_json_config(mut self, path: &str) -> RouterResult<Self> {
        self.config = Some(RouterConfig::from_json_file(path)?);
        Ok(self)
    }

    /// Set configuration directly
    pub fn with_config(mut self, config: RouterConfig) -> Self {
        self.config = Some(config);
        self
    }

    /// Build the router
    pub fn build(self) -> RouterResult<NpciRouter> {
        let config = self
            .config
            .ok_or_else(|| crate::error::RouterError::Config("No configuration provided".to_string()))?;

        NpciRouter::new(config)
    }
}

impl Default for NpciRouterBuilder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_builder() {
        let config = RouterConfig::default();
        let builder = NpciRouterBuilder::new().with_config(config);
        // Can't fully build without valid config, but test builder pattern
        assert!(builder.config.is_some());
    }
}
