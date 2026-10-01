// Copyright 2026 Cloudflare, Inc.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! An HTTP/2 client of the cache proxy that closes its connection during a fill.

use super::*;

/// An HTTP/2 connection to the h2c cache proxy, and the task driving it: aborting the task
/// closes the connection.
async fn h2c_connect() -> (h2::client::SendRequest<Bytes>, tokio::task::JoinHandle<()>) {
    let tcp = TcpStream::connect("127.0.0.1:6154").await.unwrap();
    let (client, connection) = client::handshake(tcp).await.unwrap();
    let connection = tokio::spawn(async move {
        let _ = connection.await;
    });
    (client, connection)
}

/// An HTTP/2 request to the cache proxy that records its lifecycle.
fn h2_request(method: &str, path: &str, port: u16) -> http::request::Builder {
    Request::builder()
        .method(method)
        .uri(format!("http://{HOST}{path}"))
        .header("x-port", port.to_string())
        .header("x-record-request-lifecycle", "true")
        .header("x-lock", "true")
}

/// A `HEAD` miss over HTTP/2 gets its header while the fill still reads the body, but its stream
/// only ends with the fill. A client that then closes its connection is not noticed: the fill
/// completes without an ignored downstream error.
#[tokio::test]
async fn h2_head_miss_connection_close_after_header_keeps_filling() {
    init();
    let path = "/gate_off/h2_head_miss_connection_close_after_header";
    let baseline = Baseline::new(path);
    let mut origin = ScriptedOrigin::start().await;
    let (mut client, connection) = h2c_connect().await;
    let (response, _stream) = client
        .send_request(
            h2_request("HEAD", path, origin.port).body(()).unwrap(),
            true,
        )
        .unwrap();
    origin.wait_for_request().await;
    origin.send(CHUNKED_HEADER).await;
    origin.send(HELLO_CHUNK).await;
    let response = tokio::time::timeout(TEST_TIMEOUT, response)
        .await
        .expect("the response header did not arrive")
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["x-cache-status"], "miss");
    assert!(!response.body().is_end_stream());
    drop(response);
    drop(client);
    connection.abort();
    let _ = connection.await;
    origin.send(WORLD_CHUNK_AND_END).await;
    let port = origin.port;
    assert_eq!(origin.finish().await, OriginEnd::Released);
    assert_eq!(
        baseline.wait_for_completions(1).await,
        Recorded {
            error: None,
            upstream_attempts: 1,
            ignored_downstream_errors: 0,
            disabled_cache: None,
        }
    );
    assert_cached(path, port, b"hello world!").await;
}
