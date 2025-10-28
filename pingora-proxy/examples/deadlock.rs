use async_trait::async_trait;
use pingora_core::server::Server;
use pingora_core::upstreams::peer::HttpPeer;
use pingora_core::Result;
use pingora_proxy::{http_proxy_service, ProxyHttp, Session};
use reqwest::Client;
use std::process::{Child, Command};

const PYTHON_SERVER: &str = r#"
from http.server import BaseHTTPRequestHandler, HTTPServer
class Handler(BaseHTTPRequestHandler):
    def do_POST(self):
        # If we comment this line, the server will deadlock
        # self.rfile.read(int(self.headers.get('Content-Length', 0)))
        data = b'X' * (10 * 1024 * 1024)
        self.send_response(200)
        self.send_header('Content-Length', str(len(data)))
        self.end_headers()
        self.wfile.write(data)
with HTTPServer(('', 8080), Handler) as server:
    server.serve_forever()
"#;

struct PythonServer(Child);

impl Drop for PythonServer {
    fn drop(&mut self) {
        let _ = self.0.kill();
    }
}

fn start_python_server() -> PythonServer {
    std::fs::write("/tmp/server.py", PYTHON_SERVER).unwrap();
    let child = Command::new("python3")
        .arg("/tmp/server.py")
        .spawn()
        .unwrap();
    PythonServer(child)
}

struct Svc;

#[async_trait]
impl ProxyHttp for Svc {
    type CTX = ();
    fn new_ctx(&self) -> Self::CTX {}
    async fn upstream_peer(&self, _: &mut Session, _: &mut Self::CTX) -> Result<Box<HttpPeer>> {
        Ok(Box::new(HttpPeer::new(
            ("127.0.0.1", 8080),
            false,
            "localhost".to_string(),
        )))
    }
}

#[tokio::main]
async fn main() {
    let mut _python_server = start_python_server();
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    let mut server = Server::new(None).unwrap();
    server.bootstrap();
    let mut svc = http_proxy_service(&server.configuration, Svc);
    svc.add_tcp("127.0.0.1:6080");
    server.add_service(svc);
    std::thread::spawn(|| server.run_forever());
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let client = Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build()
        .unwrap();
    let res = client
        .post("http://127.0.0.1:6080/")
        .body(vec![0u8; 10 * 1024 * 1024])
        .send()
        .await
        .unwrap();
    res.bytes().await.unwrap();
}
