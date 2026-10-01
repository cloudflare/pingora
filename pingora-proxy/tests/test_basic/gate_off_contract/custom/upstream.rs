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

//! A custom upstream for one request: a custom [`Connector`] whose session answers with a
//! cacheable 12-byte response whose body and custom messages the test produces, and which
//! records the custom messages and request body the proxy writes to it.

use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

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
use tokio::sync::{mpsc, oneshot};

use super::downstream::{CustomOutput, RecordingCustomWriter};

/// The test's ends of a custom upstream's response.
pub struct CustomUpstream {
    /// The request header the proxy wrote to the session.
    pub requested: oneshot::Receiver<RequestHeader>,
    /// With [`CustomUpstream::failing_upload`], sending or dropping it releases the response
    /// header.
    pub header: Option<oneshot::Sender<()>>,
    /// The response body; dropping it ends the body.
    pub body: mpsc::Sender<Bytes>,
    /// Custom messages from the upstream; dropping it ends them.
    pub custom: mpsc::Sender<Result<Bytes>>,
    /// The custom messages the proxy wrote to the session, and how often it finished them.
    pub custom_output: Arc<Mutex<CustomOutput>>,
    /// With [`CustomUpstream::failing_upload`], the bytes of the first failed request body write.
    pub failed_request_body: Option<oneshot::Receiver<Bytes>>,
    /// Resolves once the proxy has handed the session back to the connector.
    pub released: oneshot::Receiver<()>,
}

impl CustomUpstream {
    /// A connector for a `200` response with a body of 12 bytes that may be cached for a minute,
    /// which accepts only an empty request body, and the test's ends of that response.
    pub fn new() -> (OneSessionConnector, Self) {
        Self::build(false)
    }

    /// The same, except that the session holds the response header until the test releases it
    /// through [`Self::header`], and fails every request body write, handing the first one's
    /// bytes to [`Self::failed_request_body`].
    pub fn failing_upload() -> (OneSessionConnector, Self) {
        Self::build(true)
    }

    fn build(failing_upload: bool) -> (OneSessionConnector, Self) {
        let (requested_tx, requested) = oneshot::channel();
        let (header, header_rx) = failing_upload.then(oneshot::channel).unzip();
        let (body, body_rx) = mpsc::channel(4);
        let (custom, custom_rx) = mpsc::channel(CUSTOM_MESSAGE_QUEUE_SIZE);
        let custom_output = Arc::new(Mutex::new(CustomOutput::default()));
        let (failed_request_body_tx, failed_request_body) =
            failing_upload.then(oneshot::channel).unzip();
        let (released_tx, released) = oneshot::channel();
        let mut response = ResponseHeader::build(200, None).unwrap();
        response.insert_header("content-length", 12).unwrap();
        response
            .insert_header("cache-control", "max-age=60")
            .unwrap();
        let connector = OneSessionConnector {
            session: Mutex::new(Some(ScriptedSession {
                requested: Some(requested_tx),
                header: header_rx,
                response,
                body: body_rx,
                custom: Some(ChannelReader(custom_rx)),
                custom_output: custom_output.clone(),
                request_body: Some(RequestBody {
                    fail: failing_upload,
                    failed: failed_request_body_tx,
                }),
                response_finished: false,
            })),
            released: Mutex::new(Some(released_tx)),
        };
        (
            connector,
            Self {
                requested,
                header,
                body,
                custom,
                custom_output,
                failed_request_body,
                released,
            },
        )
    }
}

/// Hands out its one session; any later connection fails.
pub struct OneSessionConnector {
    session: Mutex<Option<ScriptedSession>>,
    released: Mutex<Option<oneshot::Sender<()>>>,
}

#[async_trait]
impl Connector for OneSessionConnector {
    type Session = ScriptedSession;

    async fn get_http_session<P: Peer + Send + Sync + 'static>(
        &self,
        _peer: &P,
    ) -> Result<(Connection<Self::Session>, bool)> {
        match self.session.lock().unwrap().take() {
            Some(session) => Ok((Connection::Session(session), false)),
            None => Error::e_explain(
                ErrorType::ConnectError,
                "the custom upstream serves one request",
            ),
        }
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
        if let Some(released) = self.released.lock().unwrap().take() {
            let _ = released.send(());
        }
    }
}

pub struct ScriptedSession {
    requested: Option<oneshot::Sender<RequestHeader>>,
    header: Option<oneshot::Receiver<()>>,
    response: ResponseHeader,
    body: mpsc::Receiver<Bytes>,
    custom: Option<ChannelReader>,
    custom_output: Arc<Mutex<CustomOutput>>,
    request_body: Option<RequestBody>,
    response_finished: bool,
}

#[async_trait]
impl Session for ScriptedSession {
    async fn write_request_header(
        &mut self,
        request: Box<RequestHeader>,
        _end: bool,
    ) -> Result<()> {
        if let Some(requested) = self.requested.take() {
            let _ = requested.send(*request);
        }
        Ok(())
    }

    async fn write_request_body(&mut self, _body: Bytes, _end: bool) -> Result<()> {
        unreachable!("the request body writer was taken")
    }

    async fn finish_request_body(&mut self) -> Result<()> {
        Ok(())
    }

    fn set_read_timeout(&mut self, _timeout: Option<Duration>) {}

    fn set_write_timeout(&mut self, _timeout: Option<Duration>) {}

    async fn read_response_header(&mut self) -> Result<()> {
        if let Some(header) = self.header.take() {
            let _ = header.await;
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
        Ok(None)
    }

    fn fd(&self) -> UniqueIDType {
        -1
    }

    async fn check_response_end_or_error(&mut self, _headers: bool) -> Result<bool> {
        Ok(self.response_finished)
    }

    fn take_request_body_writer(&mut self) -> Option<Box<dyn BodyWrite>> {
        self.request_body
            .take()
            .map(|request_body| Box::new(request_body) as _)
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
        Some(Box::new(RecordingCustomWriter(self.custom_output.clone())))
    }
}

struct ChannelReader(mpsc::Receiver<Result<Bytes>>);

impl Stream for ChannelReader {
    type Item = Result<Bytes>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.0.poll_recv(cx)
    }
}

/// Accepts an empty request body, or with `fail` fails every write.
struct RequestBody {
    fail: bool,
    /// Takes the bytes of the first failed write.
    failed: Option<oneshot::Sender<Bytes>>,
}

#[async_trait]
impl BodyWrite for RequestBody {
    async fn write_all_buf(&mut self, body: &mut Bytes) -> Result<()> {
        if self.fail {
            if let Some(failed) = self.failed.take() {
                let _ = failed.send(body.clone());
            }
            return Error::e_explain(ErrorType::WriteError, "the custom upstream fails uploads");
        }
        assert!(body.is_empty(), "unexpected request body");
        Ok(())
    }

    async fn finish(&mut self) -> Result<()> {
        Ok(())
    }

    async fn cleanup(&mut self) -> Result<()> {
        Ok(())
    }

    fn upgrade_body_writer(&mut self) {
        unreachable!("the request does not upgrade")
    }
}
