<img src="https://r2cdn.perplexity.ai/pplx-full-logo-primary-dark%402x.png" style="height:64px;margin-right:32px"/>

# But npci only provide IPs so no domain name is provided and also the certificate we generate is only with a perticular client

Since NPCI provides only IP addresses without domain names, and certificates are client-specific (likely with IP SANs or client-identified CN), your TLS certificate management and routing must adapt accordingly:

### TLS and Routing Approach for IP-Based Multi-Tenant Setup

#### 1. Client-Specific Certificates Bound to IPs

- Generate TLS certificates with the client's allocated NAT IP as the Subject Alternative Name (SAN).
- Pingora can terminate TLS based on the destination IP (connection’s local IP) rather than SNI, since no domain name is used.


#### 2. TLS Routing Based on Destination IP (IP-based TLS Selection)

- Configure Pingora or your proxy to select certificates according to the **destination IP address** the request hits (the ingress NAT IP assigned by NPCI).
- This requires Pingora to support TLS routing logic based on connection parameters (e.g., local IP) rather than SNI.
- When TCP connection reaches the proxy on a specific ingress IP, Pingora serves that client’s certificate.


#### 3. Multi-Tenant Routing via IP and Headers

- The ingress IP itself can be used to identify the tenant, as NPCI routes requests to specific ingress IPs per tenant.
- Further routing/filtering can use headers or request parameters if needed, but the initial TLS handshake and cert selection depend on the ingress IP.
- Egress NAT IP mapping similarly can be used for outbound client traffic.


#### 4. Certificate Validation for Incoming Requests

- If using mutual TLS (mTLS), client certs must be validated per tenant policy, with trusted CA chains aligned with that tenant’s cert issuance.

***

### Example Configuration Elements

- Map each NPCI-assigned ingress IP to a client certificate in Pingora’s TLS config:

```yaml
tls:
  certificates:
    - ipAddress: 198.51.100.2    # NPCI-assigned ingress IP for Client A
      certFile: /etc/certs/clientA.crt
      keyFile: /etc/certs/clientA.key
    - ipAddress: 198.51.100.3    # NPCI-assigned ingress IP for Client B
      certFile: /etc/certs/clientB.crt
      keyFile: /etc/certs/clientB.key
```

- In the absence of domain or SNI, Pingora’s connection handling filters route based on destination IP.

***

### Summary

- NPCI assigns each client a unique ingress IP; use that IP as the key for certificate selection and initial tenant routing logic.
- Certificates must be IP SAN valid for those IPs.
- Pingora proxy configuration routes TLS termination by destination IP rather than domain.
- Subsequent routing/filtering inside the proxy can use HTTP headers or connection metadata for finer control.
- This complies with NPCI’s IP-only provisioning while ensuring secure TLS using client-specific certs.

This IP-based multi-tenant TLS and routing model fits your NPCI constraints with Pingora acting as a unified gateway proxy. Let me know if you want configuration templates or code snippets for this IP-based TLS selection in Pingora.

