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

## Examples
TBD
