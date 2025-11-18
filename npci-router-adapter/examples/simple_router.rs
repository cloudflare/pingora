// Simple example demonstrating NPCI router usage

use npci_router_adapter::prelude::*;
use npci_router_adapter::config::*;
use std::collections::HashMap;
use std::net::IpAddr;
use std::path::PathBuf;

fn main() {
    // Initialize logging
    env_logger::init();

    println!("=== NPCI Router Simple Example ===\n");

    // Create a basic configuration programmatically
    let config = create_example_config();

    // Validate configuration
    match config.validate() {
        Ok(_) => println!("✓ Configuration validated successfully"),
        Err(e) => {
            eprintln!("✗ Configuration validation failed: {}", e);
            return;
        }
    }

    // Display configuration summary
    print_config_summary(&config);

    // Create router
    println!("\nCreating NPCI router...");
    match NpciRouter::new(config) {
        Ok(router) => {
            println!("✓ Router created successfully");
            println!("\nRouter is ready to handle requests!");
            println!("Note: This example creates the router but doesn't start the server.");
            println!("To run a full server, use: cargo run --bin npci-router");
        }
        Err(e) => {
            eprintln!("✗ Failed to create router: {}", e);
        }
    }
}

fn create_example_config() -> RouterConfig {
    let mut config = RouterConfig::default();

    // Server configuration
    config.server = ServerConfig {
        name: "example-router".to_string(),
        threads: 4,
        ingress_ip: "127.0.0.1".parse().unwrap(),
        ingress_port: 8443,
        egress_ip: "127.0.0.1".parse().unwrap(),
        egress_port: 8444,
        enable_h2: true,
        connection_timeout_secs: 30,
        request_timeout_secs: 30,
        keepalive_timeout_secs: 60,
        max_connections: 10000,
    };

    // Rate limiting
    config.rate_limiting = RateLimitConfig {
        enabled: true,
        default_rps: 100,
        window_secs: 1,
        algorithm: RateLimitAlgorithm::TokenBucket,
    };

    // Bandwidth tracking
    config.bandwidth = BandwidthConfig {
        enabled: true,
        report_interval_secs: 60,
        stats_file: None,
    };

    // Note: In a real setup, you would use actual certificate files
    // For this example, we're showing the structure
    println!("Note: This example uses placeholder certificate paths.");
    println!("In production, replace these with actual certificate files.\n");

    config
}

fn print_config_summary(config: &RouterConfig) {
    println!("\n=== Configuration Summary ===");
    println!("Server: {}", config.server.name);
    println!("Threads: {}", config.server.threads);
    println!("Ingress: {}:{}", config.server.ingress_ip, config.server.ingress_port);
    println!("Egress: {}:{}", config.server.egress_ip, config.server.egress_port);
    println!("HTTP/2 Enabled: {}", config.server.enable_h2);
    println!("\nRate Limiting:");
    println!("  Enabled: {}", config.rate_limiting.enabled);
    println!("  Default RPS: {}", config.rate_limiting.default_rps);
    println!("  Algorithm: {:?}", config.rate_limiting.algorithm);
    println!("\nBandwidth Tracking:");
    println!("  Enabled: {}", config.bandwidth.enabled);
    println!("  Report Interval: {}s", config.bandwidth.report_interval_secs);
    println!("\nHealth Checks:");
    println!("  Enabled: {}", config.health_check.enabled);
    println!("  Interval: {}s", config.health_check.interval_secs);
    println!("  Type: {:?}", config.health_check.check_type);
    println!("\nMonitoring:");
    println!("  Metrics Enabled: {}", config.monitoring.enabled);
    println!("  Metrics Port: {}", config.monitoring.metrics_port);
    println!("\nTenants: {}", config.tenants.len());
    println!("Products: {}", config.products.len());
}
