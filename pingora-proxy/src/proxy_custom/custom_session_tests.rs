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
use futures::Stream;
use http::HeaderMap;
use pingora_core::protocols::{l4::socket::SocketAddr, Digest};
use std::{
    collections::VecDeque,
    sync::{atomic::AtomicBool, Arc, Mutex},
    time::Duration,
};
use tokio::sync::Notify;

struct IdleSession {
    request: RequestHeader,
    response: ResponseHeader,
    body: VecDeque<Option<Bytes>>,
    body_done: bool,
    idle_polled: Arc<Notify>,
    idle_result: Option<oneshot::Receiver<Result<Option<Bytes>>>>,
}

#[derive(Debug, PartialEq)]
enum BodyEvent {
    Bytes(Bytes),
    Finish,
}

struct RecordingBodyWriter(Arc<Mutex<Vec<BodyEvent>>>);

#[async_trait]
impl BodyWrite for RecordingBodyWriter {
    async fn write_all_buf(&mut self, data: &mut Bytes) -> Result<()> {
        self.0
            .lock()
            .unwrap()
            .push(BodyEvent::Bytes(std::mem::take(data)));
        Ok(())
    }

    async fn finish(&mut self) -> Result<()> {
        self.0.lock().unwrap().push(BodyEvent::Finish);
        Ok(())
    }

    async fn cleanup(&mut self) -> Result<()> {
        Ok(())
    }

    fn upgrade_body_writer(&mut self) {}
}

struct CustomProxy;

#[async_trait]
impl ProxyHttp<IdleSession> for CustomProxy {
    type CTX = ();

    fn new_ctx(&self) {}

    async fn upstream_peer(
        &self,
        _: &mut Session<IdleSession>,
        _: &mut (),
    ) -> Result<Box<HttpPeer>> {
        unreachable!("test supplies its own body writer")
    }
}

async fn upgraded_custom_reset() -> Box<Error> {
    let (idle_tx, idle_rx) = oneshot::channel();
    let idle_polled = Arc::new(Notify::new());
    // Start after the upgrade handshake; the real relay must forward the last
    // request chunk and EOF before switching this session to its idle watch.
    let mut request = RequestHeader::build("GET", b"/upgrade", None).unwrap();
    request.insert_header("connection", "upgrade").unwrap();
    request.insert_header("upgrade", "websocket").unwrap();
    let mut response = ResponseHeader::build(101, None).unwrap();
    response.insert_header("connection", "upgrade").unwrap();
    response.insert_header("upgrade", "websocket").unwrap();
    let custom = IdleSession {
        request,
        response,
        body: [Some(Bytes::from_static(b"probe")), None].into(),
        body_done: false,
        idle_polled: idle_polled.clone(),
        idle_result: Some(idle_rx),
    };
    let modules = HttpModules::new();
    let mut session = Session::new(
        Box::new(HttpSession::new_custom(custom)),
        &modules,
        #[cfg(feature = "upstream_modules")]
        &modules,
        Arc::new(AtomicBool::new(false)),
    );
    let proxy = HttpProxy::<_, (), IdleSession>::new_custom(
        CustomProxy,
        Arc::new(ServerConf::default()),
        (),
        None,
        None,
        None,
    );
    let events = Arc::new(Mutex::new(Vec::new()));
    let mut writer: Box<dyn BodyWrite> = Box::new(RecordingBodyWriter(events.clone()));
    // These senders stay alive and silent, so neither response completion nor
    // custom-message teardown can rescue a missing request reset watch.
    let (response_tx, response_rx) = mpsc::channel(1);
    let (up_custom_tx, up_custom_rx) = mpsc::channel(1);
    let (down_custom_tx, down_custom_rx) = mpsc::channel(1);
    let (cancel_tx, _cancel_rx) = oneshot::channel();
    let mut ctx = ();
    let relay = proxy.custom_bidirection_down_to_up(
        &mut session,
        &mut writer,
        response_rx,
        &mut ctx,
        up_custom_rx,
        down_custom_rx,
        true,
        cancel_tx,
        None,
        Arc::new(AtomicU8::new(PipeState::Active as u8)),
    );
    tokio::pin!(relay);
    tokio::time::timeout(Duration::from_secs(1), async {
        tokio::select! {
            () = idle_polled.notified() => {},
            result = &mut relay => panic!("relay ended before the idle watch: {result:?}"),
        }
    })
    .await
    .expect("custom reset watch must be polled after upgraded request EOF");
    assert!(
        futures::poll!(&mut relay).is_pending(),
        "half-close alone must leave the relay open"
    );
    let expected = [
        BodyEvent::Bytes(Bytes::from_static(b"probe")),
        BodyEvent::Finish,
    ];
    assert_eq!(*events.lock().unwrap(), expected);
    idle_tx
        .send(Err(Error::explain(
            ReadError,
            "custom reset after request EOF",
        )))
        .unwrap();
    let result = tokio::time::timeout(Duration::from_secs(1), &mut relay).await;
    assert_eq!(
        *events.lock().unwrap(),
        expected,
        "idle results must not write or finish the request again"
    );
    let error = result
        .expect("idle transport event must terminate the relay")
        .expect_err("idle event must be an error");
    assert_eq!(error.esource, pingora_error::ErrorSource::Downstream);
    drop((response_tx, up_custom_tx, down_custom_tx));
    error
}

#[tokio::test]
async fn upgraded_custom_request_eof_preserves_reset_watch() {
    let error = upgraded_custom_reset().await;
    assert_eq!(error.etype, ReadError);
    assert!(error.to_string().contains("custom reset after request EOF"));
}

