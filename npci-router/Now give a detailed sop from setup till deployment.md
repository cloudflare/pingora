<img src="https://r2cdn.perplexity.ai/pplx-full-logo-primary-dark%402x.png" style="height:64px;margin-right:32px"/>

# Now give a detailed sop from setup till deployment and operation with detailed hand book

Here is a comprehensive Standard Operating Procedure (SOP) handbook detailing the complete setup, deployment, and operation of your multi-tenant NAT IP multiplexing gateway using Pingora on Kubernetes, incorporating IP-based TLS certificates per client, customized routing, and NPCI compliance:

***

# SOP Handbook: Multi-Tenant NAT IP Multiplexing Gateway with Pingora


***

## 1. Overview

This SOP describes how to deploy and operate a Kubernetes-based multi-tenant NAT proxy using Pingora, exposing fixed ingress and egress IPs to NPCI, routing traffic internally based on tenant IP mappings, and enforcing client-specific TLS certificates bound to those IPs for secure communication.

***

## 2. Prerequisites

- Kubernetes cluster (v1.25+) with admin access and node capacity
- kubectl command-line tool configured
- Persistent external static IPs assigned (minimum 2: ingress and egress)
- Access to generate client certificates with IP Subject Alternative Names (SAN)
- Rust toolchain (optional, if building Pingora from source)
- Helm 3.x installed (optional, for automated installs)
- Network readiness to route NPCI traffic to the Kubernetes LoadBalancer IPs
- Certificate management mechanism (static secrets or automation)

***

## 3. Setup and Installation

### 3.1 Download and Build Pingora (Optional)

```bash
git clone https://github.com/cloudflare/pingora.git
cd pingora
cargo build --release
```

Or use the official container images from Docker Hub or your trusted registry:

```bash
docker pull cloudflare/pingora:latest
```


***

### 3.2 Prepare Kubernetes Resources

#### A. Create Namespace

```bash
kubectl create namespace pingora-multitenant
```


#### B. Create TLS Secrets Per Tenant

Generate IP SAN certificates externally per client IP, then create Kubernetes TLS secrets:

```bash
kubectl create secret tls tenant-A-cert --cert=tenant-A.crt --key=tenant-A.key -n pingora-multitenant
kubectl create secret tls tenant-B-cert --cert=tenant-B.crt --key=tenant-B.key -n pingora-multitenant
# Repeat for all tenants
```


***

### 3.3 Create ConfigMap for Tenant Routing

Define tenant routings by ingress IP, backend NAT IP, or internal backend service:

```yaml
apiVersion: v1
kind: ConfigMap
metadata:
  name: pingora-tenant-routing
  namespace: pingora-multitenant
data:
  routing.json: |
    {
      "198.51.100.2": {
        "tenant": "tenant-A",
        "backend": "10.244.1.5", 
        "egressIP": "203.0.113.2"
      },
      "198.51.100.3": {
        "tenant": "tenant-B",
        "backend": "10.244.1.6",
        "egressIP": "203.0.113.3"
      }
    }
```

Deploy:

```bash
kubectl apply -f pingora-tenant-routing-configmap.yaml
```


***

### 3.4 Create Deployment \& Service with LoadBalancer IPs

Sample snippet to assign NPCI static IP to LoadBalancer service:

