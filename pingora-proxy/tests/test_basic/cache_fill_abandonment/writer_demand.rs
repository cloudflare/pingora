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

//! What downstream abandonment handling is for. A storage that serves one fill to coalesced
//! readers stops that fill once nobody wants it any more; the proxy must then stop reading the
//! origin, and no reader may take the stopped fill for a complete response.
//!
//! The writer's client leaves, the test application's abandonment hook releases the writer's
//! demand, and a reader streaming from the fill keeps it going. Once that reader leaves too, the
//! demand-counting test storage fails the fill's next write. Without the opt-in, which is what
//! rolling back looks like to a storage that still implements the release, nothing releases the
//! writer's demand and the fill completes.

use super::*;
use crate::utils::server_utils::{demand_fill_observations, DemandFillObservations};
use h2::{RecvStream, SendStream};

const CACHE_AUTHORITY: &str = "127.0.0.1:6154";

/// The fill's body, one chunk at a time: the origin sends the first with its header and each
/// of the others when the test asks for it.
const CHUNKS: [&[u8]; 4] = [b"ab", b"cd", b"ef", b"gh"];

/// What the demand-counting storage observed for the latest fill of `path`.
fn fill(path: &str) -> DemandFillObservations {
    demand_fill_observations(&format!("{CACHE_AUTHORITY}{path}"))
}

async fn wait_for_fill(
    path: &str,
    message: &str,
    condition: impl Fn(&DemandFillObservations) -> bool,
) {
    wait_until(&format!("{message} for {path}"), || condition(&fill(path))).await;
}

/// The origin's response head for a chunked, cacheable body.
const RESPONSE_HEAD: &[u8] =
    b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nCache-Control: max-age=30\r\n\r\n";

/// A `GET` for `path` on the demand-counting storage, filled from the origin on `upstream_port`.
fn fill_request(path: &str, upstream_port: u16) -> http::request::Builder {
    Request::builder()
        .method("GET")
        .uri(format!("http://{CACHE_AUTHORITY}{path}"))
        .header("x-port", upstream_port.to_string())
        .header("x-lock", "true")
        .header("x-cache-demand-counting", "true")
}

async fn write_chunk(socket: &mut TcpStream, chunk: &[u8]) -> std::io::Result<()> {
    socket
        .write_all(format!("{:x}\r\n", chunk.len()).as_bytes())
        .await?;
    socket.write_all(chunk).await?;
    socket.write_all(b"\r\n").await
}

enum OriginCommand {
    Chunk(&'static [u8]),
    Finish,
}

/// An HTTP/1 origin for one request. It sends a chunked response's head and [`CHUNKS`]`[0]` at
/// once, then a chunk per [`OriginCommand::Chunk`] and the last chunk on
/// [`OriginCommand::Finish`]. `closed` fires once the proxy closes the connection.
struct ChunkedOrigin {
    port: u16,
    commands: tokio::sync::mpsc::UnboundedSender<OriginCommand>,
    closed: tokio::sync::oneshot::Receiver<()>,
}

impl ChunkedOrigin {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (commands, mut commands_rx) = tokio::sync::mpsc::unbounded_channel();
        let (closed_tx, closed) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            read_h1_head(&mut socket).await;
            socket.write_all(RESPONSE_HEAD).await.unwrap();
            write_chunk(&mut socket, CHUNKS[0]).await.unwrap();
            let mut byte = [0u8; 1];
            loop {
                tokio::select! {
                    command = commands_rx.recv() => match command {
                        Some(OriginCommand::Chunk(chunk)) => {
                            if write_chunk(&mut socket, chunk).await.is_err() {
                                break;
                            }
                        }
                        Some(OriginCommand::Finish) => {
                            let _ = socket.write_all(b"0\r\n\r\n").await;
                        }
                        None => break,
                    },
                    // The request is complete, so a read only ends when the proxy closes.
                    read = socket.read(&mut byte) => {
                        if matches!(read, Ok(0) | Err(_)) {
                            break;
                        }
                    }
                }
            }
            let _ = closed_tx.send(());
        });
        Self {
            port,
            commands,
            closed,
        }
    }

    fn send(&self, command: OriginCommand) {
        self.commands.send(command).unwrap();
    }
}

