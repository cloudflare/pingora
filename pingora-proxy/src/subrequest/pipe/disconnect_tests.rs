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

use super::*;
use crate::subrequest::Ctx as SubrequestCtx;
use crate::test_support::{next_session, run_ready_tasks};
use crate::{HttpProxy, ProxyHttp, Subrequest, SubrequestSpawner};
use async_trait::async_trait;
use bytes::BytesMut;
use pingora_core::modules::http::{HttpModule, HttpModuleBuilder, HttpModules, Module};
use pingora_core::protocols::http::v2::server::{
    self as h2_server, H2Accept, HttpSession as SessionV2,
};
use pingora_core::protocols::http::ServerSession;
use pingora_core::protocols::l4::stream::Stream;
use pingora_core::protocols::Digest;
use pingora_core::server::configuration::ServerConf;
use pingora_core::upstreams::peer::HttpPeer;
use pingora_error::{ErrorSource, ErrorType};
use pingora_http::ResponseHeader;
use std::{
    any::Any,
    future::Future,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::sync::{oneshot, Notify};
use tokio::task::JoinHandle;

type PipeResult = std::result::Result<PipeSubrequestState, PipeSubrequestError>;

/// A `GET` request header without a body.
const GET_REQUEST: &[u8] = b"GET / HTTP/1.1\r\nHost: test\r\n\r\n";
/// An Upgrade `GET` request header without a body.
const UPGRADE_REQUEST: &[u8] =
    b"GET / HTTP/1.1\r\nHost: test\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\r\n";
/// The subrequest's own request header for a subrequest that reads its whole input body.
const CHILD_CHUNKED_POST: &[u8] =
    b"POST / HTTP/1.1\r\nHost: test\r\nTransfer-Encoding: chunked\r\n\r\n";
/// The subrequest's own request header for a subrequest with a 6-byte body.
const CHILD_POST: &[u8] = b"POST / HTTP/1.1\r\nHost: test\r\nContent-Length: 6\r\n\r\n";
/// The next request the client sends on the connection.
const NEXT_REQUEST: &[u8] = b"GET /next HTTP/1.1\r\nHost: test\r\n\r\n";

/// Pipe `session` to a subrequest that `spawner` serves for `child_request`, and return the
/// pipe's outcome with `session`.
///
/// Subrequest headers use an H1 parser, so the subrequest gets `child_request`, an HTTP/1
/// request header, while the pipe still observes the original parent's transport.
async fn spawn_pipe(
    mut session: Session,
    spawner: SubrequestSpawner,
    child_request: &'static [u8],
    body_mode: BodyMode,
    input: InputBodyType,
) -> JoinHandle<(PipeResult, Session)> {
    let io = tokio_test::io::Builder::new().read(child_request).build();
    let mut child_source = Session::new_h1(Box::new(io));
    child_source.read_request().await.unwrap();
    let (subrequest, handle) = spawner.create_subrequest(
        child_source.as_downstream(),
        SubrequestCtx::builder().body_mode(body_mode).build(),
    );
    tokio::spawn(async move {
        let result = pipe_subrequest(
            &mut session,
            subrequest,
            handle,
            |task| Ok(Some(task)),
            input,
        )
        .await;
        (result, session)
    })
}

/// Read from `client` until what it received ends with `end`.
async fn read_until(client: &mut (impl AsyncRead + Unpin), end: &[u8]) -> Vec<u8> {
    let mut received = Vec::new();
    while !received.ends_with(end) {
        let mut buf = [0; 1024];
        let read = client.read(&mut buf).await.unwrap();
        assert_ne!(read, 0, "the response ended early: {received:?}");
        received.extend_from_slice(&buf[..read]);
    }
    received
}

/// An HTTP/1 proxy session for a `GET` that `client` sent.
async fn h1_get_session() -> (Session, tokio::io::DuplexStream) {
    let (mut client, server) = tokio::io::duplex(1024);
    client.write_all(GET_REQUEST).await.unwrap();
    let mut session = Session::new_h1(Box::new(server));
    session.downstream_session.read_request().await.unwrap();
    (session, client)
}

struct StalledSubrequest {
    started: Arc<Notify>,
    finished: Arc<Notify>,
}

#[async_trait]
impl Subrequest for StalledSubrequest {
    async fn process_subrequest(
        self: Arc<Self>,
        mut session: Box<ServerSession>,
        _ctx: Box<SubrequestCtx>,
    ) {
        session.read_request().await.unwrap();
        self.started.notify_one();
        session.read_body_or_idle(true).await.unwrap_err();
        self.finished.notify_one();
    }
}

/// Pipe `session` to a child that never responds, and return the pipe once the child has
/// started. The child notifies `finished` when it observes that the pipe abandoned it.
async fn spawn_stalled_pipe(
    session: Session,
    input: InputBodyType,
    finished: Arc<Notify>,
) -> JoinHandle<(PipeResult, Session)> {
    let started = Arc::new(Notify::new());
    let spawner = SubrequestSpawner::new(Arc::new(StalledSubrequest {
        started: started.clone(),
        finished,
    }));
    let pipe = spawn_pipe(session, spawner, GET_REQUEST, BodyMode::NoBody, input).await;
    tokio::time::timeout(Duration::from_secs(1), started.notified())
        .await
        .unwrap();
    pipe
}

/// Check that `pipe` fails with `expected_error` from the downstream instead of waiting for
/// its child's response, and that the child then observes abandonment through `finished`.
async fn assert_pipe_fails_downstream(
    pipe: JoinHandle<(PipeResult, Session)>,
    expected_error: ErrorType,
    finished: &Notify,
) {
    let (result, _session) = tokio::time::timeout(Duration::from_secs(1), pipe)
        .await
        .expect("the pipe waited for a child response after the client left")
        .unwrap();
    let mut failure = result.unwrap_err();
    assert!(!failure.from_subreq);
    assert_eq!(failure.error.esource, ErrorSource::Downstream);
    assert_eq!(failure.error.etype, expected_error);
    failure.state.pipe_rx.take();
    tokio::time::timeout(Duration::from_secs(1), finished.notified())
        .await
        .expect("the child did not observe abandonment");
}

async fn detects_disconnect_without_child_body_demand(input: InputBodyType) {
    let (session, client) = h1_get_session().await;
    assert_stalled_pipe_disconnects(session, input, ConnectionClosed, async { drop(client) }).await;
}

/// Opt `session` in, pipe it to a stalled child, and check that `disconnect` fails the pipe with
/// `expected_error` from the downstream instead of leaving it waiting for the child's response.
async fn assert_stalled_pipe_disconnects(
    mut session: Session,
    input: InputBodyType,
    expected_error: ErrorType,
    disconnect: impl Future<Output = ()>,
) {
    session.set_handles_downstream_abandonment(true);
    let finished = Arc::new(Notify::new());
    let pipe = spawn_stalled_pipe(session, input, finished.clone()).await;
    disconnect.await;
    assert_pipe_fails_downstream(pipe, expected_error, &finished).await;
}

#[tokio::test]
async fn preset_body_still_observes_downstream_disconnect() {
    detects_disconnect_without_child_body_demand(InputBodyType::Preset(InputBody::NoBody)).await;
}

#[tokio::test]
async fn bodyless_request_observes_disconnect_without_child_body_demand() {
    detects_disconnect_without_child_body_demand(InputBodyType::SaveBody(0)).await;
}

async fn detects_half_close_without_child_body_demand(input: InputBodyType) {
    let (session, mut client) = h1_get_session().await;
    // A half-close (write side shut down, read side still open) must be
    // treated exactly like a full disconnect by default, matching the
    // pre-existing HTTP/1 body-pump policy.
    assert_stalled_pipe_disconnects(session, input, ConnectionClosed, async {
        client.shutdown().await.unwrap();
    })
    .await;
}

#[tokio::test]
async fn half_closed_request_observes_disconnect_without_child_body_demand() {
    detects_half_close_without_child_body_demand(InputBodyType::SaveBody(0)).await;
}

/// How the HTTP/2 test client ends its request.
#[derive(Clone, Copy)]
enum H2RequestEnd {
    /// A `GET` with `END_STREAM` on its `HEADERS` frame.
    Headers,
    /// A `GET` whose `END_STREAM` arrives on a separate empty `DATA` frame.
    EmptyDataFrame,
    /// A `POST` whose body `body` is followed by trailers, which carry `END_STREAM`.
    Trailers,
    /// A `POST` that has sent `bo` of its body `body` and leaves the stream open.
    OpenBody,
}

/// The client side of an HTTP/2 connection with one request stream open.
struct H2Client {
    _requests: h2::client::SendRequest<Bytes>,
    _response: h2::client::ResponseFuture,
    stream: h2::SendStream<Bytes>,
    connection: JoinHandle<()>,
    server: JoinHandle<()>,
}

impl H2Client {
    fn reset_stream(&mut self) {
        self.stream.send_reset(h2::Reason::CANCEL);
    }

    fn close_connection(&self) {
        self.connection.abort();
    }
}

impl Drop for H2Client {
    fn drop(&mut self) {
        self.server.abort();
        self.connection.abort();
    }
}

/// Open an HTTP/2 connection, send a request that ends as `end`, and return the proxy session
/// for that request, running `modules` as its downstream modules.
async fn h2_session(end: H2RequestEnd, modules: &HttpModules) -> (Session, H2Client) {
    let (client_io, server_io) = tokio::io::duplex(65536);
    let (mut requests, connection) = h2::client::handshake(client_io).await.unwrap();
    let connection = tokio::spawn(async move {
        let _ = connection.await;
    });
    let mut server_connection = h2_server::handshake(Box::new(server_io), None)
        .await
        .unwrap();
    let method = match end {
        H2RequestEnd::Headers | H2RequestEnd::EmptyDataFrame => "GET",
        H2RequestEnd::Trailers | H2RequestEnd::OpenBody => "POST",
    };
    let request = http::Request::builder()
        .method(method)
        .uri("https://test/")
        .body(())
        .unwrap();
    let (response, mut stream) = requests
        .send_request(request, matches!(end, H2RequestEnd::Headers))
        .unwrap();
    match end {
        H2RequestEnd::Headers => {}
        H2RequestEnd::EmptyDataFrame => stream.send_data(Bytes::new(), true).unwrap(),
        H2RequestEnd::Trailers => {
            stream
                .send_data(Bytes::from_static(b"body"), false)
                .unwrap();
            let mut trailers = http::HeaderMap::new();
            trailers.insert("x-trailer", http::HeaderValue::from_static("1"));
            stream.send_trailers(trailers).unwrap();
        }
        H2RequestEnd::OpenBody => stream.send_data(Bytes::from_static(b"bo"), false).unwrap(),
    }
    let digest = Arc::new(Digest::default());
    let Some(H2Accept::Session(http)) =
        SessionV2::from_h2_conn(&mut server_connection, digest.clone())
            .await
            .unwrap()
    else {
        panic!("expected the H2 test request")
    };
    let server = tokio::spawn(async move {
        while let Ok(Some(_)) =
            SessionV2::from_h2_conn(&mut server_connection, digest.clone()).await
        {}
    });
    let session = Session::new(
        Box::new(ServerSession::new_http2(http)),
        modules,
        #[cfg(feature = "upstream_modules")]
        &HttpModules::new(),
        Arc::new(AtomicBool::new(false)),
    );
    (
        session,
        H2Client {
            _requests: requests,
            _response: response,
            stream,
            connection,
            server,
        },
    )
}

/// Pipe an HTTP/2 request that ends as `end` to a stalled child fed a preset empty body, then
/// let its client reset the stream or close the connection. An opted-in pipe fails with the
/// downstream error; without the opt-in the pipe keeps waiting for the child's response.
async fn assert_h2_client_leaving_a_stalled_pipe(
    handles_downstream_abandonment: bool,
    end: H2RequestEnd,
    close_connection: bool,
) {
    let (mut session, mut client) = h2_session(end, &HttpModules::new()).await;
    session.set_handles_downstream_abandonment(handles_downstream_abandonment);
    let finished = Arc::new(Notify::new());
    let pipe = spawn_stalled_pipe(
        session,
        InputBodyType::Preset(InputBody::NoBody),
        finished.clone(),
    )
    .await;
    if close_connection {
        client.close_connection();
    } else {
        client.reset_stream();
    }
    if !handles_downstream_abandonment {
        run_ready_tasks().await;
        assert!(
            !pipe.is_finished(),
            "without the opt-in, the pipe must keep waiting for its child as before"
        );
        pipe.abort();
        return;
    }
    assert_pipe_fails_downstream(pipe, ErrorType::H2Error, &finished).await;
    if !close_connection {
        assert!(
            !client.connection.is_finished(),
            "reset must keep the H2 connection alive"
        );
    }
}

#[tokio::test]
async fn h2_reset_is_observed_while_a_preset_subrequest_stalls() {
    assert_h2_client_leaving_a_stalled_pipe(true, H2RequestEnd::Headers, false).await;
}

#[tokio::test]
async fn h2_reset_is_ignored_without_opt_in_while_a_preset_subrequest_stalls() {
    assert_h2_client_leaving_a_stalled_pipe(false, H2RequestEnd::Headers, false).await;
}

#[tokio::test]
async fn h2_connection_close_is_observed_while_a_preset_subrequest_stalls() {
    assert_h2_client_leaving_a_stalled_pipe(true, H2RequestEnd::Headers, true).await;
}

#[tokio::test]
async fn h2_connection_close_is_ignored_without_opt_in_while_a_preset_subrequest_stalls() {
    assert_h2_client_leaving_a_stalled_pipe(false, H2RequestEnd::Headers, true).await;
}

// h2 queues the empty `DATA` frame as body data, so the request body is not complete until
// someone reads it, and a pipe fed a preset body never does.
#[tokio::test]
async fn h2_request_ending_in_an_empty_data_frame_is_watched_while_a_preset_subrequest_stalls() {
    assert_h2_client_leaving_a_stalled_pipe(true, H2RequestEnd::EmptyDataFrame, false).await;
}

#[tokio::test]
async fn h2_request_ending_in_an_empty_data_frame_is_not_watched_without_opt_in() {
    assert_h2_client_leaving_a_stalled_pipe(false, H2RequestEnd::EmptyDataFrame, false).await;
}

/// A downstream module that counts the `request_body_filter` calls that end the request body.
struct EndOfBodyCounter(Arc<AtomicUsize>);

#[async_trait]
impl HttpModule for EndOfBodyCounter {
    async fn request_body_filter(
        &mut self,
        _body: &mut Option<Bytes>,
        end_of_stream: bool,
    ) -> Result<()> {
        if end_of_stream {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
        Ok(())
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

impl HttpModuleBuilder for EndOfBodyCounter {
    fn init(&self) -> Module {
        Box::new(EndOfBodyCounter(self.0.clone()))
    }
}

/// Reads its whole request body, reports it, then waits until the pipe abandons it.
struct BodyReadingSubrequest {
    body_read: Arc<Notify>,
    finished: Arc<Notify>,
}

#[async_trait]
impl Subrequest for BodyReadingSubrequest {
    async fn process_subrequest(
        self: Arc<Self>,
        mut session: Box<ServerSession>,
        _ctx: Box<SubrequestCtx>,
    ) {
        session.read_request().await.unwrap();
        let mut body = Vec::new();
        while let Some(chunk) = session.read_request_body().await.unwrap() {
            body.extend_from_slice(&chunk);
        }
        assert_eq!(body, b"body");
        self.body_read.notify_one();
        session.read_body_or_idle(true).await.unwrap_err();
        self.finished.notify_one();
    }
}

/// An HTTP/2 request's trailers stay unread once the pipe has forwarded the end of its body.
/// The pipe must end that body once, with a single end-of-body `request_body_filter` call, a
/// saved body that completes once and a single end of body sent to the subrequest, and must
/// then notice the client resetting its stream, as it does without the opt-in.
async fn assert_h2_body_with_trailers_is_ended_once(handles_downstream_abandonment: bool) {
    let end_of_body_filters = Arc::new(AtomicUsize::new(0));
    let mut modules = HttpModules::new();
    modules.add_module(Box::new(EndOfBodyCounter(end_of_body_filters.clone())));
    let (mut session, mut client) = h2_session(H2RequestEnd::Trailers, &modules).await;
    session.set_handles_downstream_abandonment(handles_downstream_abandonment);
    let body_read = Arc::new(Notify::new());
    let finished = Arc::new(Notify::new());
    let spawner = SubrequestSpawner::new(Arc::new(BodyReadingSubrequest {
        body_read: body_read.clone(),
        finished: finished.clone(),
    }));
    let pipe = spawn_pipe(
        session,
        spawner,
        CHILD_CHUNKED_POST,
        BodyMode::ExpectBody,
        InputBodyType::SaveBody(64),
    )
    .await;
    tokio::time::timeout(Duration::from_secs(1), body_read.notified())
        .await
        .expect("the subrequest did not receive the end of its request body");
    // Let the pipe go on past the end of the body it forwarded.
    run_ready_tasks().await;
    assert_eq!(
        end_of_body_filters.load(Ordering::Relaxed),
        1,
        "the pipe ended the forwarded request body more than once"
    );
    client.reset_stream();
    let (result, _session) = tokio::time::timeout(Duration::from_secs(1), pipe)
        .await
        .expect("the pipe did not notice the client resetting its stream")
        .unwrap();
    let mut failure = result.unwrap_err();
    assert!(!failure.from_subreq);
    assert_eq!(failure.error.esource, ErrorSource::Downstream);
    assert_eq!(failure.error.etype, ErrorType::H2Error);
    let saved = failure.state.saved_body.take().unwrap();
    assert!(saved.is_body_complete());
    assert_eq!(saved.body, [Bytes::from_static(b"body")]);
    assert_eq!(end_of_body_filters.load(Ordering::Relaxed), 1);
    failure.state.pipe_rx.take();
    tokio::time::timeout(Duration::from_secs(1), finished.notified())
        .await
        .expect("the child did not observe abandonment");
}

#[tokio::test]
async fn h2_body_with_trailers_is_ended_once_before_the_pipe_watches_its_client() {
    assert_h2_body_with_trailers_is_ended_once(true).await;
}

#[tokio::test]
async fn h2_body_with_trailers_is_ended_once_without_opt_in() {
    assert_h2_body_with_trailers_is_ended_once(false).await;
}

/// Asks for its request body only once `read_body` is notified, then sends the whole body.
struct LateBodySubrequest {
    started: Arc<Notify>,
    read_body: Arc<Notify>,
    body: Mutex<Option<oneshot::Sender<Vec<u8>>>>,
}

#[async_trait]
impl Subrequest for LateBodySubrequest {
    async fn process_subrequest(
        self: Arc<Self>,
        mut session: Box<ServerSession>,
        _ctx: Box<SubrequestCtx>,
    ) {
        session.read_request().await.unwrap();
        self.started.notify_one();
        self.read_body.notified().await;
        let mut body = Vec::new();
        while let Some(chunk) = session.read_request_body().await.unwrap() {
            body.extend_from_slice(&chunk);
        }
        let sender = self.body.lock().unwrap().take().unwrap();
        let _ = sender.send(body);
    }
}

/// Until its subrequest asks for the body, an opted-in pipe watches an HTTP/2 client whose
/// request body is still open. The watch only waits for a reset, so switching from it to the
/// forwarding read loses nothing: the subrequest gets the body sent before and during the watch,
/// with a single end of body.
#[tokio::test]
async fn h2_body_sent_while_the_pipe_watches_reaches_a_subrequest_that_asks_for_it_later() {
    let end_of_body_filters = Arc::new(AtomicUsize::new(0));
    let mut modules = HttpModules::new();
    modules.add_module(Box::new(EndOfBodyCounter(end_of_body_filters.clone())));
    let (mut session, mut client) = h2_session(H2RequestEnd::OpenBody, &modules).await;
    session.set_handles_downstream_abandonment(true);
    let started = Arc::new(Notify::new());
    let read_body = Arc::new(Notify::new());
    let (body_tx, body_rx) = oneshot::channel();
    let spawner = SubrequestSpawner::new(Arc::new(LateBodySubrequest {
        started: started.clone(),
        read_body: read_body.clone(),
        body: Mutex::new(Some(body_tx)),
    }));
    let pipe = spawn_pipe(
        session,
        spawner,
        CHILD_CHUNKED_POST,
        BodyMode::ExpectBody,
        InputBodyType::SaveBody(64),
    )
    .await;
    tokio::time::timeout(Duration::from_secs(1), started.notified())
        .await
        .expect("the subrequest did not start");
    // The pipe now waits for the subrequest's response and watches the client.
    run_ready_tasks().await;
    client
        .stream
        .send_data(Bytes::from_static(b"dy"), true)
        .unwrap();
    run_ready_tasks().await;
    assert!(!pipe.is_finished(), "the watch ended the pipe");

    read_body.notify_one();
    let body = tokio::time::timeout(Duration::from_secs(1), body_rx)
        .await
        .expect("the subrequest did not receive the end of its request body")
        .unwrap();
    assert_eq!(body, b"body");
    assert_eq!(end_of_body_filters.load(Ordering::Relaxed), 1);
    pipe.abort();
}

/// An HTTP/1 client sends its next request early, after the pipe forwarded its whole request
/// body to a subrequest that asked for it and still owes its response. Without the opt-in the
/// pipe reads the client there, so an opted-in pipe must read it the same way. Returns the error
/// the pipe had failed with by then, if it had, with the session it failed on.
async fn early_next_request_after_a_forwarded_body(
    handles_downstream_abandonment: bool,
) -> Option<(ErrorSource, ErrorType, Session)> {
    let (mut client, server) = tokio::io::duplex(1024);
    client
        .write_all(b"POST / HTTP/1.1\r\nHost: test\r\nContent-Length: 4\r\n\r\nbody")
        .await
        .unwrap();
    let mut session = Session::new_h1(Box::new(server));
    session.downstream_session.read_request().await.unwrap();
    session.set_handles_downstream_abandonment(handles_downstream_abandonment);
    let body_read = Arc::new(Notify::new());
    let spawner = SubrequestSpawner::new(Arc::new(BodyReadingSubrequest {
        body_read: body_read.clone(),
        finished: Arc::new(Notify::new()),
    }));
    let pipe = spawn_pipe(
        session,
        spawner,
        CHILD_CHUNKED_POST,
        BodyMode::ExpectBody,
        InputBodyType::SaveBody(64),
    )
    .await;
    tokio::time::timeout(Duration::from_secs(1), body_read.notified())
        .await
        .expect("the subrequest did not receive the end of its request body");
    client.write_all(NEXT_REQUEST).await.unwrap();
    run_ready_tasks().await;
    if !pipe.is_finished() {
        pipe.abort();
        return None;
    }
    let (result, session) = pipe.await.unwrap();
    let failure = result.unwrap_err();
    Some((
        failure.error.esource.clone(),
        failure.error.etype.clone(),
        session,
    ))
}

#[tokio::test]
async fn early_next_request_after_a_forwarded_body_is_read_as_without_the_opt_in() {
    let error = |ended: Option<(ErrorSource, ErrorType, Session)>| {
        ended.map(|(source, etype, _session)| (source, etype))
    };
    let without_opt_in = error(early_next_request_after_a_forwarded_body(false).await);
    assert_eq!(
        without_opt_in,
        Some((ErrorSource::Downstream, ErrorType::ConnectError))
    );
    assert_eq!(
        error(early_next_request_after_a_forwarded_body(true).await),
        without_opt_in
    );
}

/// The forwarding read that failed on the early next request consumed its first byte, so the
/// pipe must leave the connection unreusable, with or without the opt-in. Otherwise a caller
/// that finishes the session hands the connection to a next session, which reads `ET /next` as
/// the next request.
async fn assert_forwarding_read_failure_leaves_the_connection_unreusable(
    handles_downstream_abandonment: bool,
) {
    let (_, _, session) = early_next_request_after_a_forwarded_body(handles_downstream_abandonment)
        .await
        .expect("the pipe did not fail on the early next request");
    assert!(
        session.downstream_session.finish().await.unwrap().is_none(),
        "a pipe whose forwarding read failed left the connection reusable"
    );
}

#[tokio::test]
async fn forwarding_read_failure_leaves_the_connection_unreusable() {
    assert_forwarding_read_failure_leaves_the_connection_unreusable(true).await;
}

#[tokio::test]
async fn forwarding_read_failure_leaves_the_connection_unreusable_without_opt_in() {
    assert_forwarding_read_failure_leaves_the_connection_unreusable(false).await;
}

/// Reads the first chunk of its request body and reports it, leaves the rest of the body in its
/// channel until released, then reads it and reports that. Once allowed to respond, it writes a
/// response header and holds the response.
struct ReleasedBodyReadingSubrequest {
    first_chunk_read: Arc<Notify>,
    release: Arc<Notify>,
    drained: Arc<Notify>,
    respond: Arc<Notify>,
}

#[async_trait]
impl Subrequest for ReleasedBodyReadingSubrequest {
    async fn process_subrequest(
        self: Arc<Self>,
        mut session: Box<ServerSession>,
        _ctx: Box<SubrequestCtx>,
    ) {
        session.read_request().await.unwrap();
        let mut body = session.read_request_body().await.unwrap().unwrap().to_vec();
        self.first_chunk_read.notify_one();
        self.release.notified().await;
        while let Some(chunk) = session.read_request_body().await.unwrap() {
            body.extend_from_slice(&chunk);
        }
        assert_eq!(body, b"abcde");
        self.drained.notify_one();
        self.respond.notified().await;
        let mut response = ResponseHeader::build(200, Some(1)).unwrap();
        response.insert_header("Content-Length", "1").unwrap();
        session
            .write_response_header(Box::new(response))
            .await
            .unwrap();
        std::future::pending::<()>().await;
    }
}

/// As [`early_next_request_after_a_forwarded_body`], but the next request arrives while the
/// subrequest's 4-task channel is full with the end of the body it asked for. Without the opt-in
/// the pipe reads the client again only once the subrequest drains that channel and responds.
/// Returns how the pipe had ended after that, if it had.
async fn early_next_request_while_the_body_channel_is_full(
    handles_downstream_abandonment: bool,
) -> Option<(ErrorSource, ErrorType)> {
    let (mut client, server) = tokio::io::duplex(1024);
    client
        .write_all(b"POST / HTTP/1.1\r\nHost: test\r\nContent-Length: 5\r\n\r\n")
        .await
        .unwrap();
    let mut session = Session::new_h1(Box::new(server));
    session.downstream_session.read_request().await.unwrap();
    session.set_handles_downstream_abandonment(handles_downstream_abandonment);
    let first_chunk_read = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    // Respond as soon as the body is drained.
    let respond = Arc::new(Notify::new());
    respond.notify_one();
    let spawner = SubrequestSpawner::new(Arc::new(ReleasedBodyReadingSubrequest {
        first_chunk_read: first_chunk_read.clone(),
        release: release.clone(),
        drained: Arc::new(Notify::new()),
        respond,
    }));
    let pipe = spawn_pipe(
        session,
        spawner,
        CHILD_CHUNKED_POST,
        BodyMode::ExpectBody,
        InputBodyType::SaveBody(64),
    )
    .await;
    // One byte per read, so each byte is one body task. The subrequest takes the first; the
    // other four, the last of which ends the body, fill its channel.
    client.write_all(b"a").await.unwrap();
    tokio::time::timeout(Duration::from_secs(1), first_chunk_read.notified())
        .await
        .expect("the subrequest did not receive the first chunk of its request body");
    for byte in b"bcde" {
        client.write_all(&[*byte]).await.unwrap();
        run_ready_tasks().await;
    }
    client.write_all(NEXT_REQUEST).await.unwrap();
    run_ready_tasks().await;
    release.notify_one();
    run_ready_tasks().await;
    if !pipe.is_finished() {
        pipe.abort();
        return None;
    }
    let (result, _session) = pipe.await.unwrap();
    let failure = result.unwrap_err();
    Some((failure.error.esource.clone(), failure.error.etype.clone()))
}

#[tokio::test]
async fn early_next_request_while_the_body_channel_is_full_is_read_as_without_the_opt_in() {
    let without_opt_in = early_next_request_while_the_body_channel_is_full(false).await;
    assert_eq!(
        without_opt_in,
        Some((ErrorSource::Downstream, ErrorType::ConnectError))
    );
    assert_eq!(
        early_next_request_while_the_body_channel_is_full(true).await,
        without_opt_in
    );
}

/// A downstream module that reports each request body chunk the pipe forwards.
struct BodyChunkReporter(Arc<Notify>);

#[async_trait]
impl HttpModule for BodyChunkReporter {
    async fn request_body_filter(
        &mut self,
        _body: &mut Option<Bytes>,
        _end_of_stream: bool,
    ) -> Result<()> {
        self.0.notify_one();
        Ok(())
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

impl HttpModuleBuilder for BodyChunkReporter {
    fn init(&self) -> Module {
        Box::new(BodyChunkReporter(self.0.clone()))
    }
}

/// How an HTTP/1 client leaves its TCP connection.
#[derive(Clone, Copy)]
enum H1ClientLeaves {
    /// It closes the connection with a FIN.
    Close,
    /// It resets the connection.
    Reset,
}

/// Wait up to a second for `notify`, failing with `what` otherwise.
async fn notified_or(notify: &Notify, what: &str) {
    tokio::time::timeout(Duration::from_secs(1), notify.notified())
        .await
        .expect(what);
}

/// An HTTP/1 client's request body fills the subrequest's 4-task channel, the last task ending
/// that body, so the pipe has no body left to forward. The subrequest then drains the channel
/// and stalls without responding, and the client leaves as `leaves`. An opted-in pipe must fail
/// with the downstream error and leave the connection unreusable. Without the opt-in the pipe
/// keeps waiting until the subrequest responds.
async fn assert_client_leaving_after_its_body_filled_the_channel(
    handles_downstream_abandonment: bool,
    leaves: H1ClientLeaves,
) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut client = tokio::net::TcpStream::connect(listener.local_addr().unwrap())
        .await
        .unwrap();
    let (server, _) = listener.accept().await.unwrap();
    client
        .write_all(b"POST / HTTP/1.1\r\nHost: test\r\nContent-Length: 5\r\n\r\n")
        .await
        .unwrap();
    let chunk_forwarded = Arc::new(Notify::new());
    let mut modules = HttpModules::new();
    modules.add_module(Box::new(BodyChunkReporter(chunk_forwarded.clone())));
    let mut session = Session::new_h1_with_modules(Box::new(Stream::from(server)), &modules);
    session.downstream_session.read_request().await.unwrap();
    session.set_handles_downstream_abandonment(handles_downstream_abandonment);
    let first_chunk_read = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let drained = Arc::new(Notify::new());
    let respond = Arc::new(Notify::new());
    let spawner = SubrequestSpawner::new(Arc::new(ReleasedBodyReadingSubrequest {
        first_chunk_read: first_chunk_read.clone(),
        release: release.clone(),
        drained: drained.clone(),
        respond: respond.clone(),
    }));
    let pipe = spawn_pipe(
        session,
        spawner,
        CHILD_CHUNKED_POST,
        BodyMode::ExpectBody,
        InputBodyType::SaveBody(64),
    )
    .await;
    // Send the next byte only once the pipe forwarded the last, so each byte is one body task.
    // The subrequest takes the first; the other four, the last of which ends the body, fill its
    // channel.
    for byte in b"abcde" {
        client.write_all(&[*byte]).await.unwrap();
        notified_or(&chunk_forwarded, "the pipe did not forward a body chunk").await;
        if *byte == b'a' {
            notified_or(&first_chunk_read, "the subrequest did not read a chunk").await;
        }
    }
    release.notify_one();
    notified_or(&drained, "the subrequest did not drain its request body").await;
    // Let the pipe go on past the drained channel.
    run_ready_tasks().await;
    match leaves {
        H1ClientLeaves::Close => drop(client),
        H1ClientLeaves::Reset => {
            client.set_zero_linger().unwrap();
            drop(client);
        }
    }
    if !handles_downstream_abandonment {
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            !pipe.is_finished(),
            "without the opt-in, the pipe must keep waiting for its child as before"
        );
        respond.notify_one();
    }
    let (result, session) = tokio::time::timeout(Duration::from_secs(1), pipe)
        .await
        .expect("the pipe waited for a child response after the client left")
        .unwrap();
    let failure = result.unwrap_err();
    assert!(!failure.from_subreq);
    assert_eq!(failure.error.esource, ErrorSource::Downstream);
    if !handles_downstream_abandonment {
        // Which read or write notices the client once the child responds is a race.
        return;
    }
    let expected_error = match leaves {
        H1ClientLeaves::Close => ErrorType::ConnectionClosed,
        H1ClientLeaves::Reset => ErrorType::ReadError,
    };
    assert_eq!(failure.error.etype, expected_error);
    assert!(
        !session
            .downstream_session
            .as_http1()
            .unwrap()
            .will_keepalive(),
        "a pipe that failed on its client's idle read left the connection reusable"
    );
}

#[tokio::test]
async fn client_closing_after_its_body_filled_the_channel_fails_the_pipe() {
    assert_client_leaving_after_its_body_filled_the_channel(true, H1ClientLeaves::Close).await;
}

#[tokio::test]
async fn client_closing_after_its_body_filled_the_channel_is_ignored_without_opt_in() {
    assert_client_leaving_after_its_body_filled_the_channel(false, H1ClientLeaves::Close).await;
}

#[tokio::test]
async fn client_resetting_after_its_body_filled_the_channel_fails_the_pipe() {
    assert_client_leaving_after_its_body_filled_the_channel(true, H1ClientLeaves::Reset).await;
}

#[tokio::test]
async fn client_resetting_after_its_body_filled_the_channel_is_ignored_without_opt_in() {
    assert_client_leaving_after_its_body_filled_the_channel(false, H1ClientLeaves::Reset).await;
}

/// How the H1 client of a stalled pipe leaves.
#[derive(Clone, Copy)]
enum H1ClientLeaving {
    /// Closes the connection.
    Close,
    /// Shuts down its write side and keeps reading.
    HalfClose,
}

/// Whether a pipe to a stalled child has ended once its H1 client left as `leaving` and the
/// ready tasks have run.
async fn stalled_pipe_ends_after_client_leaves(
    handles_downstream_abandonment: bool,
    input: InputBodyType,
    leaving: H1ClientLeaving,
) -> bool {
    let (mut session, mut client) = h1_get_session().await;
    session.set_handles_downstream_abandonment(handles_downstream_abandonment);
    let pipe = spawn_stalled_pipe(session, input, Arc::new(Notify::new())).await;
    match leaving {
        H1ClientLeaving::Close => drop(client),
        H1ClientLeaving::HalfClose => client.shutdown().await.unwrap(),
    }
    run_ready_tasks().await;
    let ended = pipe.is_finished();
    pipe.abort();
    ended
}

async fn assert_leaving_ends_stalled_pipe_only_when_opted_in(
    input: fn() -> InputBodyType,
    leaving: H1ClientLeaving,
) {
    assert!(
        stalled_pipe_ends_after_client_leaves(true, input(), leaving).await,
        "an opted-in pipe kept waiting for its child after the client left"
    );
    assert!(
        !stalled_pipe_ends_after_client_leaves(false, input(), leaving).await,
        "without the opt-in, the pipe must keep waiting for its child as before"
    );
}

#[tokio::test]
async fn bodyless_request_disconnect_is_ignored_without_opt_in() {
    assert_leaving_ends_stalled_pipe_only_when_opted_in(
        || InputBodyType::SaveBody(0),
        H1ClientLeaving::Close,
    )
    .await;
}

#[tokio::test]
async fn preset_body_disconnect_is_ignored_without_opt_in() {
    assert_leaving_ends_stalled_pipe_only_when_opted_in(
        || InputBodyType::Preset(InputBody::NoBody),
        H1ClientLeaving::Close,
    )
    .await;
}

#[tokio::test]
async fn half_closed_request_disconnect_is_ignored_without_opt_in() {
    assert_leaving_ends_stalled_pipe_only_when_opted_in(
        || InputBodyType::SaveBody(0),
        H1ClientLeaving::HalfClose,
    )
    .await;
}

/// Send the next request early, then close the connection.
async fn send_early_next_request_then_close(client: &mut tokio::io::DuplexStream) {
    client.write_all(NEXT_REQUEST).await.unwrap();
    client.shutdown().await.unwrap();
}

/// An opted-in client sends its next request early and then closes while the child stalls.
/// Pipelining is disabled, so the watch fails the pipe on the first byte of that request instead
/// of leaving it waiting for the child's response.
async fn assert_early_next_request_then_close_fails_stalled_pipe(input: InputBodyType) {
    let (session, mut client) = h1_get_session().await;
    assert_stalled_pipe_disconnects(
        session,
        input,
        ConnectError,
        send_early_next_request_then_close(&mut client),
    )
    .await;
}

/// Without the opt-in the pipe never reads its client, so it keeps waiting for the stalled child.
async fn assert_early_next_request_then_close_is_ignored_without_opt_in(input: InputBodyType) {
    let (session, mut client) = h1_get_session().await;
    let pipe = spawn_stalled_pipe(session, input, Arc::new(Notify::new())).await;
    send_early_next_request_then_close(&mut client).await;
    run_ready_tasks().await;
    assert!(
        !pipe.is_finished(),
        "without the opt-in, the pipe must keep waiting for its child as before"
    );
    pipe.abort();
}

#[tokio::test]
async fn early_next_request_then_close_fails_a_stalled_pipe() {
    assert_early_next_request_then_close_fails_stalled_pipe(InputBodyType::SaveBody(0)).await;
}

#[tokio::test]
async fn early_next_request_then_close_is_ignored_without_opt_in() {
    assert_early_next_request_then_close_is_ignored_without_opt_in(InputBodyType::SaveBody(0))
        .await;
}

#[tokio::test]
async fn early_next_request_then_close_fails_a_stalled_preset_pipe() {
    assert_early_next_request_then_close_fails_stalled_pipe(InputBodyType::Preset(
        InputBody::NoBody,
    ))
    .await;
}

#[tokio::test]
async fn early_next_request_then_close_is_ignored_by_a_preset_pipe_without_opt_in() {
    assert_early_next_request_then_close_is_ignored_without_opt_in(InputBodyType::Preset(
        InputBody::NoBody,
    ))
    .await;
}

/// Writes a 12-byte response's header and `hello `, then `world!` once released. It reports that
/// it started once it stalls: after `hello `, or with `stalls_before_responding`, before writing
/// anything.
struct ReleasedResponseSubrequest {
    started: Arc<Notify>,
    release: Arc<Notify>,
    stalls_before_responding: bool,
}

impl ReleasedResponseSubrequest {
    async fn stall(&self) {
        self.started.notify_one();
        self.release.notified().await;
    }
}

#[async_trait]
impl Subrequest for ReleasedResponseSubrequest {
    async fn process_subrequest(
        self: Arc<Self>,
        mut session: Box<ServerSession>,
        _ctx: Box<SubrequestCtx>,
    ) {
        session.read_request().await.unwrap();
        if self.stalls_before_responding {
            self.stall().await;
        }
        let mut response = ResponseHeader::build(200, Some(1)).unwrap();
        response.insert_header("Content-Length", "12").unwrap();
        session
            .write_response_header(Box::new(response))
            .await
            .unwrap();
        session
            .write_response_body(Bytes::from_static(b"hello "), false)
            .await
            .unwrap();
        if !self.stalls_before_responding {
            self.stall().await;
        }
        session
            .write_response_body(Bytes::from_static(b"world!"), true)
            .await
            .unwrap();
    }
}

/// How a pipe ended when its client sent the next request before the pipe was done.
#[derive(Debug, PartialEq)]
enum NextRequestOutcome {
    /// The pipe failed with this error from the downstream.
    Failed(ErrorType),
    /// The pipe succeeded, and the next session on the reused connection read the next request,
    /// starting with these bytes the pipe had already read.
    NextSession(Option<BytesMut>),
}

/// Wait for `pipe` and return the downstream error it failed with, or, once its child is done,
/// read the next request from the reused connection.
///
/// A failed pipe must leave its connection unreusable, since its idle read may have consumed
/// the start of the next request.
async fn pipe_outcome(pipe: JoinHandle<(PipeResult, Session)>) -> NextRequestOutcome {
    let (result, session) = tokio::time::timeout(Duration::from_secs(1), pipe)
        .await
        .expect("the pipe did not finish after its response was released")
        .unwrap();
    let state = match result {
        Ok(state) => state,
        Err(failure) => {
            assert!(!failure.from_subreq);
            assert_eq!(failure.error.esource, ErrorSource::Downstream);
            assert!(
                !session
                    .downstream_session
                    .as_http1()
                    .unwrap()
                    .will_keepalive(),
                "a pipe that failed on its client's idle read left the connection reusable"
            );
            return NextRequestOutcome::Failed(failure.error.etype.clone());
        }
    };
    state.join_handle.unwrap().await.unwrap();
    let (next, prefix) = next_session(session).await;
    assert_eq!(next.req_header().uri.path(), "/next");
    NextRequestOutcome::NextSession(prefix)
}

/// When a client sends its next request early, and on which request.
#[derive(Clone, Copy)]
enum EarlyRequest {
    /// On a `GET`, once the child has written its response header and `hello `.
    AfterResponseStarted,
    /// On an Upgrade `GET`, before the child has responded, as a client may send data before the
    /// `101` it expects. The child then refuses the upgrade with its `200` response.
    BeforeUpgradeResponse,
}

/// Pipe a request without a body to a child that stalls, and send the next request `when` it
/// stalls. If the pipe is still running, let the response finish; the client must then receive
/// all of it.
async fn early_next_request_during_a_stalled_pipe(
    when: EarlyRequest,
    handles_downstream_abandonment: bool,
    pipelining_enabled: bool,
    input: InputBodyType,
) -> NextRequestOutcome {
    let request = match when {
        EarlyRequest::AfterResponseStarted => GET_REQUEST,
        EarlyRequest::BeforeUpgradeResponse => UPGRADE_REQUEST,
    };
    let (mut client, server) = tokio::io::duplex(1024);
    client.write_all(request).await.unwrap();
    let mut session = Session::new_h1(Box::new(server));
    session
        .downstream_session
        .set_pipelining_enabled(pipelining_enabled);
    session.downstream_session.read_request().await.unwrap();
    session.set_handles_downstream_abandonment(handles_downstream_abandonment);
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let spawner = SubrequestSpawner::new(Arc::new(ReleasedResponseSubrequest {
        started: started.clone(),
        release: release.clone(),
        stalls_before_responding: matches!(when, EarlyRequest::BeforeUpgradeResponse),
    }));
    let pipe = spawn_pipe(session, spawner, request, BodyMode::NoBody, input).await;
    tokio::time::timeout(Duration::from_secs(1), started.notified())
        .await
        .unwrap();
    client.write_all(NEXT_REQUEST).await.unwrap();
    // Let a watching pipe read the next request before the response resumes.
    run_ready_tasks().await;
    if pipe.is_finished() {
        return pipe_outcome(pipe).await;
    }
    release.notify_one();
    let outcome = pipe_outcome(pipe).await;
    if matches!(outcome, NextRequestOutcome::NextSession(_)) {
        tokio::time::timeout(
            Duration::from_secs(1),
            read_until(&mut client, b"\r\n\r\nhello world!"),
        )
        .await
        .expect("the client did not receive the whole response");
    }
    outcome
}

// Pipelining is disabled, so the first byte of the next request that the watch reads is data
// sent after the end of the request, which fails the pipe as it fails the regular proxy's idle
// read.
#[tokio::test]
async fn early_next_request_during_a_stalled_pipe_fails_it_without_pipelining() {
    assert_eq!(
        early_next_request_during_a_stalled_pipe(
            EarlyRequest::AfterResponseStarted,
            true,
            false,
            InputBodyType::SaveBody(0),
        )
        .await,
        NextRequestOutcome::Failed(ErrorType::ConnectError)
    );
}

#[tokio::test]
async fn early_next_request_during_a_stalled_pipe_stays_unread_without_the_opt_in() {
    assert_eq!(
        early_next_request_during_a_stalled_pipe(
            EarlyRequest::AfterResponseStarted,
            false,
            false,
            InputBodyType::SaveBody(0),
        )
        .await,
        NextRequestOutcome::NextSession(None),
        "a pipe without the opt-in never reads its client"
    );
}

#[tokio::test]
async fn early_next_request_during_a_stalled_preset_pipe_fails_it_without_pipelining() {
    assert_eq!(
        early_next_request_during_a_stalled_pipe(
            EarlyRequest::AfterResponseStarted,
            true,
            false,
            InputBodyType::Preset(InputBody::NoBody),
        )
        .await,
        NextRequestOutcome::Failed(ErrorType::ConnectError)
    );
}

#[tokio::test]
async fn early_next_request_during_a_stalled_preset_pipe_stays_unread_without_the_opt_in() {
    assert_eq!(
        early_next_request_during_a_stalled_pipe(
            EarlyRequest::AfterResponseStarted,
            false,
            false,
            InputBodyType::Preset(InputBody::NoBody),
        )
        .await,
        NextRequestOutcome::NextSession(None)
    );
}

// With pipelining enabled, the watch keeps the byte it read for the next session, as the
// regular proxy's idle read does.
#[tokio::test]
async fn early_next_request_during_a_stalled_pipe_reaches_the_next_session_with_pipelining() {
    assert_eq!(
        early_next_request_during_a_stalled_pipe(
            EarlyRequest::AfterResponseStarted,
            true,
            true,
            InputBodyType::SaveBody(0),
        )
        .await,
        NextRequestOutcome::NextSession(Some(BytesMut::from(&b"G"[..])))
    );
}

#[tokio::test]
async fn early_next_request_during_a_stalled_pipe_with_pipelining_stays_unread_without_the_opt_in()
{
    assert_eq!(
        early_next_request_during_a_stalled_pipe(
            EarlyRequest::AfterResponseStarted,
            false,
            true,
            InputBodyType::SaveBody(0),
        )
        .await,
        NextRequestOutcome::NextSession(None)
    );
}

// The watch cannot tell data that an Upgrade request's client sends before the response from an
// early next request, so it follows the same policy: without pipelining that data fails the
// pipe.
#[tokio::test]
async fn early_data_on_an_upgrade_request_fails_a_stalled_pipe_without_pipelining() {
    assert_eq!(
        early_next_request_during_a_stalled_pipe(
            EarlyRequest::BeforeUpgradeResponse,
            true,
            false,
            InputBodyType::SaveBody(0),
        )
        .await,
        NextRequestOutcome::Failed(ErrorType::ConnectError)
    );
}

#[tokio::test]
async fn early_data_on_an_upgrade_request_stays_unread_without_the_opt_in() {
    assert_eq!(
        early_next_request_during_a_stalled_pipe(
            EarlyRequest::BeforeUpgradeResponse,
            false,
            false,
            InputBodyType::SaveBody(0),
        )
        .await,
        NextRequestOutcome::NextSession(None)
    );
}

// With pipelining enabled, the watch keeps that data for the next session once the child refuses
// the upgrade.
#[tokio::test]
async fn early_data_on_an_upgrade_request_reaches_the_next_session_with_pipelining() {
    assert_eq!(
        early_next_request_during_a_stalled_pipe(
            EarlyRequest::BeforeUpgradeResponse,
            true,
            true,
            InputBodyType::SaveBody(0),
        )
        .await,
        NextRequestOutcome::NextSession(Some(BytesMut::from(&b"G"[..])))
    );
}

#[tokio::test]
async fn early_data_on_an_upgrade_request_with_pipelining_stays_unread_without_the_opt_in() {
    assert_eq!(
        early_next_request_during_a_stalled_pipe(
            EarlyRequest::BeforeUpgradeResponse,
            false,
            true,
            InputBodyType::SaveBody(0),
        )
        .await,
        NextRequestOutcome::NextSession(None)
    );
}

/// Responds with an empty body without reading its request body, then keeps its session, and so
/// its request input, until released.
struct RespondsThenHoldsSubrequest {
    responded: Arc<Notify>,
    release: Arc<Notify>,
}

#[async_trait]
impl Subrequest for RespondsThenHoldsSubrequest {
    async fn process_subrequest(
        self: Arc<Self>,
        mut session: Box<ServerSession>,
        _ctx: Box<SubrequestCtx>,
    ) {
        session.read_request().await.unwrap();
        let mut response = ResponseHeader::build(200, Some(1)).unwrap();
        response.insert_header("Content-Length", "0").unwrap();
        session
            .write_response_header(Box::new(response))
            .await
            .unwrap();
        self.responded.notify_one();
        self.release.notified().await;
    }
}

/// A client closes once it has its whole response, while the pipe still waits for the child to
/// take the preset body it never asked for. The pipe must then end once the child is done, as it
/// does without the opt-in, instead of failing on a close that came after the response.
async fn assert_client_close_after_the_complete_response_does_not_fail_the_pipe(
    handles_downstream_abandonment: bool,
) {
    let (mut session, mut client) = h1_get_session().await;
    session.set_handles_downstream_abandonment(handles_downstream_abandonment);
    let responded = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let spawner = SubrequestSpawner::new(Arc::new(RespondsThenHoldsSubrequest {
        responded: responded.clone(),
        release: release.clone(),
    }));
    let pipe = spawn_pipe(
        session,
        spawner,
        CHILD_POST,
        BodyMode::ExpectBody,
        InputBodyType::Preset(InputBody::Bytes(vec![Bytes::from_static(b"preset")])),
    )
    .await;
    tokio::time::timeout(Duration::from_secs(1), responded.notified())
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(1), read_until(&mut client, b"\r\n\r\n"))
        .await
        .expect("the client did not receive the response");
    drop(client);
    // Let a watching pipe see the close before the child is done.
    run_ready_tasks().await;
    release.notify_one();
    let (result, _session) = tokio::time::timeout(Duration::from_secs(1), pipe)
        .await
        .expect("the pipe did not end once the child was done")
        .unwrap();
    result.unwrap_or_else(|failure| {
        panic!(
            "a close after the complete response must not fail the pipe: {}",
            failure.error
        )
    });
}

#[tokio::test]
async fn client_close_after_the_complete_response_does_not_fail_the_pipe() {
    assert_client_close_after_the_complete_response_does_not_fail_the_pipe(true).await;
}

#[tokio::test]
async fn client_close_after_the_complete_response_does_not_fail_the_pipe_without_opt_in() {
    assert_client_close_after_the_complete_response_does_not_fail_the_pipe(false).await;
}

/// Writes its whole 12-byte response without ending it, then ends it once released.
struct StalledEndSubrequest {
    responded: Arc<Notify>,
    release: Arc<Notify>,
}

#[async_trait]
impl Subrequest for StalledEndSubrequest {
    async fn process_subrequest(
        self: Arc<Self>,
        mut session: Box<ServerSession>,
        _ctx: Box<SubrequestCtx>,
    ) {
        session.read_request().await.unwrap();
        let mut response = ResponseHeader::build(200, Some(1)).unwrap();
        response.insert_header("Content-Length", "12").unwrap();
        session
            .write_response_header(Box::new(response))
            .await
            .unwrap();
        // A subrequest's body task never carries the end flag; its end is the `Done` task that
        // `finish` sends.
        session
            .write_response_body(Bytes::from_static(b"hello world!"), false)
            .await
            .unwrap();
        self.responded.notify_one();
        self.release.notified().await;
        session.finish().await.unwrap();
    }
}

/// A client reads its whole response and then sends its next request, while the child that
/// wrote that response has not ended it. Returns how the pipe ended once the child ends it. With
/// `preset_body` the child is fed a preset body it never reads, so the pipe runs on after the
/// response is complete; otherwise the pipe is done once it has written that response.
async fn next_request_after_a_complete_response_whose_end_stalls(
    handles_downstream_abandonment: bool,
    preset_body: bool,
) -> NextRequestOutcome {
    let (mut session, mut client) = h1_get_session().await;
    session.set_handles_downstream_abandonment(handles_downstream_abandonment);
    let responded = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let spawner = SubrequestSpawner::new(Arc::new(StalledEndSubrequest {
        responded: responded.clone(),
        release: release.clone(),
    }));
    let pipe = if preset_body {
        let input = InputBodyType::Preset(InputBody::Bytes(vec![Bytes::from_static(b"preset")]));
        spawn_pipe(session, spawner, CHILD_POST, BodyMode::ExpectBody, input).await
    } else {
        let input = InputBodyType::SaveBody(0);
        spawn_pipe(session, spawner, GET_REQUEST, BodyMode::NoBody, input).await
    };
    tokio::time::timeout(Duration::from_secs(1), responded.notified())
        .await
        .unwrap();
    tokio::time::timeout(
        Duration::from_secs(1),
        read_until(&mut client, b"\r\n\r\nhello world!"),
    )
    .await
    .expect("the client did not receive the whole response");
    client.write_all(NEXT_REQUEST).await.unwrap();
    // Let a pipe that still watched its client read the next request before the response ends.
    run_ready_tasks().await;
    release.notify_one();
    pipe_outcome(pipe).await
}

// The response is complete by its Content-Length before the child ends it. The watch stops with
// the complete response, so the next request stays unread until the next session, as without
// the opt-in.
#[tokio::test]
async fn next_request_after_a_complete_response_reaches_the_next_session() {
    assert_eq!(
        next_request_after_a_complete_response_whose_end_stalls(true, false).await,
        NextRequestOutcome::NextSession(None)
    );
}

#[tokio::test]
async fn next_request_after_a_complete_response_reaches_the_next_session_without_opt_in() {
    assert_eq!(
        next_request_after_a_complete_response_whose_end_stalls(false, false).await,
        NextRequestOutcome::NextSession(None)
    );
}

#[tokio::test]
async fn next_request_after_a_complete_preset_pipe_response_reaches_the_next_session() {
    assert_eq!(
        next_request_after_a_complete_response_whose_end_stalls(true, true).await,
        NextRequestOutcome::NextSession(None)
    );
}

#[tokio::test]
async fn next_request_after_a_complete_preset_pipe_response_reaches_the_next_session_without_opt_in(
) {
    assert_eq!(
        next_request_after_a_complete_response_whose_end_stalls(false, true).await,
        NextRequestOutcome::NextSession(None)
    );
}

struct PresetBodySubrequest;

#[async_trait]
impl Subrequest for PresetBodySubrequest {
    async fn process_subrequest(
        self: Arc<Self>,
        mut session: Box<ServerSession>,
        _ctx: Box<SubrequestCtx>,
    ) {
        session.read_request().await.unwrap();
        let mut body = Vec::new();
        while let Some(chunk) = session.read_request_body().await.unwrap() {
            body.extend_from_slice(&chunk);
        }
        assert_eq!(body, b"preset");
        let mut response = ResponseHeader::build(200, Some(1)).unwrap();
        response.insert_header("Content-Length", "0").unwrap();
        session
            .write_response_header(Box::new(response))
            .await
            .unwrap();
    }
}

/// A child fed a preset body reads it and responds, while the client's own request body stays
/// unread in the connection. The pipe must leave that body for the main session to read, opted
/// in as without the opt-in: the watch waits for the request body to be complete, which it never
/// is here.
async fn assert_preset_body_does_not_consume_unread_downstream_body(
    handles_downstream_abandonment: bool,
) {
    let (mut client, server) = tokio::io::duplex(1024);
    client
        .write_all(b"POST / HTTP/1.1\r\nHost: test\r\nContent-Length: 6\r\n\r\n")
        .await
        .unwrap();
    let mut session = Session::new_h1(Box::new(server));
    session.downstream_session.read_request().await.unwrap();
    // Send the body only once the header is read, so that it waits in the connection, where any
    // read of the client would take it, rather than in the session's buffer.
    client.write_all(b"client").await.unwrap();
    session.set_handles_downstream_abandonment(handles_downstream_abandonment);
    let spawner = SubrequestSpawner::new(Arc::new(PresetBodySubrequest));
    let ctx = SubrequestCtx::builder()
        .body_mode(BodyMode::ExpectBody)
        .build();
    let (subrequest, handle) = spawner.create_subrequest(session.as_downstream(), ctx);
    let state = tokio::time::timeout(
        Duration::from_secs(1),
        pipe_subrequest(
            &mut session,
            subrequest,
            handle,
            |task| Ok(Some(task)),
            InputBodyType::Preset(InputBody::Bytes(vec![Bytes::from_static(b"preset")])),
        ),
    )
    .await
    .unwrap()
    .unwrap_or_else(|failure| panic!("preset body pipe failed: {}", failure.error));
    state.join_handle.unwrap().await.unwrap();
    assert_eq!(session.body_bytes_read(), 0);
    assert_eq!(
        session.read_request_body().await.unwrap().unwrap(),
        "client"
    );
}

#[tokio::test]
async fn preset_body_does_not_consume_unread_downstream_body() {
    assert_preset_body_does_not_consume_unread_downstream_body(true).await;
}

#[tokio::test]
async fn preset_body_does_not_consume_unread_downstream_body_without_opt_in() {
    assert_preset_body_does_not_consume_unread_downstream_body(false).await;
}

/// Unlike the tests above, this pins the `abort_on_close` opt-out: a client half-closes while its
/// child stalls before responding, with `set_abort_on_close(false)`. The pipe must then deliver
/// the child's whole response, opted in as without the opt-in, instead of treating the half-close
/// as abandonment.
async fn assert_half_closed_client_receives_response_without_abort_on_close(
    handles_downstream_abandonment: bool,
) {
    let (mut session, mut client) = h1_get_session().await;
    session.downstream_session.set_abort_on_close(false);
    session.set_handles_downstream_abandonment(handles_downstream_abandonment);
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let spawner = SubrequestSpawner::new(Arc::new(ReleasedResponseSubrequest {
        started: started.clone(),
        release: release.clone(),
        stalls_before_responding: true,
    }));
    let pipe = spawn_pipe(
        session,
        spawner,
        GET_REQUEST,
        BodyMode::NoBody,
        InputBodyType::SaveBody(0),
    )
    .await;
    notified_or(&started, "the subrequest did not start").await;
    client.shutdown().await.unwrap();
    // Let a watching pipe read the half-close before the child responds.
    run_ready_tasks().await;
    release.notify_one();
    let (result, _session) = tokio::time::timeout(Duration::from_secs(1), pipe)
        .await
        .expect("with abort_on_close disabled, the pipe must still deliver the child's response")
        .unwrap();
    let state =
        result.unwrap_or_else(|failure| panic!("half-closed pipe failed: {}", failure.error));
    state.join_handle.unwrap().await.unwrap();
    tokio::time::timeout(
        Duration::from_secs(1),
        read_until(&mut client, b"\r\n\r\nhello world!"),
    )
    .await
    .expect("the half-closed client did not receive the whole response");
}

#[tokio::test]
async fn half_closed_downstream_still_receives_response_when_abort_on_close_is_disabled() {
    assert_half_closed_client_receives_response_without_abort_on_close(true).await;
}

#[tokio::test]
async fn half_closed_downstream_still_receives_response_without_abort_on_close_or_opt_in() {
    assert_half_closed_client_receives_response_without_abort_on_close(false).await;
}

/// Answers every subrequest in `request_filter`, which runs after the proxy recorded the
/// opt-in: it idle-waits on the subrequest's downstream and records when that wait ends.
struct IdleProbeProxy {
    handles_downstream_abandonment: bool,
    started: Arc<Notify>,
    idle_ended: Arc<AtomicBool>,
}

#[async_trait]
impl ProxyHttp for IdleProbeProxy {
    type CTX = ();

    fn new_ctx(&self) -> Self::CTX {}

    fn should_handle_downstream_abandonment(&self, _session: &Session, _ctx: &Self::CTX) -> bool {
        self.handles_downstream_abandonment
    }

    async fn request_filter(&self, session: &mut Session, _ctx: &mut Self::CTX) -> Result<bool> {
        self.started.notify_one();
        session
            .downstream_session
            .read_body_or_idle(true)
            .await
            .unwrap_err();
        self.idle_ended.store(true, Ordering::Release);
        Ok(true)
    }

    async fn upstream_peer(
        &self,
        _session: &mut Session,
        _ctx: &mut Self::CTX,
    ) -> Result<Box<HttpPeer>> {
        unreachable!("request_filter answers every subrequest")
    }
}

/// Pipe a `GET` whose parent has the given opt-in to a subrequest the proxy runs with its own
/// opt-in, and return the pipe once that subrequest idle-waits in `request_filter`, with the
/// parent's client and the flag set once that wait ends.
async fn spawn_pipe_to_proxied_subrequest(
    parent_handles_downstream_abandonment: bool,
    subrequest_handles_downstream_abandonment: bool,
) -> (
    JoinHandle<(PipeResult, Session)>,
    tokio::io::DuplexStream,
    Arc<AtomicBool>,
) {
    let (mut session, client) = h1_get_session().await;
    session.set_handles_downstream_abandonment(parent_handles_downstream_abandonment);
    let started = Arc::new(Notify::new());
    let idle_ended = Arc::new(AtomicBool::new(false));
    let proxy = HttpProxy::new(
        IdleProbeProxy {
            handles_downstream_abandonment: subrequest_handles_downstream_abandonment,
            started: started.clone(),
            idle_ended: idle_ended.clone(),
        },
        Arc::new(ServerConf::default()),
    );
    let pipe = spawn_pipe(
        session,
        SubrequestSpawner::new(Arc::new(proxy)),
        GET_REQUEST,
        BodyMode::NoBody,
        InputBodyType::SaveBody(0),
    )
    .await;
    tokio::time::timeout(Duration::from_secs(1), started.notified())
        .await
        .expect("the subrequest did not reach request_filter");
    (pipe, client, idle_ended)
}

/// Fail an opted-in pipe to a subrequest the proxy runs with the given opt-in, and report
/// whether that subrequest's idle wait had ended while the parent still held the subrequest's
/// response receiver, and after the parent dropped it.
async fn proxied_subrequest_idle_ends(handles_downstream_abandonment: bool) -> (bool, bool) {
    let (pipe, client, idle_ended) =
        spawn_pipe_to_proxied_subrequest(true, handles_downstream_abandonment).await;
    drop(client);
    // The failed pipe has dropped its request sender, but the error keeps the receiver.
    let (result, _session) = tokio::time::timeout(Duration::from_secs(1), pipe)
        .await
        .expect("the pipe waited for a child response after the client closed")
        .unwrap();
    let mut failure = result.unwrap_err();
    run_ready_tasks().await;
    let ended_while_received = idle_ended.load(Ordering::Acquire);
    failure.state.pipe_rx.take();
    run_ready_tasks().await;
    (ended_while_received, idle_ended.load(Ordering::Acquire))
}

#[tokio::test]
async fn opted_in_subrequest_waits_until_its_response_is_no_longer_received() {
    assert_eq!(proxied_subrequest_idle_ends(true).await, (false, true));
}

#[tokio::test]
async fn subrequest_without_opt_in_stops_waiting_when_its_request_input_closes() {
    assert_eq!(proxied_subrequest_idle_ends(false).await, (true, true));
}

/// A parent without the opt-in keeps waiting for its subrequest after its client closes,
/// whatever the subrequest's own opt-in, and that subrequest's idle wait ends only once the pipe
/// is gone.
async fn assert_parent_without_opt_in_ignores_its_client_closing(
    subrequest_handles_downstream_abandonment: bool,
) {
    let (pipe, client, idle_ended) =
        spawn_pipe_to_proxied_subrequest(false, subrequest_handles_downstream_abandonment).await;
    drop(client);
    run_ready_tasks().await;
    assert!(
        !pipe.is_finished(),
        "a parent without the opt-in must keep waiting for its subrequest as before"
    );
    assert!(!idle_ended.load(Ordering::Acquire));
    pipe.abort();
    let _ = pipe.await;
    run_ready_tasks().await;
    assert!(
        idle_ended.load(Ordering::Acquire),
        "the subrequest must stop waiting once the pipe is gone"
    );
}

#[tokio::test]
async fn parent_without_opt_in_ignores_its_client_closing_with_an_opted_in_subrequest() {
    assert_parent_without_opt_in_ignores_its_client_closing(true).await;
}

#[tokio::test]
async fn parent_without_opt_in_ignores_its_client_closing_with_a_subrequest_without_it() {
    assert_parent_without_opt_in_ignores_its_client_closing(false).await;
}