```yaml
apiVersion: v1
kind: Service
metadata:
  name: pingora-ingress
  namespace: pingora-multitenant
spec:
  type: LoadBalancer
  loadBalancerIP: 198.51.100.2  # NPCI ingress IP for tenant-A
  selector:
    app: pingora
  ports:
    - port: 443
      targetPort: 8443
---
apiVersion: apps/v1
kind: Deployment
metadata:
  name: pingora
  namespace: pingora-multitenant
spec:
  replicas: 3
  selector:
    matchLabels:
      app: pingora
  template:
    metadata:
      labels:
        app: pingora
    spec:
      containers:
      - name: pingora
        image: cloudflare/pingora:latest
        ports:
        - containerPort: 8443
        volumeMounts:
          - name: certs
            mountPath: /etc/pingora/certs
            readOnly: true
        env:
          - name: ROUTING_CONFIG_PATH
            value: /etc/pingora/config/routing.json
      volumes:
        - name: certs
          projected:
            sources:
              - secret:
                  name: tenant-A-cert
                  items:
                    - key: tls.crt
                      path: tenant-A.crt
                    - key: tls.key
                      path: tenant-A.key
              - secret:
                  name: tenant-B-cert
                  items:
                    - key: tls.crt
                      path: tenant-B.crt
                    - key: tls.key
                      path: tenant-B.key
              - configMap:
                  name: pingora-tenant-routing
                  items:
                    - key: routing.json
                      path: routing.json
```


***

## 4. Pingora Configuration and Customization

### 4.1 TLS Handling \& Routing Logic

- Pingora must be configured to select TLS certs based on destination ingress IP.
- Implement Rust plugin or Lua filter (if supported) to read client IP and route to tenant backend accordingly.
- Use provided routing config (ConfigMap mounted in container) to lookup tenants and backends dynamically.

Example pseudo-code:

```rust
fn select_cert_and_route(connection: &Connection) -> Result<Route, Error> {
    let dest_ip = connection.local_ip();
    let config = load_routing_config()?;
    if let Some(tenant) = config.get(&dest_ip) {
        let cert = load_cert_for_ip(dest_ip)?;
        connection.set_certificate(cert);
        Ok(Route::new(tenant.backend))
    } else {
        Err(Error::TenantNotFound)
    }
}
```


***

### 4.2 Connection Pooling \& Failover

- Enable connection reuse options in Pingora config to minimize latency.
- Implement health checks and readiness probes for backend failures and pod restarts.
- Configure Kubernetes Horizontal Pod Autoscaler based on CPU/memory or custom Pingora metrics.

***

## 5. Deployment Steps

1. Apply namespace, secrets (certs), configmap (routing), deployment, and services manifests
2. Wait for pods to become Ready (`kubectl get pods -n pingora-multitenant`)
3. Confirm LoadBalancer IPs are assigned (`kubectl get svc -n pingora-multitenant`)
4. Test access using client certs and NPCI whitelisted IPs
5. Monitor logs and metrics, check connection pooling in proxy logs

***

## 6. Operation and Maintenance

### 6.1 Monitoring

- Use Prometheus exporters exposed by Pingora or custom exporter scripts.
- Monitor health endpoints and pod status.
- Log tenant access and TLS handshake metrics.


### 6.2 Certificate Renewal

- Automate cert renewal with external tools (e.g., cert-manager) via IP-SAN cert requests.
- Reload config or rollout pingora pods on secret updates.


### 6.3 Scaling and Upgrades

- Use Kubernetes rolling updates for Pingora container updates.
- Scale replicas horizontally based on load.

***

## 7. Troubleshooting

| Issue | Cause | Action |
| :-- | :-- | :-- |
| Backend unreachable | Routing config error | Check ConfigMap, pod logs |
| TLS handshake failure | Incorrect cert loaded | Verify cert IP SAN, reload pod |
| High latency or timeouts | No connection pooling | Enable/reconfigure pooling |
| Pod crash or restart loop | Misconfiguration or resource | Check logs, increase resources |


***

## 8. Security Considerations

- Use TLS 1.3 and strong cipher suites.
- Limit secret access tightly, enable K8s RBAC.
- Enable mutual TLS if required for strong client authentication.
- Regular certificate audits and rotation.

***

This SOP enables your team to deploy, operate, and maintain a resilient, secure, multi-tenant NAT IP multiplexing gateway using Pingora and Kubernetes, fulfilling NPCI IP-based and client certificate requirements.

If needed, example manifests and Rust code snippets for the routing/filter logic can be provided for faster development. Would you require those?

