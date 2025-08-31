//! This example demonstrates to how to implement a custom L4 connector
//! together with a virtual socket.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;

use async_trait::async_trait;
use pingora_core::connectors::L4Connect;
use pingora_core::prelude::HttpPeer;
use pingora_core::protocols::l4::socket::SocketAddr as L4SocketAddr;
use pingora_core::protocols::l4::stream::Stream;
use pingora_core::protocols::l4::virt::{VirtualSocket, VirtualSocketStream};
use pingora_core::server::RunArgs;
use pingora_core::server::{configuration::ServerConf, Server};
use pingora_core::services::listening::Service;
use pingora_core::upstreams::peer::PeerOptions;
use pingora_error::Result;
use pingora_proxy::{http_proxy_service_with_name, prelude::*, HttpProxy, ProxyHttp};
use tokio::io::{AsyncRead, AsyncWrite};

/// Static virtual socket that serves a single HTTP request with a static response.
///
/// In real world use cases you would implement [`VirtualSocket`] for streams
/// that implement `AsyncRead + AsyncWrite`.
#[derive(Debug)]
struct StaticVirtualSocket {
    content: Vec<u8>,
    read_pos: usize,
}

impl StaticVirtualSocket {
    fn new() -> Self {
        let response = b"HTTP/1.1 200 OK\r\nContent-Length: 13\r\n\r\nHello, world!";
        Self {
            content: response.to_vec(),
            read_pos: 0,
        }
    }
}

impl AsyncRead for StaticVirtualSocket {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        debug_assert!(self.read_pos <= self.content.len());

        let remaining = self.content.len() - self.read_pos;
        if remaining == 0 {
            return std::task::Poll::Ready(Ok(()));
        }

        let to_read = std::cmp::min(remaining, buf.remaining());
        buf.put_slice(&self.content[self.read_pos..self.read_pos + to_read]);
        self.read_pos += to_read;

        std::task::Poll::Ready(Ok(()))
    }
}

impl AsyncWrite for StaticVirtualSocket {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        // Discard all writes
        std::task::Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }
}

impl VirtualSocket for StaticVirtualSocket {
    fn set_socket_option(
        &self,
        _opt: pingora_core::protocols::l4::virt::VirtualSockOpt,
    ) -> std::io::Result<()> {
        Ok(())
    }
}

#[derive(Debug)]
struct VirtualConnector;

#[async_trait]
impl L4Connect for VirtualConnector {
    async fn connect(&self, _addr: &L4SocketAddr) -> pingora_error::Result<Stream> {
        Ok(Stream::from(VirtualSocketStream::new(Box::new(
            StaticVirtualSocket::new(),
        ))))
    }
}

struct VirtualProxy {
    connector: Arc<dyn L4Connect + Send + Sync>,
}

impl VirtualProxy {
    fn new() -> Self {
        Self {
            connector: Arc::new(VirtualConnector),
        }
    }
}

#[async_trait::async_trait]
impl ProxyHttp for VirtualProxy {
    type CTX = ();

    fn new_ctx(&self) -> Self::CTX {}

    // Route everything to example.org unless the Host header is "virtual.test",
    // in which case target the special virtual address 203.0.113.1:18080.
    async fn upstream_peer(
        &self,
        _session: &mut Session,
        _ctx: &mut Self::CTX,
    ) -> Result<Box<pingora_core::upstreams::peer::HttpPeer>> {
        let mut options = PeerOptions::new();
        options.custom_l4 = Some(self.connector.clone());

        Ok(Box::new(HttpPeer {
            _address: L4SocketAddr::Inet(SocketAddr::new(
                IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1)),
                80,
            )),
            scheme: pingora_core::upstreams::peer::Scheme::HTTP,
            sni: "example.org".to_string(),
            proxy: None,
            client_cert_key: None,
            group_key: 0,
            options,
        }))
    }
}

fn main() {
    // Minimal server config
    let conf = Arc::new(ServerConf::default());

    // Build the service and set the default L4 connector
    let mut svc: Service<HttpProxy<VirtualProxy>> =
        http_proxy_service_with_name(&conf, VirtualProxy::new(), "virtual-proxy");

    // Listen
    let addr = "127.0.0.1:6196";
    svc.add_tcp(addr);

    let mut server = Server::new(None).unwrap();
    server.add_service(svc);
    let run = RunArgs::default();

    eprintln!("Listening on {addr}, try: curl http://{addr}/");
    server.run(run);
}
