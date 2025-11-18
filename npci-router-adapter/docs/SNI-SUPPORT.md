# SNI (Server Name Indication) Support

## Overview

The NPCI Router now supports **SNI (Server Name Indication)** as a toggleable feature that works alongside traditional IP-based TLS certificate selection. This provides flexibility for both current NPCI requirements (IP-only) and future scenarios where domain names may be available.

## What is SNI?

SNI (Server Name Indication) is a TLS extension that allows a client to specify which hostname it is attempting to connect to at the start of the TLS handshake. This enables a single server with a single IP address to serve multiple domains with different TLS certificates.

## Why Add SNI Support?

1. **Future-Proofing**: While NPCI currently provides only IP addresses, future requirements may include domain-based routing
2. **Flexibility**: Some clients may prefer domain-based connections over IP-only
3. **Multi-Domain Certificates**: Support scenarios where one certificate covers multiple domains
4. **Hybrid Approach**: Can work alongside IP-based routing without disruption

## SNI Modes

The router supports four SNI modes:

### 1. Disabled (Default - NPCI Compatible)
```toml
[tls]
sni_mode = "disabled"
```

- Uses **only** IP-based certificate selection
- Ignores SNI even if provided by clients
- **Recommended for traditional NPCI deployments**
- No changes to existing behavior

### 2. Enabled (Hybrid with Fallback)
```toml
[tls]
sni_mode = "enabled"
prefer_sni = false
```

- Tries IP-based selection first
- Falls back to SNI if IP match fails
- Best for **gradual migration** from IP-only to SNI
- Maintains backward compatibility

### 3. Strict (SNI Required)
```toml
[tls]
sni_mode = "strict"
prefer_sni = true
strict_sni = true
```

- **Requires** SNI from all clients
- Rejects connections without SNI
- Use only when **all** clients support SNI
- Not recommended for NPCI environments

### 4. Hybrid (Most Flexible)
```toml
[tls]
sni_mode = "hybrid"
prefer_sni = false  # or true, depending on preference
```

- Supports **both** IP and SNI simultaneously
- Can prefer one method over the other
- **Recommended for mixed environments**
- Maximum flexibility

## Configuration

### Global TLS Configuration

```toml
[tls]
min_version = "1.2"
enable_alpn = true
alpn_protocols = ["h2", "http/1.1"]

# SNI settings
sni_mode = "hybrid"           # disabled, enabled, strict, hybrid
prefer_sni = false            # Use IP first, then SNI
strict_sni = false            # Don't reject connections without SNI
```

### Per-Tenant SNI Configuration

#### Option 1: IP-Only (Traditional NPCI)

```toml
[tenants.hdfc]
id = "hdfc"
name = "HDFC Bank"
nat_ip = "192.168.10.10"
certificate_path = "/etc/certs/hdfc-cert.pem"
key_path = "/etc/certs/hdfc-key.pem"

[tenants.hdfc.sni]
enabled = false              # SNI disabled for this tenant
hostnames = []
fallback_to_ip = true
```

#### Option 2: SNI Enabled with IP Fallback

```toml
[tenants.icici]
id = "icici"
name = "ICICI Bank"
nat_ip = "192.168.10.11"
certificate_path = "/etc/certs/icici-cert.pem"
key_path = "/etc/certs/icici-key.pem"

[tenants.icici.sni]
enabled = true
hostnames = ["icici.npci.internal", "api.icici.npci.internal"]
fallback_to_ip = true        # Still works with IP if SNI not provided
```

#### Option 3: SNI-Only (Future Scenario)

```toml
[tenants.future_bank]
id = "future_bank"
name = "Future Bank"
nat_ip = "192.168.10.12"
certificate_path = "/etc/certs/future-cert.pem"
key_path = "/etc/certs/future-key.pem"

[tenants.future_bank.sni]
enabled = true
hostnames = ["bank.example.com", "api.bank.example.com"]
fallback_to_ip = false       # Require SNI, don't fallback to IP
```

## How Certificate Selection Works

### When Global SNI Mode = Disabled
1. **Always** use IP-based certificate selection
2. Ignore SNI from clients
3. Same behavior as before

### When Global SNI Mode = Enabled
1. Check `prefer_sni` setting
2. If `prefer_sni = false`:
   - Try IP-based selection first
   - If no match, try SNI
3. If `prefer_sni = true`:
   - Try SNI first (if provided)
   - If no match, try IP-based

### When Global SNI Mode = Strict
1. **Require** SNI from client
2. Reject connections without SNI
3. Use only SNI-based selection

### When Global SNI Mode = Hybrid
1. Support both IP and SNI lookups
2. Use `prefer_sni` to determine priority
3. Most flexible option

## Migration Path

### Phase 1: Current (IP-Only)
```toml
[tls]
sni_mode = "disabled"

[tenants.bank1.sni]
enabled = false
```

- No changes to existing setup
- Traditional NPCI IP-based routing

### Phase 2: Testing (Hybrid)
```toml
[tls]
sni_mode = "hybrid"
prefer_sni = false  # Keep IP as primary

[tenants.bank1.sni]
enabled = true
hostnames = ["bank1.npci.internal"]
fallback_to_ip = true
```

