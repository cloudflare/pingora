# Multi-stage Docker build for NPCI Router

# Stage 1: Build
FROM rust:1.82-slim AS builder

# Install build dependencies
RUN apt-get update && apt-get install -y \
    pkg-config \
    libssl-dev \
    cmake \
    clang \
    perl \
    && rm -rf /var/lib/apt/lists/*

# Create app directory
WORKDIR /app

# Copy manifests
COPY Cargo.toml Cargo.lock ./

# Copy source code
COPY src ./src

# Copy pingora dependencies (adjust paths as needed)
COPY ../pingora-core ../pingora-core
COPY ../pingora-proxy ../pingora-proxy
COPY ../pingora-load-balancing ../pingora-load-balancing
COPY ../pingora-http ../pingora-http
COPY ../pingora-limits ../pingora-limits
COPY ../pingora-error ../pingora-error

# Build the project
RUN cargo build --release

# Stage 2: Runtime
FROM debian:bookworm-slim

# Install runtime dependencies
RUN apt-get update && apt-get install -y \
    ca-certificates \
    libssl3 \
    && rm -rf /var/lib/apt/lists/*

# Create non-root user
RUN groupadd -r npci && useradd -r -g npci -u 1000 npci

# Create directories
RUN mkdir -p /etc/npci-router /var/log/npci-router && \
    chown -R npci:npci /etc/npci-router /var/log/npci-router

# Copy binary from builder
COPY --from=builder /app/target/release/npci-router /usr/local/bin/npci-router

# Copy default configuration
COPY config/example.toml /etc/npci-router/config.toml

# Set ownership
RUN chown -R npci:npci /usr/local/bin/npci-router /etc/npci-router

# Switch to non-root user
USER npci

# Expose ports
EXPOSE 443 8443 9090

# Health check
HEALTHCHECK --interval=10s --timeout=5s --start-period=30s --retries=3 \
    CMD curl -f http://localhost:9090/health || exit 1

# Run the router
ENTRYPOINT ["/usr/local/bin/npci-router"]
CMD ["-c", "/etc/npci-router/config.toml"]
