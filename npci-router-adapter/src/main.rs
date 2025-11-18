// NPCI Router - Main entry point

use clap::Parser;
use log::{error, info};
use npci_router_adapter::prelude::*;
use pingora_core::server::configuration::Opt as PingoraOpt;
use pingora_core::server::Server;
use pingora_proxy::http_proxy_service;
use std::sync::Arc;

/// Command line arguments
#[derive(Parser, Debug)]
#[command(name = "npci-router")]
#[command(about = "Production-grade NPCI Router built on Pingora", long_about = None)]
#[command(version)]
struct Args {
    /// Path to configuration file (TOML or JSON)
    #[arg(short, long, default_value = "config.toml")]
    config: String,

    /// Enable daemon mode
    #[arg(short, long)]
    daemon: bool,

    /// Enable upgrade mode (graceful restart)
    #[arg(short, long)]
    upgrade: bool,

    /// PID file path (for daemon mode)
    #[arg(long, default_value = "/var/run/npci-router.pid")]
    pid_file: String,

    /// Test configuration and exit
    #[arg(short, long)]
    test: bool,

    /// Log level (error, warn, info, debug, trace)
    #[arg(short, long, default_value = "info")]
    log_level: String,
}

fn main() {
    // Parse command line arguments
    let args = Args::parse();

    // Initialize logging
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(&args.log_level))
        .format_timestamp_millis()
        .init();

    info!(
        "Starting {} v{}",
        npci_router_adapter::name(),
        npci_router_adapter::version()
    );

    // Load configuration
    info!("Loading configuration from: {}", args.config);
    let config = match load_config(&args.config) {
        Ok(cfg) => cfg,
        Err(e) => {
            error!("Failed to load configuration: {}", e);
            std::process::exit(1);
        }
    };

    // Test mode: validate configuration and exit
    if args.test {
        info!("Configuration test successful");
        std::process::exit(0);
    }

    // Create NPCI router
    info!("Initializing NPCI router...");
    let router = match NpciRouter::new(config.clone()) {
        Ok(r) => Arc::new(r),
        Err(e) => {
            error!("Failed to create router: {}", e);
            std::process::exit(1);
        }
    };

    info!("Router initialized successfully");

    // Create Pingora server
    let mut pingora_opt = PingoraOpt::default();
    pingora_opt.daemon = args.daemon;
    pingora_opt.upgrade = args.upgrade;

    let mut server = match Server::new(Some(pingora_opt)) {
        Ok(s) => s,
        Err(e) => {
            error!("Failed to create Pingora server: {}", e);
            std::process::exit(1);
        }
    };

    server.bootstrap();

    // Create HTTP proxy service
    let mut proxy_service = http_proxy_service(&server.configuration, router.clone());

    // Add ingress listener
    let ingress_addr = format!("{}:{}", config.server.ingress_ip, config.server.ingress_port);
    info!("Adding ingress listener on: {}", ingress_addr);
    proxy_service.add_tcp(&ingress_addr);

    // Add egress listener
    let egress_addr = format!("{}:{}", config.server.egress_ip, config.server.egress_port);
    info!("Adding egress listener on: {}", egress_addr);
    proxy_service.add_tcp(&egress_addr);

    // Add services
    server.add_service(proxy_service);

    // Start metrics server if enabled
    if config.monitoring.enabled {
        let metrics_addr = format!(
            "{}:{}",
            config.monitoring.metrics_addr, config.monitoring.metrics_port
        );
        info!("Starting metrics server on: {}", metrics_addr);

        tokio::spawn(async move {
            if let Err(e) = start_metrics_server(&metrics_addr).await {
                error!("Metrics server error: {}", e);
            }
        });
    }

    // Run server
    info!("NPCI Router started successfully");
    info!("Ingress: {}", ingress_addr);
    info!("Egress: {}", egress_addr);

    if let Err(e) = server.run_forever() {
        error!("Server error: {}", e);
        std::process::exit(1);
    }
}

/// Load configuration from file
fn load_config(path: &str) -> RouterResult<RouterConfig> {
    if path.ends_with(".toml") {
        RouterConfig::from_toml_file(path)
    } else if path.ends_with(".json") {
        RouterConfig::from_json_file(path)
    } else {
        // Try to detect format
        let content = std::fs::read_to_string(path)
            .map_err(|e| RouterError::Config(format!("Failed to read config file: {}", e)))?;

        if content.trim().starts_with('{') {
            RouterConfig::from_json_file(path)
        } else {
            RouterConfig::from_toml_file(path)
        }
    }
}

/// Start metrics HTTP server
async fn start_metrics_server(addr: &str) -> Result<(), Box<dyn std::error::Error>> {
    use hyper::{
        service::{make_service_fn, service_fn},
        Body, Request, Response, Server,
    };

    let make_svc = make_service_fn(|_conn| async {
        Ok::<_, hyper::Error>(service_fn(metrics_handler))
    });

    let addr: std::net::SocketAddr = addr.parse()?;
    let server = Server::bind(&addr).serve(make_svc);

    info!("Metrics server listening on http://{}", addr);
    server.await?;

    Ok(())
}

/// Handle metrics HTTP requests
async fn metrics_handler(
    req: Request<Body>,
) -> Result<Response<Body>, hyper::Error> {
    match req.uri().path() {
        "/metrics" => {
            let metrics = npci_router_adapter::metrics::MetricsCollector::gather();
            Ok(Response::builder()
                .status(200)
                .header("Content-Type", "text/plain; version=0.0.4")
                .body(Body::from(metrics))
                .unwrap())
        }
        "/health" => {
            Ok(Response::builder()
                .status(200)
                .header("Content-Type", "application/json")
                .body(Body::from(r#"{"status":"healthy"}"#))
                .unwrap())
        }
        "/ready" => {
            Ok(Response::builder()
                .status(200)
                .header("Content-Type", "application/json")
                .body(Body::from(r#"{"status":"ready"}"#))
                .unwrap())
        }
        _ => {
            Ok(Response::builder()
                .status(404)
                .body(Body::from("Not Found"))
                .unwrap())
        }
    }
}
