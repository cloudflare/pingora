<img src="https://r2cdn.perplexity.ai/pplx-full-logo-primary-dark%402x.png" style="height:64px;margin-right:32px"/>

# Design a NAT IP multiplexing pattern for multi tenant routing

A resilient NAT IP multiplexing pattern for multi-tenant routing leverages centralized proxying, dynamic mapping, and intelligent routing to expose only a few public IPs while flexibly handling traffic for many tenants behind the scenes. This pattern is ideal for scenarios like your NPCI integration, where you want to minimize whitelist complexity yet maintain full tenant isolation and scalability.[^1][^2][^3]

### Core Design Principles

- Expose only two public NAT IPs: one for ingress, one for egress.
- Use a programmable proxy (e.g., Pingora or competitors) as the NAT traffic broker.
- Maintain tenant-to-NAT-IP mapping in a central database or config, dynamically referenced by the proxy for routing decisions.[^4]
- Internally, tenant traffic is multiplexed over private subnets, with tenant-specific overlay networks (VXLAN/GRE) or virtualized routing domains (VRFs) for isolation.[^5][^6]
- Routing decisions use header metadata, source IP, or tenant tokens to select the correct downstream NAT mapping.
- Connection pooling and failover improve performance and resilience.[^7][^4]


### Pattern Diagram

```
[NPCI]
  |
[Public NAT IPs]
  |         |
[Ingress Proxy] -----> [Tenant 1 Network]
  |         |
             -----> [Tenant 2 Network]
             -----> [Tenant N Network]
        |
[Egress Proxy]
  |
[NPCI]
```


### Key Components

- **Ingress Proxy/NAT:** Receives all inbound traffic from NPCI on a single public IP, runs programmable logic (Pingora recommended) to identify tenant context and routes to the correct backend tenant network/private NAT IP.[^7][^4]
- **Egress Proxy/NAT:** All outgoing traffic destined for NPCI is source-NATed to the designated egress public IP, abstracting all tenant details.
- **Tenant Isolation:** Use VRFs or K8s namespaces for network isolation, applying network policies to restrict lateral movement.[^8][^9][^10]
- **Multiplex Logic:** Proxy uses SNI, HTTP headers, JWT, or source IP mapping to determine tenant origin and route accordingly, leveraging a mapping database with tenant-to-NAT-IP relationships.
- **Resilience:** Kubernetes delivers self-healing and autoscaling; deploy multiple Proxy/NAT pods behind load balancers, with automatic failover.[^11]


### Enhancements for Production Grade

- Store tenant-NAT mapping in a dynamic backend (e.g., Redis or etcd) for live updates.
- Use programmable filters in Pingora to inspect and route traffic as required.
- Leverage health checks, readiness probes, and application-level failover to avoid downtime.[^4][^7]
- Enable logging and metrics per tenant for monitoring and debugging.


### Example: Pingora-driven Multiplexing

Pingora allows full customization at the routing stage. Each incoming request triggers user-defined code to look up the target tenant based on request properties, then routes to the right backend NAT or network segment.[^12][^4]

### Advantages

- NPCI whitelists only two IPs, simplifying their operations and yours.
- Tenants are isolated, each getting custom routing, monitoring, and failovers.
- Scaling, onboarding, or decommissioning tenants requires only an update to the mapping database, not changes to NPCI or public NAT IPs.

This pattern is robust, highly available, scalable, and ideal for cloud-native multi-tenant architectures, especially when combined with Kubernetes orchestration.[^3][^7][^4]
<span style="display:none">[^13][^14][^15][^16][^17][^18][^19][^20]</span>

<div align="center">⁂</div>

[^1]: https://bluecatnetworks.com/blog/multi-tenancy-and-ipam/

[^2]: https://learn.microsoft.com/en-us/azure/architecture/guide/multitenant/service/private-link

[^3]: https://learn.microsoft.com/en-us/azure/architecture/guide/multitenant/service/nat-gateway

[^4]: https://blog.cloudflare.com/pingora-open-source/

[^5]: https://openmetal.io/resources/blog/multi-tenant-openstack-architecture-basics/

[^6]: https://www.cisco.com/c/en/us/td/docs/solutions/Enterprise/Data_Center/VMDC/2-0/design_guide/vmdcDesignGuideCompactPoD20/design.html

[^7]: https://www.linkedin.com/pulse/unveiling-pingora-cloudflares-game-changer-open-source-http-5hsvc

[^8]: https://www.tigera.io/learn/guides/kubernetes-security/kubernetes-multi-tenancy/

[^9]: https://kubernetes.io/docs/concepts/security/multi-tenancy/

[^10]: https://www.sysdig.com/blog/multi-tenant-isolation-boundaries-kubernetes

[^11]: https://dev.to/gerimate/streamlining-multi-tenant-kubernetes-a-practical-implementation-guide-for-2025-1bin

[^12]: https://rpallas92.github.io/pingora-internals-1-async-runtime/

[^13]: https://frontegg.com/guides/multi-tenant-architecture

[^14]: https://www.geeksforgeeks.org/system-design/multi-tenancy-architecture-system-design/

[^15]: https://blah.cloud/infrastructure/designing-modern-private-cloud-network/

[^16]: https://www.nagarro.com/en/blog/architectural-design-patterns-aws-multi-tenancy

[^17]: https://aws.amazon.com/blogs/networking-and-content-delivery/secure-customer-resource-access-in-multi-tenant-saas-with-amazon-vpc-lattice/

[^18]: https://daily.dev/blog/multi-tenant-database-design-patterns-2024

[^19]: https://github.com/cloudflare/pingora

[^20]: https://portal.nutanix.com/docs/Self-Service-Admin-Operations-Guide-v4_2_1:nuc-app-mgmt-multi-tenant-capability-sp-c.html