- Enable SNI for testing
- IP still works as primary
- No disruption to existing clients

### Phase 3: Migration (Prefer SNI)
```toml
[tls]
sni_mode = "hybrid"
prefer_sni = true  # Prefer SNI over IP

[tenants.bank1.sni]
enabled = true
hostnames = ["bank1.npci.internal"]
fallback_to_ip = true
```

- SNI becomes primary method
- IP still available as fallback
- Gradual client migration

### Phase 4: Future (SNI-Only)
```toml
[tls]
sni_mode = "strict"
prefer_sni = true
strict_sni = true

[tenants.bank1.sni]
enabled = true
hostnames = ["bank1.npci.internal"]
fallback_to_ip = false
```

- All clients must support SNI
- IP-based routing disabled
- Full domain-based routing

## Testing SNI

### Test IP-Based Connection
```bash
curl -v --resolve bank1.npci.internal:443:192.168.10.10 \
     https://192.168.10.10/api/test
```

### Test SNI-Based Connection
```bash
curl -v --resolve bank1.npci.internal:443:192.168.10.10 \
     https://bank1.npci.internal/api/test
```

### Test with OpenSSL
```bash
# Without SNI
openssl s_client -connect 192.168.10.10:443

# With SNI
openssl s_client -connect 192.168.10.10:443 \
                 -servername bank1.npci.internal
```

## Monitoring

### Metrics

The router exports metrics to track SNI usage:

```prometheus
# Certificate selection method
npci_tls_certificate_selection_total{method="ip"} 1000
npci_tls_certificate_selection_total{method="sni"} 500
npci_tls_certificate_selection_total{method="fallback"} 50

# SNI errors
npci_tls_sni_missing_total 10
npci_tls_sni_mismatch_total 5
```

### Logs

```
# IP-based selection
INFO Selected certificate for IP: 192.168.10.10, tenant: hdfc

# SNI-based selection
DEBUG Selected certificate using SNI: icici.npci.internal
INFO Certificate selected via SNI for tenant: icici

# Fallback
DEBUG SNI hostname not found, falling back to IP-based selection
INFO Fallback to IP-based certificate for 192.168.10.11
```

## Best Practices

1. **Start with Hybrid Mode**: Set `sni_mode = "hybrid"` and `prefer_sni = false`
2. **Test Gradually**: Enable SNI per-tenant, not globally
3. **Monitor Metrics**: Watch for SNI vs IP usage patterns
4. **Keep Fallback Enabled**: Set `fallback_to_ip = true` during migration
5. **Document Hostnames**: Keep clear records of SNI hostnames per tenant
6. **Certificate SAN**: Ensure certificates include both IP SANs and domain SANs

## Troubleshooting

### Issue: Certificate Not Found for SNI

**Symptom**: `ERROR No certificate found for SNI hostname: example.com`

**Solution**:
1. Check tenant SNI configuration:
   ```toml
   [tenants.tenant1.sni]
   enabled = true
   hostnames = ["example.com"]  # Must match exactly
   ```
2. Verify certificate is registered
3. Check logs for certificate registration

### Issue: Strict SNI Rejects Connections

**Symptom**: `WARN Strict SNI mode: connection rejected without SNI`

**Solution**:
1. Change to hybrid mode temporarily:
   ```toml
   [tls]
   sni_mode = "hybrid"
   strict_sni = false
   ```
2. Or ensure all clients send SNI

### Issue: SNI Ignored

**Symptom**: SNI provided but IP-based cert used

**Solution**:
1. Check global SNI mode:
   ```toml
   [tls]
   sni_mode = "enabled"  # or "hybrid"
   ```
2. Check prefer_sni setting:
   ```toml
   prefer_sni = true  # To prefer SNI
   ```

## Security Considerations

1. **Certificate Validation**: Both IP SANs and DNS SANs should be validated
2. **SNI Spoofing**: While SNI is not encrypted in TLS 1.2, it's visible to intermediaries
3. **TLS 1.3 Encrypted SNI**: Consider upgrading to TLS 1.3 for eSNI support
4. **Strict Mode**: Use `strict_sni = true` only when all clients are trusted

## Compatibility

| Feature | IP-Only | SNI-Enabled | Strict SNI |
|---------|---------|-------------|------------|
| NPCI Traditional Setup | ✅ | ✅ | ❌ |
| Domain-based Clients | ❌ | ✅ | ✅ |
| Mixed Environment | ✅ | ✅ | ❌ |
| Future-Proof | ⚠️ | ✅ | ✅ |

## Summary

SNI support in the NPCI Router provides:

- ✅ **Backward Compatibility**: IP-based routing still works
- ✅ **Future-Ready**: SNI available when needed
- ✅ **Toggleable**: Enable/disable per tenant
- ✅ **Flexible**: Multiple modes (disabled, enabled, strict, hybrid)
- ✅ **Zero Disruption**: Can be enabled without affecting existing deployments

**Default Configuration**: SNI is **disabled** by default, maintaining full compatibility with traditional NPCI IP-only setups. Enable it when you're ready!
