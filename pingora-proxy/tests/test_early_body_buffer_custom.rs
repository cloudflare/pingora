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

//! Early-buffered request bodies forwarded to a custom upstream.
//!
//! The custom upstream records what the proxy writes, so these tests check the header's end
//! flag, the body bytes, and how often the body writer is finished, across retries.

#![cfg(all(unix, feature = "early_body_buffer"))]

use async_trait::async_trait;
use bytes::Bytes;
use futures::Stream;
use http::HeaderMap;
use pingora_core::connectors::http::custom::{Connection, Connector};
use pingora_core::listeners::ListenerConfig;
use pingora_core::protocols::http::custom::{client, BodyWrite, CustomMessageWrite};
use pingora_core::protocols::tls::{CustomALPN, ALPN};
use pingora_core::protocols::{l4::socket::SocketAddr as L4Addr, Digest, UniqueIDType};
use pingora_core::server::{configuration::ServerConf, Fds, ListenFds};
use pingora_core::services::Service;
use pingora_core::upstreams::peer::{HttpPeer, Peer};
use pingora_error::{Error, ErrorType, Result, RetryType};
use pingora_http::{RequestHeader, ResponseHeader};
use pingora_proxy::{http_proxy_service_with_name_custom_connector, ProxyHttp, Session};
use std::net::{SocketAddr, TcpListener};
use std::os::fd::IntoRawFd;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::watch;
use tokio::task::JoinHandle;

const TEST_TIMEOUT: Duration = Duration::from_secs(5);
const RESPONSE_BODY: &[u8] = b"ok";

/// What one custom upstream attempt received.
#[derive(Debug, Default)]
struct Attempt {
    header: Option<(RequestHeader, bool)>,
    body: Vec<u8>,
    finishes: usize,
}

type Attempts = Arc<Mutex<Vec<Attempt>>>;

struct RecordingBody {
    attempts: Attempts,
    index: usize,
}

#[async_trait]
impl BodyWrite for RecordingBody {
    async fn write_all_buf(&mut self, body: &mut Bytes) -> Result<()> {
        self.attempts.lock().unwrap()[self.index]
            .body
            .extend_from_slice(body);
        Ok(())
    }

    async fn finish(&mut self) -> Result<()> {
        self.attempts.lock().unwrap()[self.index].finishes += 1;
        Ok(())
    }

    async fn cleanup(&mut self) -> Result<()> {
        Ok(())
    }

    fn upgrade_body_writer(&mut self) {
        unreachable!("test request does not upgrade")
    }
}

struct RecordingSession {
    attempts: Attempts,
    index: usize,
    fail_response: bool,
    response: ResponseHeader,
    response_body: Option<Bytes>,
    response_finished: bool,
}

#[async_trait]
impl client::Session for RecordingSession {
    async fn write_request_header(&mut self, req: Box<RequestHeader>, end: bool) -> Result<()> {
        self.attempts.lock().unwrap()[self.index].header = Some((*req, end));
        Ok(())
    }

    async fn write_request_body(&mut self, _data: Bytes, _end: bool) -> Result<()> {
        unreachable!("request body writer was extracted")
    }

    async fn finish_request_body(&mut self) -> Result<()> {
        Ok(())
    }

    fn set_read_timeout(&mut self, _timeout: Option<Duration>) {}

    fn set_write_timeout(&mut self, _timeout: Option<Duration>) {}

    async fn read_response_header(&mut self) -> Result<()> {
        if self.fail_response {
            let mut error = Error::explain(ErrorType::ConnectionClosed, "injected upstream close");
            error.retry = RetryType::Decided(true);
            return Err(error.into_up());
        }
        Ok(())
    }

    async fn read_response_body(&mut self) -> Result<Option<Bytes>> {
        let body = self.response_body.take();
        self.response_finished = body.is_none();
        Ok(body)
    }

    fn response_finished(&self) -> bool {
        self.response_finished
    }

    async fn shutdown(&mut self, _code: u32, _ctx: &str) {}

    fn response_header(&self) -> Option<&ResponseHeader> {
        Some(&self.response)
    }

    fn was_upgraded(&self) -> bool {
        false
    }

    fn digest(&self) -> Option<&Digest> {
        None
    }

    fn digest_mut(&mut self) -> Option<&mut Digest> {
        None
    }

    fn server_addr(&self) -> Option<&L4Addr> {
        None
    }

    fn client_addr(&self) -> Option<&L4Addr> {
        None
    }

    async fn read_trailers(&mut self) -> Result<Option<HeaderMap>> {
        Ok(None)
    }

    fn fd(&self) -> UniqueIDType {
        -1
    }

    async fn check_response_end_or_error(&mut self, _headers: bool) -> Result<bool> {
        Ok(self.response_finished)
    }

    fn take_request_body_writer(&mut self) -> Option<Box<dyn BodyWrite>> {
        Some(Box::new(RecordingBody {
            attempts: Arc::clone(&self.attempts),
            index: self.index,
        }))
    }

    async fn finish_custom(&mut self) -> Result<()> {
        Ok(())
    }