impl DownstreamSession for IdleSession {
    fn req_header(&self) -> &RequestHeader {
        &self.request
    }

    fn req_header_mut(&mut self) -> &mut RequestHeader {
        &mut self.request
    }

    async fn read_body_bytes(&mut self) -> Result<Option<Bytes>> {
        let body = self.body.pop_front().expect("body read after EOF");
        self.body_done = body.is_none();
        Ok(body)
    }

    async fn drain_request_body(&mut self) -> Result<()> {
        unreachable!("server session: drain_request_body")
    }

    async fn write_response_header(
        &mut self,
        _resp: Box<ResponseHeader>,
        _end: bool,
    ) -> Result<()> {
        unreachable!("server session: write_response_header")
    }

    async fn write_response_header_ref(
        &mut self,
        _resp: &ResponseHeader,
        _end: bool,
    ) -> Result<()> {
        unreachable!("server session: write_response_header_ref")
    }

    async fn write_body(&mut self, _data: Bytes, _end: bool) -> Result<()> {
        unreachable!("server session: write_body")
    }

    async fn write_trailers(&mut self, _trailers: HeaderMap) -> Result<()> {
        unreachable!("server session: write_trailers")
    }

    async fn response_duplex_vec(&mut self, _tasks: Vec<HttpTask>) -> Result<bool> {
        unreachable!("server session: response_duplex_vec")
    }

    fn set_read_timeout(&mut self, _timeout: Option<Duration>) {
        unreachable!("server session: set_read_timeout")
    }

    fn get_read_timeout(&self) -> Option<Duration> {
        unreachable!("server_session: get_read_timeout")
    }

    fn set_write_timeout(&mut self, _timeout: Option<Duration>) {
        unreachable!("server session: set_write_timeout")
    }

    fn get_write_timeout(&self) -> Option<Duration> {
        unreachable!("server_session: get_write_timeout")
    }

    fn set_total_drain_timeout(&mut self, _timeout: Option<Duration>) {
        unreachable!("server session: set_total_drain_timeout")
    }

    fn get_total_drain_timeout(&self) -> Option<Duration> {
        unreachable!("server_session: get_total_drain_timeout")
    }

    fn request_summary(&self) -> String {
        "custom upgrade test".into()
    }

    fn response_written(&self) -> Option<&ResponseHeader> {
        Some(&self.response)
    }

    async fn shutdown(&mut self, _code: u32, _ctx: &str) {
        unreachable!("server session: shutdown")
    }

    fn is_body_done(&mut self) -> bool {
        self.body_done
    }

    async fn finish(&mut self) -> Result<()> {
        unreachable!("server session: finish")
    }

    fn is_body_empty(&mut self) -> bool {
        false
    }

    async fn read_body_or_idle(&mut self, no_body_expected: bool) -> Result<Option<Bytes>> {
        if !no_body_expected {
            return self.read_body_bytes().await;
        }
        assert!(self.body_done, "idle mode must follow request EOF");
        self.idle_polled.notify_one();
        if let Some(receiver) = self.idle_result.as_mut() {
            let result = receiver.await.expect("test must provide an idle result");
            self.idle_result = None;
            result
        } else {
            std::future::pending().await
        }
    }

    fn body_bytes_sent(&self) -> usize {
        unreachable!("server session: body_bytes_sent")
    }

    fn body_bytes_read(&self) -> usize {
        unreachable!("server session: body_bytes_read")
    }

    fn digest(&self) -> Option<&Digest> {
        unreachable!("server session: digest")
    }

    fn digest_mut(&mut self) -> Option<&mut Digest> {
        unreachable!("server session: digest_mut")
    }

    fn client_addr(&self) -> Option<&SocketAddr> {
        unreachable!("server session: client_addr")
    }

    fn server_addr(&self) -> Option<&SocketAddr> {
        unreachable!("server session: server_addr")
    }

    fn pseudo_raw_h1_request_header(&self) -> Bytes {
        unreachable!("server session: pseudo_raw_h1_request_header")
    }

    fn enable_retry_buffering(&mut self) {
        unreachable!("server session: enable_retry_bufferings")
    }

    fn retry_buffer_truncated(&self) -> bool {
        unreachable!("server session: retry_buffer_truncated")
    }

    fn get_retry_buffer(&self) -> Option<Bytes> {
        None
    }

    async fn finish_custom(&mut self) -> Result<()> {
        unreachable!("server session: finish_custom")
    }

    fn take_custom_message_reader(
        &mut self,
    ) -> Option<Box<dyn Stream<Item = Result<Bytes>> + Unpin + Send + Sync + 'static>> {
        unreachable!("server session: get_custom_message_reader")
    }

    fn restore_custom_message_reader(
        &mut self,
        _reader: Box<dyn Stream<Item = Result<Bytes>> + Unpin + Send + Sync + 'static>,
    ) -> Result<()> {
        unreachable!("server session: get_custom_message_reader")
    }

    fn take_custom_message_writer(&mut self) -> Option<Box<dyn CustomMessageWrite>> {
        unreachable!("server session: get_custom_message_writer")
    }

    fn restore_custom_message_writer(
        &mut self,
        _writer: Box<dyn CustomMessageWrite>,
    ) -> Result<()> {
        unreachable!("server session: restore_custom_message_writer")
    }

    fn is_upgrade_req(&self) -> bool {
        true
    }

    fn was_upgraded(&self) -> bool {
        true
    }
}
