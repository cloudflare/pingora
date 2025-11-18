<img src="https://r2cdn.perplexity.ai/pplx-full-logo-primary-dark%402x.png" style="height:64px;margin-right:32px"/>

# Can you now give me a run book to install and make this setup operational and create task to do it and the configuration to have this thing operational. And also give all the configuration and customisation I have to do it in pringora and make it work like a unified gateway and all the kubernetes helm file to deploy and test it

Here is a detailed runbook, tasks, and example configurations to install and operate a unified multi-tenant NAT IP multiplexing gateway using Pingora on Kubernetes:

***

## Runbook and Tasks for Pingora Multi-Tenant NAT Proxy Setup

### Prerequisites

- A Kubernetes cluster (v1.25+) with kubectl access
- Rust toolchain (v1.82+) for building Pingora if using source
- At least 2 static external IPs assigned for ingress and egress NAT exposure
- DNS and network setup to allow these IPs routing to Kubernetes LoadBalancer
- Clang and Perl installed on the build host (for dependencies like boringssl and openssl)

***

### Step 1: Pingora Installation

1. **Clone Pingora repository:**

```bash
git clone https://github.com/cloudflare/pingora.git
cd pingora
```

2. **Build Pingora (optional if using prebuilt binary/container):**

```bash
cargo build --release
# or docker build -t custom-pingora . 
```

3. **Alternatively, use official Pingora container images (recommended for prod).**

***

### Step 2: Prepare Kubernetes Deployment Files (Helm Chart or YAML)

- Create a Helm chart or Kubernetes manifests with these components:

**Helm values.yaml example snippet:**

```yaml
replicaCount: 3
image:
  repository: cloudflare/pingora
  tag: latest
  pullPolicy: IfNotPresent

service:
  type: LoadBalancer
  ingressIP: <INGRESS_STATIC_IP> # NPCI whitelisted ingress IP
  egressIP: <EGRESS_STATIC_IP>   # NPCI whitelisted egress IP

resources:
  limits:
    cpu: "2"
    memory: "2Gi"
  requests:
    cpu: "500m"
    memory: "1Gi"

config:
  routingTable:
    tenant1:
      natIP: 192.168.10.2
      backend: tenant1-backend.svc.cluster.local
    tenant2:
      natIP: 192.168.10.3
      backend: tenant2-backend.svc.cluster.local
  ingressIP: <INGRESS_STATIC_IP>
  egressIP: <EGRESS_STATIC_IP>

```

**Deployment Notes:**

- Expose two services (or one with dual listeners) for ingress and egress IPs.
- Configure Kubernetes to assign static IPs to LoadBalancer services.
- Use ConfigMap or a Secret for tenant routing config if preferences dictate.

***

### Step 3: Pingora Configuration \& Customization for Multi-Tenant Routing

- Write custom Rust plugins/filters or configure Pingora with the following logic:

```rust
// Pseudo-code routing filter

fn route_request(req: &Request) -> Result<Route, Error> {
  // Extract tenant ID from header or path
  let tenant_id = req.headers.get("X-Tenant-ID").or(parse_path_for_tenant(req.path));
  
  // Lookup NAT backend IP from tenant routing table
  let backend_ip = ROUTING_TABLE.get(tenant_id)?;

  Ok(Route::new(backend_ip))
}
```

- Enable connection pooling and keepalive to maximize performance.
- Utilize health check endpoints to detect backend availability and failover.
- Gracefully reload configuration on updates without downtime.

***

### Step 4: Deploy to Kubernetes

- Package your Helm chart or apply manifests:

```bash
helm install pingora ./pingora-chart
# Or kubectl apply -f pingora-deployment.yaml
```

- Monitor pods:

```bash
kubectl get pods -l app=pingora
kubectl logs -f <pod-name>
```

- Verify LoadBalancer IP allocation:

```bash
kubectl get svc pingora-ingress
kubectl get svc pingora-egress
```


***

### Step 5: Test the Setup

- Send sample requests with tenant headers to ingress IP:

```bash
curl -H "X-Tenant-ID: tenant1" http://<INGRESS_STATIC_IP>/api/...
curl -H "X-Tenant-ID: tenant2" http://<INGRESS_STATIC_IP>/api/...
```

- Verify routed traffic reaches expected backend and outgoing to NPCI uses egress IP.
- Simulate failover by killing backend pod, ensure Pingora routes to healthy backend.

***

### Step 6: Monitoring \& Logging

- Configure Pingora metrics endpoint (Prometheus exporter).
- Use Kubernetes probes for liveness and readiness.
- Log tenant-level routing info for audit and debugging.

***

## Summary of Key Configurations for Pingora

| Config | Purpose | Example |
| :-- | :-- | :-- |
| Routing Table | Maps tenant ID/header to NAT IP/backend | tenant1 -> 192.168.10.2 |
| Ingress IP | Public static IP exposed to NPCI ingress | 203.0.113.100 |
| Egress IP | Public static IP for outgoing traffic | 203.0.113.101 |
| Connection Pooling | Reuse connections for performance | Enabled in proxy config |
| Health Checks | Backend health detection \& failover | HTTP probe to backend endpoint |


***

## A Minimal Example Helm Chart Structure

```
pingora-chart/
  ├─ charts/
  ├─ templates/
  │   ├─ deployment.yaml
  │   ├─ service.yaml
  │   ├─ configmap.yaml
  │   ├─ ingress.yaml
  ├─ values.yaml
  ├─ Chart.yaml
  ├─ README.md
```


***

This runbook and setup will allow you to deploy a unified gateway proxy that multiplexes multi-tenant NAT IP routing via Pingora in Kubernetes, exposes fixed whitelisted IPs to NPCI, and is resilient, scalable, and self-healing.

Would you like example Kubernetes manifests or a detailed code example for the routing filter logic in Pingora?

