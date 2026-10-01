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
use pingora_error::Result;
use pingora_http::{RequestHeader, ResponseHeader};
use std::pin::Pin;
use std::sync::Mutex;
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};

pub const FIRST_BODY: &[u8] = b"before abandonment\n";
pub const LAST_BODY: &[u8] = b"after abandonment\n";
pub const LATE_MESSAGE_COUNT: usize = CUSTOM_MESSAGE_QUEUE_SIZE + 1;

pub struct CustomUpstream {
    pub connector: StreamingConnector,
    pub producer: StreamingProducer,
}

impl CustomUpstream {
    pub fn new() -> Self {
        Self::build(None)
    }

    /// The upstream holds its response header until the producer is released.
    pub fn with_held_response_header() -> Self {
        let (release, held) = oneshot::channel();
        let mut upstream = Self::build(Some(held));
        upstream.producer.header = Some(release);
        upstream
    }

    fn build(held_header: Option<oneshot::Receiver<()>>) -> Self {
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
                })),
            },
            producer: StreamingProducer {
                header: None,
                custom: custom_tx,
                body: body_tx,
            },
        }
    }
}

pub struct StreamingProducer {
    header: Option<oneshot::Sender<()>>,
    custom: mpsc::Sender<Result<Bytes>>,
    body: mpsc::Sender<Bytes>,
}

impl StreamingProducer {
    pub async fn run(self, release: oneshot::Receiver<()>) -> bool {
        self.body
            .send(Bytes::from_static(FIRST_BODY))
            .await
            .unwrap();
        self.custom
            .send(Ok(Bytes::from_static(b"first")))
            .await
            .unwrap();
        release
            .await
            .expect("test did not release the upstream response");
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
    }
}

pub struct StreamingSession {
    held_header: Option<oneshot::Receiver<()>>,
    response: ResponseHeader,
    body: mpsc::Receiver<Bytes>,
    custom: Option<BoundedCustomReader>,
    response_finished: bool,
}

#[async_trait]
impl Session for StreamingSession {
    async fn write_request_header(&mut self, request: Box<RequestHeader>, end: bool) -> Result<()> {
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
        self.response_finished = body.is_none();
        Ok(body)
    }

    fn response_finished(&self) -> bool {
        self.response_finished
    }

    async fn shutdown(&mut self, _code: u32, _context: &str) {}

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
