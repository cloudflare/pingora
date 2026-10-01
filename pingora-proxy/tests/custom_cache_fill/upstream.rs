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

use async_trait::async_trait;
use bytes::Bytes;
use futures::Stream;
use http::HeaderMap;
use pingora_core::connectors::http::custom::{Connection, Connector};
use pingora_core::protocols::http::custom::{
    client::Session, BodyWrite, CustomMessageWrite, CUSTOM_MESSAGE_QUEUE_SIZE,
};
use pingora_core::protocols::{l4::socket::SocketAddr, Digest, UniqueIDType};
use pingora_core::upstreams::peer::Peer;
use pingora_error::{Error, ErrorType, Result};
use pingora_http::{RequestHeader, ResponseHeader};
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot, Notify};

pub const FIRST_BODY: &[u8] = b"before abandonment\n";
pub const LAST_BODY: &[u8] = b"after abandonment\n";
pub const LATE_MESSAGE_COUNT: usize = CUSTOM_MESSAGE_QUEUE_SIZE + 1;

pub struct CustomUpstream {
    pub connector: StreamingConnector,
    pub producer: StreamingProducer,
    /// Abandon/shutdown calls the custom session recorded, shared with tests.
    pub abandon_calls: Arc<Mutex<Vec<String>>>,
    /// Request headers the proxy asked the custom session to write, shared with tests.
    pub request_headers_written: Arc<AtomicUsize>,
    /// Sessions the proxy released back to the connector, shared with tests.
    pub releases: Arc<AtomicUsize>,
    /// Fired once the session reads the terminal `None` of the response body.
    pub body_drained: Arc<Notify>,
}

impl CustomUpstream {
    pub fn new() -> Self {
        Self::build(None, false)
    }

    /// The upstream holds its response header until the producer is released.
    pub fn with_held_response_header() -> Self {
        let (release, held) = oneshot::channel();
        let mut upstream = Self::build(Some(held), false);
        upstream.producer.header = Some(release);
        upstream
    }

    /// Like [`Self::new`], but once the producer is dropped the response body fails with an
    /// upstream read error instead of ending.
    pub fn failing_when_producer_drops() -> Self {
        Self::build(None, true)
    }

    fn build(held_header: Option<oneshot::Receiver<()>>, fail_when_producer_drops: bool) -> Self {
        let abandon_calls = Arc::new(Mutex::new(Vec::new()));
        let request_headers_written = Arc::new(AtomicUsize::new(0));
        let releases = Arc::new(AtomicUsize::new(0));
        let body_drained = Arc::new(Notify::new());
        let (custom_tx, custom_rx) = mpsc::channel(CUSTOM_MESSAGE_QUEUE_SIZE);
        let (body_tx, body_rx) = mpsc::channel(1);
        let mut response = ResponseHeader::build(200, None).unwrap();
        response
            .insert_header("content-length", FIRST_BODY.len() + LAST_BODY.len())
            .unwrap();
        response
            .insert_header("cache-control", "max-age=60")
            .unwrap();
        Self {
            connector: StreamingConnector {
                session: Mutex::new(Some(StreamingSession {
                    held_header,
                    response,
                    body: body_rx,
                    custom: Some(BoundedCustomReader(custom_rx)),
                    response_finished: false,
                    fail_when_producer_drops,
                    abandon_calls: Arc::clone(&abandon_calls),
                    request_headers_written: Arc::clone(&request_headers_written),
                    body_drained: Arc::clone(&body_drained),
                })),
                releases: Arc::clone(&releases),
            },
            producer: StreamingProducer {
                header: None,
                custom: custom_tx,
                body: body_tx,
            },
            abandon_calls,
            request_headers_written,
            releases,
            body_drained,
        }
    }
}

pub struct StreamingProducer {
    header: Option<oneshot::Sender<()>>,
    custom: mpsc::Sender<Result<Bytes>>,
    body: mpsc::Sender<Bytes>,
}

/// How [`StreamingProducer::run_watching_reader`] ended.
#[derive(Debug, PartialEq, Eq)]
pub enum ProducerOutcome {
    /// The proxy dropped its response body reader before the test released the producer.
    BodyReaderDropped,
    /// The test released the producer, which then sent the rest of the response.
    Released {
        /// Whether the proxy had dropped its custom-message reader by then.
        reader_closed: bool,
    },
}

impl StreamingProducer {
    /// Send the first body chunk and custom message, wait for the release, then send the rest.
    ///
    /// Blocks on the release however the proxy behaves; tests that expect the proxy to give
    /// up early abort this future instead.
    pub async fn run(self, release: oneshot::Receiver<()>) -> bool {
        self.send_first().await;
        release
            .await
            .expect("test did not release the upstream response");
        self.send_rest().await
    }

    /// Like [`Self::run`], but also watch for the proxy dropping its response body reader
    /// while waiting for the release.
    pub async fn run_watching_reader(self, release: oneshot::Receiver<()>) -> ProducerOutcome {
        self.send_first().await;
        tokio::select! {
            _ = self.body.closed() => return ProducerOutcome::BodyReaderDropped,
            released = release => {
                released.expect("test did not release the upstream response");
            }
        }
        ProducerOutcome::Released {
            reader_closed: self.send_rest().await,
        }
    }

    /// Send the first body chunk and custom message, then stop once `fail` fires or is dropped.
    ///
    /// A [`CustomUpstream::failing_when_producer_drops`] session then fails its response body.
    pub async fn run_until_failure(self, fail: oneshot::Receiver<()>) {
        self.send_first().await;
        let _ = fail.await;
    }

