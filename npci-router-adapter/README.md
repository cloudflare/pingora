# NPCI Router Adapter

A production-grade, multi-tenant HTTP/HTTPS router built on [Pingora](https://github.com/cloudflare/pingora) for NPCI (National Payments Corporation of India) integration.

## Features

✅ **Multi-Tenant Routing**: IP-based, header-based, path-based, and combined routing strategies
✅ **IP-Based TLS**: Different certificates for different clients based on IP (no SNI required)
✅ **Configuration-Driven**: Fully configurable via TOML/JSON files - add tenants/products without code changes
✅ **Bandwidth Tracking**: Real-time bandwidth monitoring per tenant/product
✅ **Rate Limiting**: Token bucket, sliding window, and fixed window algorithms
✅ **Header Manipulation**: Add, remove, set headers for requests and responses
✅ **Health Checks**: TCP/HTTP/HTTPS health checks with automatic failover
✅ **Connection Pooling**: Efficient connection reuse with per-backend pools
✅ **Load Balancing**: Weighted round-robin with health-aware backend selection
✅ **Metrics & Monitoring**: Prometheus metrics, health endpoints, access logs
✅ **High Availability**: Self-healing, auto-scaling, rolling updates
✅ **Kubernetes Ready**: Complete K8s manifests and Helm charts included

## Architecture

```
┌─────────────────┐
│  NPCI (Traffic) │
└────────┬────────┘
         │
    ┌────▼────┐
    │Ingress IP│ (Single whitelisted IP)
    └────┬────┘
         │
┌────────▼────────────────────────────────────┐
│         NPCI Router (Pingora)               │
│  ┌──────────────────────────────────────┐   │
│  │  • IP-based TLS Termination          │   │
│  │  • Tenant Identification             │   │
│  │  • Rate Limiting                     │   │
│  │  • Bandwidth Tracking                │   │
│  │  • Header Manipulation               │   │
│  │  • Load Balancing                    │   │
│  └──────────────────────────────────────┘   │
└────────┬────────────────────────────────────┘
         │
    ┌────▼────┐
    │Egress IP│ (Single whitelisted IP)
    └────┬────┘
         │
   ┌─────▼──────┐
   │ Tenant     │
   │ Backends   │
   └────────────┘
```

## Quick Start

### Prerequisites

- Rust 1.82+
- Kubernetes cluster (for production deployment)
- TLS certificates for each tenant

### Installation

```bash
# Clone the repository
cd npci-router-adapter

# Build the project
cargo build --release

# Run tests
cargo test

# Run with example configuration
./target/release/npci-router -c config/example.toml
```

### Configuration

Create a `config.toml` file:

```toml
[server]
name = "npci-router"
threads = 8
ingress_ip = "0.0.0.0"
ingress_port = 443
egress_ip = "0.0.0.0"
egress_port = 8443

[tenants.bank1]
id = "bank1"
name = "Example Bank"
nat_ip = "192.168.10.10"
certificate_path = "/path/to/cert.pem"
key_path = "/path/to/key.pem"
enabled = true

[[tenants.bank1.backends]]
id = "backend1"
address = "10.0.1.100:8080"
weight = 100

# See config/example.toml for complete configuration
```

### Running

```bash
# Test configuration
npci-router -c config.toml --test

# Run in foreground
npci-router -c config.toml

# Run as daemon
npci-router -c config.toml --daemon

# Enable debug logging
npci-router -c config.toml --log-level debug
```

## Kubernetes Deployment

### Using kubectl

```bash
# Create namespace and apply manifests
kubectl apply -f k8s/deployment.yaml

# Check status
kubectl get pods -n npci-router
kubectl get svc -n npci-router

# View logs
kubectl logs -f deployment/npci-router -n npci-router

# Access metrics
kubectl port-forward -n npci-router svc/npci-router-metrics 9090:9090
curl http://localhost:9090/metrics
```

### Using Helm

```bash
# Install with Helm
helm install npci-router ./helm/npci-router \
  --namespace npci-router \
  --create-namespace \
  --values values.yaml

# Upgrade
helm upgrade npci-router ./helm/npci-router \
  --namespace npci-router

# Uninstall
helm uninstall npci-router --namespace npci-router
```

## Configuration Guide

### Adding a New Tenant

Simply add a new tenant section to your configuration file:

```toml
[tenants.new_bank]
id = "new_bank"
name = "New Bank Ltd"
nat_ip = "192.168.10.20"
certificate_path = "/etc/certs/new_bank-cert.pem"
key_path = "/etc/certs/new_bank-key.pem"
enabled = true

[[tenants.new_bank.backends]]
id = "backend1"
address = "10.0.5.100:8080"
weight = 100
```

Reload configuration:
```bash
# Graceful reload (zero downtime)
killall -HUP npci-router
```

### Adding a New Product

```toml
[products.new_product]
id = "new_product"
name = "New Payment Product"
ips = ["192.168.30.10"]

[[products.new_product.backends]]
id = "backend1"
address = "10.2.1.100:9000"
weight = 100
```

### Routing Strategies

#### 1. IP-Based Routing (Default)

```toml
[tenants.bank1.routing]
strategy = "ip"
```

Routes based on the NAT IP assigned to the tenant.

#### 2. Header-Based Routing

```toml
[tenants.bank1.routing]
strategy = "header"
header_name = "X-Bank-ID"
```

Routes based on a specific header value.

#### 3. Path-Based Routing

```toml
[tenants.bank1.routing]
strategy = "path"
path_prefix = "/bank1"
```

Routes based on URL path prefix.

#### 4. Combined Routing

```toml
[tenants.bank1.routing]
strategy = "combined"
header_name = "X-Bank-ID"
path_prefix = "/bank1"
```

Tries header first, then path, then IP.

## Monitoring

### Prometheus Metrics

The router exposes Prometheus metrics on the configured metrics port (default: 9090):

```bash
curl http://localhost:9090/metrics
```

Key metrics:
- `npci_requests_total` - Total requests per tenant
- `npci_request_duration_seconds` - Request latency histogram
- `npci_bandwidth_in_bytes_total` - Ingress bandwidth
- `npci_bandwidth_out_bytes_total` - Egress bandwidth
- `npci_rate_limit_exceeded_total` - Rate limit violations
- `npci_backend_health` - Backend health status
- `npci_errors_total` - Errors by type

### Health Endpoints

- `GET /health` - Health check (returns 200 if healthy)
- `GET /ready` - Readiness check (returns 200 if ready)
- `GET /metrics` - Prometheus metrics

### Access Logs

Access logs are written to the configured log file with the following format:

```
request_id=xxx tenant=bank1 method=POST status=200 duration=0.123s bytes_in=1024 bytes_out=2048
```

## Bandwidth Tracking

View bandwidth statistics:

```bash
# Export to JSON
curl http://localhost:9090/bandwidth/stats

# View real-time stats
tail -f /var/log/npci-router/bandwidth.json
```

## Rate Limiting

Configure rate limits per tenant:

```toml
[tenants.bank1.rate_limit]
rps = 2000  # Requests per second
burst = 500  # Burst capacity
```

Global rate limit:

```toml
[rate_limiting]
enabled = true
default_rps = 1000
algorithm = "tokenBucket"  # or "slidingWindow", "fixedWindow"
```

## Header Manipulation

Add, remove, or modify headers:

```toml
[tenants.bank1.headers]
add = { "X-Source" = "NPCI", "X-Environment" = "Production" }
remove = ["X-Internal-Token"]
set = { "Host" = "backend.internal" }
response_add = { "X-Router-Version" = "1.0" }
response_remove = ["Server"]
```

## Security

### TLS Configuration

```toml
[tls]
min_version = "1.2"  # or "1.3"
enable_alpn = true
alpn_protocols = ["h2", "http/1.1"]
mtls = false  # Set to true for mutual TLS
```

### Mutual TLS (mTLS)

```toml
[tls]
mtls = true
ca_cert_path = "/etc/certs/ca.pem"
```

### Security Best Practices

1. **Use TLS 1.3**: Set `min_version = "1.3"` for best security
2. **Enable mTLS**: Verify client certificates
3. **Regular Certificate Rotation**: Automate with cert-manager in K8s
4. **Network Policies**: Restrict traffic using Kubernetes NetworkPolicies
5. **Resource Limits**: Set appropriate CPU/memory limits
6. **Read-Only Filesystem**: Run with read-only root filesystem
7. **Non-Root User**: Run as non-root user (UID 1000)

## Performance Tuning

### Connection Pooling

```toml
[[tenants.bank1.backends]]
pool_size = 64  # Increase for high traffic
```

### Worker Threads

```toml
[server]
threads = 16  # Set to number of CPU cores
```

### Timeout Configuration

```toml
[server]
connection_timeout_secs = 30
request_timeout_secs = 30
keepalive_timeout_secs = 60
```

## Troubleshooting

### Check Logs

```bash
# Kubernetes
kubectl logs -f deployment/npci-router -n npci-router

# Local
tail -f /var/log/npci-router/access.log
```

### Debug Mode

```bash
npci-router -c config.toml --log-level debug
```

### Test Configuration

```bash
npci-router -c config.toml --test
```

### Common Issues

#### 1. Certificate Errors

Ensure certificate files exist and have correct permissions:
```bash
ls -l /etc/npci-router/certs/
chmod 600 /etc/npci-router/certs/*.key
```

#### 2. Port Already in Use

Check if port is in use:
```bash
netstat -tulpn | grep 443
```

#### 3. Backend Connection Failures

Check backend health:
```bash
curl http://localhost:9090/metrics | grep npci_backend_health
```

## Development

### Building from Source

```bash
# Debug build
cargo build

# Release build (optimized)
cargo build --release

# Run tests
cargo test

# Run with coverage
cargo tarpaulin --out Html
```

### Running Tests

```bash
# Unit tests
cargo test

# Integration tests
cargo test --test integration

# Specific test
cargo test test_rate_limiting
```

### Code Structure

```
src/
├── main.rs              # Entry point
├── lib.rs               # Library exports
├── config/              # Configuration module
│   ├── mod.rs           # Config structures
│   ├── loader.rs        # Config loading
│   └── validator.rs     # Config validation
├── router/              # Core router
│   ├── mod.rs           # Router implementation
│   └── builder.rs       # Router builder
├── bandwidth/           # Bandwidth tracking
├── ratelimit/           # Rate limiting
├── headers/             # Header manipulation
├── health/              # Health checking
├── tls/                 # TLS management
├── metrics/             # Prometheus metrics
└── error/               # Error handling
```

## Contributing

Contributions are welcome! Please follow these guidelines:

1. Fork the repository
2. Create a feature branch
3. Write tests for new features
4. Ensure all tests pass
5. Submit a pull request

## License

Apache 2.0 - See LICENSE file

## Support

For issues and questions:
- GitHub Issues: [npci-router-adapter/issues](https://github.com/your-org/npci-router-adapter/issues)
- Documentation: [docs/](docs/)

## Acknowledgments

Built on [Pingora](https://github.com/cloudflare/pingora) by Cloudflare.
