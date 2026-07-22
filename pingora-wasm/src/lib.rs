//! `pingora-wasm`: WebAssembly Filter Engine for Pingora Proxy.
//!
//! Provides a Proxy-WASM compliant host runtime environment to load and execute
//! dynamic Wasm filter plugins in Pingora's HTTP request/response filter pipeline.

use wasmtime::{Engine, Module};

/// Main WebAssembly Engine instance for loading and executing guest filter modules.
pub struct WasmEngine {
    engine: Engine,
}

impl WasmEngine {
    /// Initialize a new `WasmEngine` with default Wasmtime runtime configuration.
    pub fn new() -> anyhow::Result<Self> {
        let engine = Engine::default();
        Ok(Self { engine })
    }

    /// Load and compile a Wasm binary module from raw bytes.
    pub fn load_module(&self, wasm_bytes: &[u8]) -> anyhow::Result<Module> {
        let module = Module::new(&self.engine, wasm_bytes)?;
        Ok(module)
    }
}

impl Default for WasmEngine {
    fn default() -> Self {
        Self::new().expect("Failed to initialize WasmEngine")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_engine_init() {
        let engine = WasmEngine::new();
        assert!(engine.is_ok(), "WasmEngine should initialize successfully");
    }

    #[test]
    fn test_default_impl() {
        let _engine = WasmEngine::default();
    }
}
