# `Peer`: 如何连接到上游

在 `upstream_peer()` 阶段，用户应返回一个 `Peer` 对象，该对象定义了如何连接到指定的上游。

## `Peer`
`HttpPeer` 定义了要连接到的上游。
| 属性      | 含义        |
| ------------- |-------------|
|address: `SocketAddr`| 要连接的 IP:Port |
|scheme: `Scheme`| Http 或 Https |
|sni: `String`| 要使用的 SNI，仅限 Https |
|proxy: `Option<Proxy>`| 设置是否通过 [CONNECT 代理](https://developer.mozilla.org/en-US/docs/Web/HTTP/Methods/CONNECT) 代理请求 |
|client_cert_key: `Option<Arc<CertKey>>`| 在与上游的 mTLS 连接中要使用的客户端证书 |
|options: `PeerOptions`| 详见下文 |


## `PeerOptions`
`PeerOptions` 定义了如何连接到上游。
| 属性      | 含义        |
| ------------- |-------------|
|bind_to: `Option<InetSocketAddr>`| 作为客户端 IP 绑定到哪个本地地址 |
|connection_timeout: `Option<Duration>`| 在放弃 *建立* TCP 连接之前等待多长时间 |
|total_connection_timeout: `Option<Duration>`| 在放弃 *建立* 连接之前等待多长时间，包括 TLS 握手时间 |
|read_timeout: `Option<Duration>`| 在从上游进行每个 `read()` 之前等待多长时间。每次 `read()` 后计时器会被重置 |
|idle_timeout: `Option<Duration>`| 在等待连接复用的空闲连接关闭之前等待多长时间 |
|write_timeout: `Option<Duration>`| 在向上游进行 `write()` 完成之前等待多长时间 |
|verify_cert: `bool`| 是否检查上游的服务器证书是否有效和经过验证 |
|verify_hostname: `bool`| 是否检查上游服务器证书的 CN 是否与 SNI 匹配 |
|alternative_cn: `Option<String>`| 如果 CN 与此名称匹配，则接受证书 |
|alpn: `ALPN`| 在 ALPN 期间要广告的 HTTP 协议，http1.1 和/或 http2 |
|ca: `Option<Arc<Box<[X509]>>>`| 要用于验证服务器证书的根 CA |
|tcp_keepalive: `Option<TcpKeepalive>`| 上游的 TCP keepalive 设置 |

## 示例
待定