async fn send_h2_request(
    client: &mut client::SendRequest<Bytes>,
    request: http::request::Builder,
) -> (http::response::Parts, RecvStream, SendStream<Bytes>) {
    let (response, stream) = client
        .send_request(request.body(()).unwrap(), true)
        .unwrap();
    let response = tokio::time::timeout(TEST_TIMEOUT, response)
        .await
        .expect("proxy did not send the response header")
        .unwrap();
    let (head, body) = response.into_parts();
    (head, body, stream)
}

/// Read at least `len` more body bytes into `received`.
async fn read_h2_body_bytes(body: &mut RecvStream, received: &mut Vec<u8>, len: usize) {
    let target = received.len() + len;
    while received.len() < target {
        received.extend_from_slice(&read_h2_body_chunk(body).await);
    }
}

/// Read the rest of a response body into `received`, returning how it ended.
async fn read_h2_body_to_end(
    body: &mut RecvStream,
    received: &mut Vec<u8>,
) -> Result<(), h2::Error> {
    loop {
        match tokio::time::timeout(TEST_TIMEOUT, body.data())
            .await
            .expect("the response neither ended nor failed")
        {
            Some(Ok(chunk)) => {
                body.flow_control().release_capacity(chunk.len()).unwrap();
                received.extend_from_slice(&chunk);
            }
            Some(Err(e)) => return Err(e),
            None => return Ok(()),
        }
    }
}

/// Reset `stream` and return once the proxy has read that reset. The probe request sent after
/// the reset on the same connection is queued behind it, so its response proves the proxy's
/// connection has processed the reset.
async fn reset_and_confirm(
    client: &mut client::SendRequest<Bytes>,
    stream: &mut SendStream<Bytes>,
    path: &str,
    probe_port: u16,
) {
    stream.send_reset(h2::Reason::CANCEL);
    let probe = Request::builder()
        .method("GET")
        .uri(format!("http://{CACHE_AUTHORITY}{path}/probe"))
        .header("x-port", probe_port.to_string())
        .header("x-bypass-cache", "true");
    let (head, _, _) = send_h2_request(client, probe).await;
    assert_eq!(head.status, StatusCode::OK);
}

async fn probe_origin() -> HttpOrigin {
    HttpOrigin::bind(|_request| async {
        Response::builder()
            .header(header::CONTENT_LENGTH, "5")
            .body(Bytes::from_static(b"probe"))
            .unwrap()
    })
    .await
    .unwrap()
}

/// The writer's client resets once the proxy has the first body bytes. Both runs ignore that
/// for the fill; only an opted-in request tells the application, whose hook releases the
/// writer's demand.
async fn writer_leaves(
    path: &str,
    writer_connection: tokio::task::JoinHandle<()>,
    warn_logs_before: usize,
    abandoned_before: usize,
    handles_abandonment: bool,
) {
    // Dropping the connection, not resetting the stream: a reset can leave an opted-out
    // writer's pending HTTP/2 write to its client unwoken (with the master code too), stalling
    // the fill; a closed connection fails every stream at once.
    writer_connection.abort();
    wait_until(
        &format!("the proxy did not report the writer's reset for {path}"),
        || downstream_cache_warn_log_calls_for(path) != warn_logs_before,
    )
    .await;
    assert_eq!(
        downstream_abandoned_during_cache_fill_calls(path),
        abandoned_before + usize::from(handles_abandonment)
    );
    let observed = fill(path);
    assert_eq!(observed.writer_claim, !handles_abandonment);
    assert_eq!(
        observed.writer_demand_releases,
        usize::from(handles_abandonment)
    );
}