    async fn send_first(&self) {
        self.body
            .send(Bytes::from_static(FIRST_BODY))
            .await
            .unwrap();
        self.custom
            .send(Ok(Bytes::from_static(b"first")))
            .await
            .unwrap();
    }

    /// Release a held response header, then send the rest. Returns whether the proxy had dropped
    /// its custom-message reader.
    async fn send_rest(self) -> bool {
        if let Some(header) = self.header {
            header.send(()).unwrap();
        }
        let mut reader_closed = false;
        // A dropped receiver discards custom messages. An idle, live receiver blocks HTTP EOF.
        for _ in 0..LATE_MESSAGE_COUNT {
            reader_closed |= self
                .custom
                .send(Ok(Bytes::from_static(b"late")))
                .await
                .is_err();
        }
        drop(self.custom);
        self.body.send(Bytes::from_static(LAST_BODY)).await.unwrap();
        reader_closed
    }
}

pub struct StreamingConnector {
    session: Mutex<Option<StreamingSession>>,
    releases: Arc<AtomicUsize>,
}

#[async_trait]
impl Connector for StreamingConnector {
    type Session = StreamingSession;

    async fn get_http_session<P: Peer + Send + Sync + 'static>(
        &self,
        _peer: &P,
    ) -> Result<(Connection<Self::Session>, bool)> {
        let session = self
            .session
            .lock()
            .unwrap()
            .take()
            .expect("unexpected retry");
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
        self.releases.fetch_add(1, Ordering::Relaxed);
    }
}

pub struct StreamingSession {
    held_header: Option<oneshot::Receiver<()>>,
    response: ResponseHeader,
    body: mpsc::Receiver<Bytes>,
    custom: Option<BoundedCustomReader>,
    response_finished: bool,
    fail_when_producer_drops: bool,
    abandon_calls: Arc<Mutex<Vec<String>>>,
    request_headers_written: Arc<AtomicUsize>,
    body_drained: Arc<Notify>,
}

#[async_trait]
impl Session for StreamingSession {
    async fn write_request_header(&mut self, request: Box<RequestHeader>, end: bool) -> Result<()> {
        self.request_headers_written.fetch_add(1, Ordering::Relaxed);
        assert_eq!(request.method, http::Method::GET);
        assert!(end);
        Ok(())
    }

    async fn write_request_body(&mut self, _body: Bytes, _end: bool) -> Result<()> {
        unreachable!("request body writer was extracted")
    }

    async fn finish_request_body(&mut self) -> Result<()> {
        Ok(())
    }

    fn set_read_timeout(&mut self, _timeout: Option<Duration>) {}

    fn set_write_timeout(&mut self, _timeout: Option<Duration>) {}

    async fn read_response_header(&mut self) -> Result<()> {
        if let Some(held) = self.held_header.take() {
            held.await
                .expect("producer did not release the response header");
        }
        Ok(())
    }

    async fn read_response_body(&mut self) -> Result<Option<Bytes>> {
        let body = self.body.recv().await;
        if body.is_none() && self.fail_when_producer_drops {
            return Error::e_explain(ErrorType::ReadError, "injected upstream failure")
                .map_err(|e| e.into_up());
        }
        self.response_finished = body.is_none();
        if self.response_finished {
            self.body_drained.notify_one();
        }
        Ok(body)
    }

    fn response_finished(&self) -> bool {
        self.response_finished
    }

    async fn shutdown(&mut self, code: u32, ctx: &str) {
        self.abandon_calls
            .lock()
            .unwrap()
            .push(format!("shutdown({code}, {ctx})"));
    }

    async fn abandon(&mut self, ctx: &str) {
        self.abandon_calls
            .lock()
            .unwrap()
            .push(format!("abandon({ctx})"));
    }

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

    fn server_addr(&self) -> Option<&SocketAddr> {
        None
    }

    fn client_addr(&self) -> Option<&SocketAddr> {
        None
    }

    async fn read_trailers(&mut self) -> Result<Option<HeaderMap>> {
        assert!(self.response_finished);
        Ok(None)
    }

    fn fd(&self) -> UniqueIDType {
        -1
    }

    async fn check_response_end_or_error(&mut self, _headers: bool) -> Result<bool> {
        Ok(self.response_finished)
    }

    fn take_request_body_writer(&mut self) -> Option<Box<dyn BodyWrite>> {
        Some(Box::new(EmptyRequestBody))
    }

    async fn finish_custom(&mut self) -> Result<()> {
        Ok(())
    }

    fn take_custom_message_reader(
        &mut self,
    ) -> Option<Box<dyn Stream<Item = Result<Bytes>> + Unpin + Send + Sync + 'static>> {
        self.custom.take().map(|reader| Box::new(reader) as _)
    }

    async fn drain_custom_messages(&mut self) -> Result<()> {
        Ok(())
    }

    fn take_custom_message_writer(&mut self) -> Option<Box<dyn CustomMessageWrite>> {
        Some(Box::new(()))
    }
}

struct BoundedCustomReader(mpsc::Receiver<Result<Bytes>>);

impl Stream for BoundedCustomReader {
    type Item = Result<Bytes>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.0.poll_recv(cx)
    }
}

struct EmptyRequestBody;

#[async_trait]
impl BodyWrite for EmptyRequestBody {
    async fn write_all_buf(&mut self, body: &mut Bytes) -> Result<()> {
        assert!(body.is_empty());
        Ok(())
    }

    async fn finish(&mut self) -> Result<()> {
        Ok(())
    }

    async fn cleanup(&mut self) -> Result<()> {
        Ok(())
    }

    fn upgrade_body_writer(&mut self) {
        unreachable!("test request does not upgrade")
    }
}
