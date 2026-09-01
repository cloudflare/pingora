# `Peer`: how to connect to upstream

In the `upstream_peer()` phase the user should return a `Peer` object which defines how to connect to a certain upstream.

## `Peer`
A `HttpPeer` defines which upstream to connect to.
| attribute      | meaning        |
| ------------- |-------------|
|address: `SocketAddr`| The IP:Port to connect to |
|scheme: `Scheme`| Http or Https |
|sni: `String`| The SNI to use, Https only |
|proxy: `Option<Proxy>`| The setting to proxy the request through a [CONNECT proxy](https://developer.mozilla.org/en-US/docs/Web/HTTP/Methods/CONNECT) |
|client_cert_key: `Option<Arc<CertKey>>`| The client certificate to use in mTLS connections to upstream |
|options: `PeerOptions`| See below |


## `PeerOptions`
A `PeerOptions` defines how to connect to the upstream.
| attribute      | meaning        |
| ------------- |-------------|
|bind_to: `Option<InetSocketAddr>`| Which local address to bind to as the client IP |
|connection_timeout: `Option<Duration>`| How long to wait before giving up *establishing* a TCP connection |
|total_connection_timeout: `Option<Duration>`| How long to wait before giving up *establishing* a connection including TLS handshake time |
|read_timeout: `Option<Duration>`| How long to wait before each individual `read()` from upstream. The timer is reset after each `read()` |
|idle_timeout: `Option<Duration>`| How long to wait before closing a idle connection waiting for connection reuse |
|write_timeout: `Option<Duration>`| How long to wait before a `write()` to upstream finishes |
|verify_cert: `bool`| Whether to check if upstream' server cert is valid and validated |
|verify_hostname: `bool`| Whether to check if upstream server cert's CN matches the SNI |
|use_system_certs: `bool`| Whether the system trust store should be loaded and used when verifying certificates. Impacts performance (s2n-tls only) |
|alternative_cn: `Option<String>`| Accept the cert if the CN matches this name |
|alpn: `ALPN`| Which HTTP protocol to advertise during ALPN, http1.1 and/or http2 |
|ca: `Option<Arc<Box<[X509]>>>`| Which Root CA to use to validate the server's cert |
|psk: `Option<Arc<PskConfig>>` | The PSK configuration to use in [PSK-TLS](https://datatracker.ietf.org/doc/html/rfc4279) handshakes (s2n-tls only) |
|s2n_security_policy: `Option<S2NPolicy>` | S2N [Security Policy](https://aws.github.io/s2n-tls/usage-guide/ch06-security-policies.html) to use. Defaults to `default_tls13` if undefined. (s2n-tls only) |
|max_blinding_delay: `Option<u32>` | S2N-TLS will delay a response up to the [max blinding delay](https://aws.github.io/s2n-tls/usage-guide/ch03-error-handling.html#blinding) (default 30) seconds whenever an error triggered by a peer occurs to mitigate against timing side channels. (s2n-tls only) |
|tcp_keepalive: `Option<TcpKeepalive>`| TCP keepalive settings to upstream |
|http_upstream_request_policy: `HttpUpstreamRequestPolicy`| Controls automatic forwarding of hop-by-hop request headers and HTTP/1 upgrades |

### HTTP upstream request header policy

By default, Pingora strips downstream hop-by-hop request fields and fields nominated by
`Connection` before sending a request to an HTTP upstream. For an HTTP/1 upstream, if a non-empty
downstream request body is left without `Content-Length` or `Transfer-Encoding` after
`upstream_request_filter()` runs, Pingora adds `Transfer-Encoding: chunked`. This allows an
application filter to remove a known content length while continuing to stream the resulting
body. Valid WebSocket upgrade handshakes are forwarded in a normalized form; other HTTP/1
upgrades are not forwarded by default. Requests sent to an HTTP/1 upstream use HTTP/1.1 on the
wire.

When nominated-field stripping is enabled, requests are rejected if `Connection` nominates
`Host`, `X-Forwarded-For`, `X-Forwarded-Host`, `X-Forwarded-Proto`, or a pseudo-header-shaped
field such as `:authority`. Pingora also rejects requests with ten or more connection
nominations. These checks prevent sanitization from silently removing routing or request-origin
metadata.

For compatibility with an upstream that requires the previous passthrough behavior:

```rust
use pingora_core::upstreams::peer::HttpUpstreamRequestPolicy;

peer.options.http_upstream_request_policy = HttpUpstreamRequestPolicy::preserve();
```

This preset is RFC-non-compliant and must be used at your own risk. An application's
`upstream_request_filter()` becomes solely responsible for valid hop-by-hop request handling.

To retain only fields nominated in `Connection` while retaining the other default behavior:

```rust
peer.options
    .http_upstream_request_policy
    .strip_connection_nominated = false;
```

To preserve complete HTTP/1 upgrade handshakes while retaining ordinary request normalization:

```rust
use pingora_core::upstreams::peer::H1UpgradePolicy;

peer.options.http_upstream_request_policy.h1_upgrade = H1UpgradePolicy::Preserve;
```

This preserves complete request metadata for requests containing `Upgrade`, since an arbitrary
protocol handshake can depend on any connection-nominated field. This mode is RFC-non-compliant
and must be used at your own risk; protected nomination validation applies when
`strip_connection_nominated` is enabled.

## Examples
TBD