/// The writer's client leaves once it has the first chunk, a coalesced reader keeps the fill
/// going, then leaves too, and a late reader attaches to the fill before its next chunk.
async fn assert_last_reader_leaves(path: &'static str, handles_abandonment: bool) {
    init();
    let mut origin = ChunkedOrigin::start().await;
    let probe = probe_origin().await;
    let completed_before = recorded_request_completions(path);
    let abandoned_before = downstream_abandoned_during_cache_fill_calls(path);
    let warn_logs_before = downstream_cache_warn_log_calls_for(path);

    // The writer.
    let (mut writer_client, writer_connection) = h2c_client_with_connection().await;
    let (head, mut writer_body, _writer_stream) = send_h2_request(
        &mut writer_client,
        opt_in(fill_request(path, origin.port), handles_abandonment)
            .header("x-record-request-lifecycle", "true"),
    )
    .await;
    assert_eq!(head.status, StatusCode::OK);
    assert_eq!(head.headers["x-cache-status"], "miss");
    assert_eq!(read_h2_body_chunk(&mut writer_body).await, CHUNKS[0]);

    // A coalesced reader streams from the fill.
    let mut reader_client = h2c_client(6154).await;
    let (head, mut reader_body, mut reader_stream) =
        send_h2_request(&mut reader_client, fill_request(path, origin.port)).await;
    assert_eq!(head.status, StatusCode::OK);
    assert_eq!(head.headers["x-cache-status"], "hit");
    assert_eq!(read_h2_body_chunk(&mut reader_body).await, CHUNKS[0]);
    assert_eq!(fill(path).reader_claims, 1);

    writer_leaves(
        path,
        writer_connection,
        warn_logs_before,
        abandoned_before,
        handles_abandonment,
    )
    .await;

    // The reader keeps the fill going.
    origin.send(OriginCommand::Chunk(CHUNKS[1]));
    wait_for_fill(path, "the fill did not take the second chunk", |fill| {
        fill.body_writes == 2
    })
    .await;
    assert_eq!(read_h2_body_chunk(&mut reader_body).await, CHUNKS[1]);

    // The reader's client leaves too. The proxy notices at its next write to that client, so
    // the next chunk still reaches the fill and only then does the reader drop its claim.
    reset_and_confirm(
        &mut reader_client,
        &mut reader_stream,
        path,
        probe.addr().port(),
    )
    .await;
    origin.send(OriginCommand::Chunk(CHUNKS[2]));
    wait_for_fill(
        path,
        "the leaving reader did not release its claim after the third chunk",
        |fill| fill.body_writes == 3 && fill.reader_claims == 0,
    )
    .await;
    assert_eq!(fill(path).stopped, handles_abandonment);

    // A late reader attaches to the fill before its next chunk.
    let mut late_client = h2c_client(6154).await;
    let (head, mut late_body, _late_stream) =
        send_h2_request(&mut late_client, fill_request(path, origin.port)).await;
    assert_eq!(head.status, StatusCode::OK);
    assert_eq!(head.headers["x-cache-status"], "hit");
    let mut late_received = Vec::new();
    read_h2_body_bytes(&mut late_body, &mut late_received, 6).await;
    assert_eq!(late_received, CHUNKS[..3].concat());
    assert_eq!(fill(path).readers_attached, 2);
    assert!(
        origin.closed.try_recv().is_err(),
        "the proxy closed the origin connection before the fill stopped"
    );

    origin.send(OriginCommand::Chunk(CHUNKS[3]));
    if handles_abandonment {
        // Nobody wants the fill: its next write fails, and that failure ends the request, so
        // the proxy stops reading the origin.
        wait_for_recorded_request_completion(path, completed_before).await;
        assert_eq!(
            recorded_request_error(path),
            Some(pingora_error::ErrorType::FileWriteError)
        );
        let observed = fill(path);
        assert_eq!(
            (
                observed.body_writes,
                observed.failed_writes,
                observed.finished
            ),
            (3, 1, false)
        );
        tokio::time::timeout(TEST_TIMEOUT, &mut origin.closed)
            .await
            .expect("the proxy kept reading the origin after the fill stopped")
            .unwrap();
        // The late reader fails instead of seeing the end of a truncated body.
        let end = read_h2_body_to_end(&mut late_body, &mut late_received).await;
        assert!(
            end.is_err(),
            "the late reader's truncated response looked complete: {late_received:?}"
        );
        assert_eq!(late_received, CHUNKS[..3].concat());
    } else {
        wait_for_fill(path, "the fill did not take the last chunk", |fill| {
            fill.body_writes == 4
        })
        .await;
        origin.send(OriginCommand::Finish);
        wait_for_recorded_request_completion(path, completed_before).await;
        assert_eq!(recorded_request_error(path), None);
        let observed = fill(path);
        assert!(observed.finished, "{observed:?}");
        assert_eq!(
            (
                observed.writer_claim,
                observed.stopped,
                observed.failed_writes
            ),
            (true, false, 0)
        );
        let end = read_h2_body_to_end(&mut late_body, &mut late_received).await;
        assert!(end.is_ok(), "{end:?}");
        assert_eq!(late_received, CHUNKS.concat());
    }

    // Nobody gets the stopped fill from the cache either.
    let recovery = HttpOrigin::bind(|_request| async {
        Response::builder()
            .header(header::CONTENT_LENGTH, "8")
            .header(header::CACHE_CONTROL, "max-age=30")
            .body(Bytes::from_static(b"recovery"))
            .unwrap()
    })
    .await
    .unwrap();
    let mut next_client = h2c_client(6154).await;
    let (head, mut next_body, _next_stream) =
        send_h2_request(&mut next_client, fill_request(path, recovery.addr().port())).await;
    assert_eq!(head.status, StatusCode::OK);
    let mut next_received = Vec::new();
    read_h2_body_to_end(&mut next_body, &mut next_received)
        .await
        .unwrap();
    if handles_abandonment {
        assert_eq!(head.headers["x-cache-status"], "miss");
        assert_cache_lock_was_free(&head.headers);
        assert_eq!(next_received, b"recovery");
    } else {
        assert_eq!(head.headers["x-cache-status"], "hit");
        assert_eq!(next_received, CHUNKS.concat());
    }
    recovery.shutdown().await;
    probe.shutdown().await;
}

