use async_trait::async_trait;
use bytes::Bytes;
use serde::{Deserialize, Serialize};

use pingora_core::server::Server;
use pingora_core::upstreams::peer::HttpPeer;
use pingora_core::Result;
use pingora_http::{RequestHeader, ResponseHeader};
use pingora_proxy::{ProxyHttp, Session};

const HOST: &str = "ip.jsontest.com";

#[derive(Serialize, Deserialize)]
pub struct Resp {
    ip: String,
}

pub struct Json2Yaml {
    addr: std::net::SocketAddr,
}

pub struct MyCtx {
    buffer: Vec<u8>,
}

#[async_trait]
impl ProxyHttp for Json2Yaml {
    type CTX = MyCtx;

    fn new_ctx(&self) -> Self::CTX {
        MyCtx { buffer: vec![] }
    }

    async fn upstream_peer(
        &self,
        _session: &mut Session,
        _ctx: &mut Self::CTX,
    ) -> Result<Box<HttpPeer>> {
        let peer = Box::new(HttpPeer::new(self.addr, false, HOST.to_owned()));
        Ok(peer)
    }

    async fn upstream_request_filter(
        &self,
        _session: &mut Session,
        upstream_request: &mut RequestHeader,
        _ctx: &mut Self::CTX,
    ) -> Result<()> {
        upstream_request
            .insert_header("Host", HOST)
            .unwrap();
        Ok(())
    }

    async fn response_filter(
        &self,
        _session: &mut Session,
        upstream_response: &mut ResponseHeader,
        _ctx: &mut Self::CTX,
    ) -> Result<()> {
        // Remove Content-Length because the size of the new body is unknown
        upstream_response.remove_header("Content-Length");
        upstream_response
            .insert_header("Transfer-Encoding", "chunked")
            .unwrap();
        Ok(())
    }

    fn response_body_filter(
        &self,
        _session: &mut Session,
        body: &mut Option<Bytes>,
        end_of_stream: bool,
        ctx: &mut Self::CTX,
    ) -> Result<Option<std::time::Duration>>
    where
        Self::CTX: Send + Sync,
    {
        // Buffer the data
        if let Some(b) = body.take() {
            ctx.buffer.extend_from_slice(&b);
        }

        if end_of_stream {
            // This is the last chunk; we can process the data now
            let json_body: Resp = serde_json::from_slice(&ctx.buffer).unwrap();
            let yaml_body = serde_yaml::to_string(&json_body).unwrap();
            *body = Some(Bytes::copy_from_slice(yaml_body.as_bytes()));
        }

        Ok(None)
    }
}

fn main() {
    env_logger::init();

    let mut my_server = Server::new(None).unwrap();
    my_server.bootstrap();

    let mut my_proxy = pingora_proxy::http_proxy_service(
        &my_server.configuration,
        Json2Yaml {
            addr: "142.251.2.121:80".parse().unwrap(),
        },
    );

    my_proxy.add_tcp("127.0.0.1:6191");

    my_server.add_service(my_proxy);
    my_server.run_forever();
}