    fn take_custom_message_reader(
        &mut self,
    ) -> Option<Box<dyn Stream<Item = Result<Bytes>> + Unpin + Send + Sync + 'static>> {
        Some(Box::new(futures::stream::empty()))
    }

    async fn drain_custom_messages(&mut self) -> Result<()> {
        Ok(())
    }

    fn take_custom_message_writer(&mut self) -> Option<Box<dyn CustomMessageWrite>> {
        Some(Box::new(()))
    }
}

/// Hands out one recording session per attempt; the first `failures` fail their response.
struct RecordingConnector {
    attempts: Attempts,
    failures: usize,
    created: AtomicUsize,
}

#[async_trait]
impl Connector for RecordingConnector {
    type Session = RecordingSession;

    async fn get_http_session<P: Peer + Send + Sync + 'static>(
        &self,
        _peer: &P,
    ) -> Result<(Connection<Self::Session>, bool)> {
        let index = self.created.fetch_add(1, Ordering::SeqCst);
        self.attempts.lock().unwrap().push(Attempt::default());
        let mut response = ResponseHeader::build(200, None).unwrap();
        response
            .insert_header(http::header::CONTENT_LENGTH, RESPONSE_BODY.len())
            .unwrap();
        let session = RecordingSession {
            attempts: Arc::clone(&self.attempts),
            index,
            fail_response: index < self.failures,
            response,
            response_body: Some(Bytes::from_static(RESPONSE_BODY)),
            response_finished: false,
        };
        Ok((Connection::Session(session), false))
    }

    async fn reused_http_session<P: Peer + Send + Sync + 'static>(
        &self,
        _peer: &P,
    ) -> Option<Self::Session> {
        None
    }

    async fn release_http_session<P: Peer + Send + Sync + 'static>(
        &self,
        _session: Self::Session,
        _peer: &P,
        _idle_timeout: Option<Duration>,
    ) {
    }
}

#[derive(Clone, Copy, Default)]
enum BodyChange {
    #[default]
    Keep,
    /// Discard every chunk in `early_request_body_filter` and clear the framing headers.
    DropInEarlyFilter,
    /// Replace the body with an explicit empty buffer in `request_filter`.
    ReplaceWithEmpty,
}

struct CustomBufferProxy {
    body_change: BodyChange,
    body_filter_calls: Arc<AtomicUsize>,
}

#[async_trait]
impl ProxyHttp for CustomBufferProxy {
    type CTX = ();

    fn new_ctx(&self) {}

    fn early_request_body_buffer_limit(&self, _session: &Session, _ctx: &()) -> Option<usize> {
        Some(1024)
    }

    async fn early_request_body_filter(
        &self,
        session: &mut Session,
        body: &mut Option<Bytes>,
        _end_of_stream: bool,
        _ctx: &mut (),
    ) -> Result<()> {
        if matches!(self.body_change, BodyChange::DropInEarlyFilter) {
            *body = None;
            let req = session.downstream_session.req_header_mut();
            req.remove_header(&http::header::CONTENT_LENGTH);
            req.remove_header(&http::header::TRANSFER_ENCODING);
        }
        Ok(())
    }

    async fn request_filter(&self, session: &mut Session, _ctx: &mut ()) -> Result<bool> {
        if matches!(self.body_change, BodyChange::ReplaceWithEmpty) {
            session.set_buffered_body(Some(Bytes::new()))?;
            session
                .req_header_mut()
                .insert_header(http::header::CONTENT_LENGTH, "0")?;
        }
        Ok(false)
    }

    async fn request_body_filter(
        &self,
        _session: &mut Session,
        body: &mut Option<Bytes>,
        _end_of_stream: bool,
        _ctx: &mut (),
    ) -> Result<()> {
        if body.is_some() {
            self.body_filter_calls.fetch_add(1, Ordering::SeqCst);
        }
        Ok(())
    }

    async fn upstream_peer(&self, _session: &mut Session, _ctx: &mut ()) -> Result<Box<HttpPeer>> {
        let mut peer = HttpPeer::new(("127.0.0.1", 1), false, String::new());
        peer.options.alpn = ALPN::Custom(CustomALPN::new(b"test/1".to_vec()));
        Ok(Box::new(peer))
    }
}

struct RunningProxy {
    address: SocketAddr,
    shutdown: watch::Sender<bool>,
    _task: JoinHandle<()>,
    attempts: Attempts,
    body_filter_calls: Arc<AtomicUsize>,
}