#[tokio::test]
async fn test_h2_last_coalesced_reader_leaving_stops_abandoned_fill() {
    bounded(assert_last_reader_leaves(
        "/test_h2_last_coalesced_reader_leaving_stops_abandoned_fill",
        true,
    ))
    .await;
    // Without the opt-in nothing releases the writer's demand, so the fill outlives both
    // clients, the late reader gets the whole body, and the response is cached.
    bounded(assert_last_reader_leaves(
        "/test_h2_last_coalesced_reader_leaving_ungated",
        false,
    ))
    .await;
}

/// The streaming origin's chunked body: [`STREAM_CHUNK`]-byte chunks, [`STREAM_TOTAL`] bytes.
const STREAM_CHUNK: usize = 16 * 1024;
const STREAM_TOTAL: usize = 64 * 1024 * 1024;
/// Most body bytes the origin may still send once the fill has stopped: whatever sits in the
/// origin's small send buffer, the proxy's receive buffer and its task queue when it drops the
/// connection. A proxy that kept reading would take the rest of [`STREAM_TOTAL`].
const AFTER_STOP_LIMIT: usize = 8 * 1024 * 1024;

/// An HTTP/1 origin that streams a cacheable chunked body as fast as the proxy reads it, until
/// a write fails. The task returns how many body bytes it wrote.
async fn start_streaming_origin(
    go: tokio::sync::oneshot::Receiver<()>,
) -> (u16, tokio::task::JoinHandle<usize>) {
    let socket = tokio::net::TcpSocket::new_v4().unwrap();
    // A small, fixed send buffer, inherited by the accepted socket, keeps what the kernel holds
    // for the proxy small.
    socket.set_send_buffer_size(64 * 1024).unwrap();
    socket.bind("127.0.0.1:0".parse().unwrap()).unwrap();
    let listener = socket.listen(1).unwrap();
    let port = listener.local_addr().unwrap().port();
    let task = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        read_h1_head(&mut socket).await;
        socket.write_all(RESPONSE_HEAD).await.unwrap();
        let chunk = vec![b's'; STREAM_CHUNK];
        write_chunk(&mut socket, &chunk).await.unwrap();
        let mut written = STREAM_CHUNK;
        // From here on it streams without waiting for anything.
        go.await.unwrap();
        while written < STREAM_TOTAL {
            if write_chunk(&mut socket, &chunk).await.is_err() {
                return written;
            }
            written += STREAM_CHUNK;
        }
        let _ = socket.write_all(b"0\r\n\r\n").await;
        written
    });
    (port, task)
}

/// The writer's client and then the coalesced reader's leave an opted-in fill before the origin
/// starts streaming as fast as the proxy reads. Once the fill stops, the proxy must stop reading
/// the origin within [`AFTER_STOP_LIMIT`].
async fn assert_streaming_origin_stops(path: &'static str) {
    init();
    let (go, go_rx) = tokio::sync::oneshot::channel();
    let (origin_port, origin) = start_streaming_origin(go_rx).await;
    let probe = probe_origin().await;
    let completed_before = recorded_request_completions(path);
    let abandoned_before = downstream_abandoned_during_cache_fill_calls(path);
    let warn_logs_before = downstream_cache_warn_log_calls_for(path);

    let (mut writer_client, writer_connection) = h2c_client_with_connection().await;
    let (head, mut writer_body, _writer_stream) = send_h2_request(
        &mut writer_client,
        opt_in(fill_request(path, origin_port), true).header("x-record-request-lifecycle", "true"),
    )
    .await;
    assert_eq!(head.headers["x-cache-status"], "miss");
    read_h2_body_chunk(&mut writer_body).await;

    let mut reader_client = h2c_client(6154).await;
    let (head, mut reader_body, mut reader_stream) =
        send_h2_request(&mut reader_client, fill_request(path, origin_port)).await;
    assert_eq!(head.headers["x-cache-status"], "hit");
    let mut reader_received = Vec::new();
    read_h2_body_bytes(&mut reader_body, &mut reader_received, 1).await;
    assert_eq!(fill(path).reader_claims, 1);

    writer_leaves(
        path,
        writer_connection,
        warn_logs_before,
        abandoned_before,
        true,
    )
    .await;

    // The reader leaves before the origin starts streaming, so its claim goes at the proxy's
    // first write to it once the origin streams, and the fill stops at the write after that.
    reset_and_confirm(
        &mut reader_client,
        &mut reader_stream,
        path,
        probe.addr().port(),
    )
    .await;
    go.send(()).unwrap();
    wait_for_recorded_request_completion(path, completed_before).await;
    let origin_written = tokio::time::timeout(TEST_TIMEOUT, origin)
        .await
        .expect("the origin kept streaming")
        .unwrap();
    let observed = fill(path);
    assert_eq!(
        recorded_request_error(path),
        Some(pingora_error::ErrorType::FileWriteError)
    );
    assert_eq!(observed.failed_writes, 1, "{observed:?}");
    let taken_before_stop = observed.body_bytes + observed.failed_bytes;
    assert!(origin_written >= taken_before_stop);
    assert!(
        origin_written - taken_before_stop <= AFTER_STOP_LIMIT,
        "the origin sent {} bytes after the fill stopped",
        origin_written - taken_before_stop
    );
    probe.shutdown().await;
}

#[tokio::test]
async fn test_h2_streaming_origin_stops_within_limit_once_last_reader_leaves() {
    tokio::time::timeout(
        std::time::Duration::from_secs(60),
        assert_streaming_origin_stops(
            "/test_h2_streaming_origin_stops_within_limit_once_last_reader_leaves",
        ),
    )
    .await
    .expect("streaming origin scenario did not finish");
}

/// An h2c client of the cache proxy, with the task driving its connection.
async fn h2c_client_with_connection() -> (client::SendRequest<Bytes>, tokio::task::JoinHandle<()>) {
    let tcp = TcpStream::connect(("127.0.0.1", 6154)).await.unwrap();
    let (client, connection) = client::handshake(tcp).await.unwrap();
    let connection = tokio::spawn(async move {
        let _ = connection.await;
    });
    (client, connection)
}