impl RunningProxy {
    fn start(body_change: BodyChange, failures: usize) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let attempts = Attempts::default();
        let body_filter_calls = Arc::new(AtomicUsize::new(0));
        let mut service = http_proxy_service_with_name_custom_connector(
            &Arc::new(ServerConf::default()),
            CustomBufferProxy {
                body_change,
                body_filter_calls: Arc::clone(&body_filter_calls),
            },
            "early body buffer custom test",
            RecordingConnector {
                attempts: Arc::clone(&attempts),
                failures,
                created: AtomicUsize::new(0),
            },
        );
        service.add_listener(ListenerConfig::tcp(address.to_string()));
        let fds: ListenFds = Arc::new(Fds::new().into());
        fds.lock().add(address.to_string(), listener.into_raw_fd());
        let (shutdown, watch) = watch::channel(false);
        let task = tokio::spawn(async move { service.start_service(Some(fds), watch, 1).await });
        Self {
            address,
            shutdown,
            _task: task,
            attempts,
            body_filter_calls,
        }
    }

    /// Sends `head`, then `body` once the proxy answers `100 Continue` if `expect_continue`.
    async fn request(&self, head: &str, body: &[u8], expect_continue: bool) -> String {
        tokio::time::timeout(TEST_TIMEOUT, async {
            let mut client = TcpStream::connect(self.address).await.unwrap();
            client.write_all(head.as_bytes()).await.unwrap();
            if expect_continue {
                let interim = read_head(&mut client).await;
                assert!(interim.starts_with("HTTP/1.1 100"), "got {interim:?}");
            }
            client.write_all(body).await.unwrap();
            let response = read_head(&mut client).await;
            let mut body = vec![0; RESPONSE_BODY.len()];
            client.read_exact(&mut body).await.unwrap();
            assert_eq!(body, RESPONSE_BODY);
            response
        })
        .await
        .expect("proxy did not answer")
    }

    fn attempts(&self) -> std::sync::MutexGuard<'_, Vec<Attempt>> {
        self.attempts.lock().unwrap()
    }
}

impl Drop for RunningProxy {
    fn drop(&mut self) {
        let _ = self.shutdown.send(true);
    }
}

async fn read_head(client: &mut TcpStream) -> String {
    let mut head = Vec::new();
    while !head.ends_with(b"\r\n\r\n") {
        head.push(client.read_u8().await.unwrap());
    }
    String::from_utf8(head).unwrap()
}

const POST_HELLO: &str =
    "POST / HTTP/1.1\r\nHost: example.com\r\nContent-Length: 5\r\nConnection: close\r\n\r\n";

fn assert_attempt_carried(attempt: &Attempt, body: &[u8]) {
    let (header, end) = attempt.header.as_ref().expect("request header was written");
    assert!(
        !end,
        "a non-empty body must not end the request on its header"
    );
    assert!(header.headers.get(http::header::EXPECT).is_none());
    assert_eq!(attempt.body, body);
    assert_eq!(attempt.finishes, 1);
}

#[tokio::test]
async fn buffered_body_is_forwarded_to_custom_upstream() {
    let proxy = RunningProxy::start(BodyChange::Keep, 0);

    let response = proxy.request(POST_HELLO, b"hello", false).await;

    assert!(response.starts_with("HTTP/1.1 200"), "got {response:?}");
    let attempts = proxy.attempts();
    assert_eq!(attempts.len(), 1);
    assert_attempt_carried(&attempts[0], b"hello");
}

#[tokio::test]
async fn buffered_body_is_replayed_to_each_custom_attempt() {
    let proxy = RunningProxy::start(BodyChange::Keep, 1);
    // The default retry policy only replays idempotent methods.
    let head = POST_HELLO.replacen("POST", "PUT", 1);

    let response = proxy.request(&head, b"hello", false).await;

    assert!(response.starts_with("HTTP/1.1 200"), "got {response:?}");
    let attempts = proxy.attempts();
    assert_eq!(attempts.len(), 2);
    for attempt in attempts.iter() {
        assert_attempt_carried(attempt, b"hello");
    }
    assert_eq!(proxy.body_filter_calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn expect_continue_is_answered_locally_and_not_sent_to_custom_upstream() {
    let proxy = RunningProxy::start(BodyChange::Keep, 0);
    let head = "POST / HTTP/1.1\r\nHost: example.com\r\nContent-Length: 5\r\n\
                Expect: 100-continue\r\nConnection: close\r\n\r\n";

    let response = proxy.request(head, b"hello", true).await;

    assert!(response.starts_with("HTTP/1.1 200"), "got {response:?}");
    assert_attempt_carried(&proxy.attempts()[0], b"hello");
}

async fn assert_empty_body_ends_on_header(body_change: BodyChange) {
    let proxy = RunningProxy::start(body_change, 0);

    let response = proxy.request(POST_HELLO, b"hello", false).await;

    assert!(response.starts_with("HTTP/1.1 200"), "got {response:?}");
    let attempts = proxy.attempts();
    let (_, end) = attempts[0]
        .header
        .as_ref()
        .expect("request header was written");
    assert!(
        end,
        "an empty buffered body must end the request on its header"
    );
    assert!(attempts[0].body.is_empty());
    assert_eq!(attempts[0].finishes, 0, "the request was already ended");
}

#[tokio::test]
async fn filtered_empty_body_ends_custom_request_on_header() {
    assert_empty_body_ends_on_header(BodyChange::DropInEarlyFilter).await;
}

#[tokio::test]
async fn explicit_empty_buffer_ends_custom_request_on_header() {
    assert_empty_body_ends_on_header(BodyChange::ReplaceWithEmpty).await;
}
