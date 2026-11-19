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
use pingora_core::protocols::http::v2::server::{
    self as h2_server, H2Accept, HttpSession as SessionV2,
};
use pingora_core::protocols::l4::socket::SocketAddr;
use std::collections::VecDeque;
use std::sync::atomic::AtomicUsize;

/// The request body length the uploads declare; the client sends only its first byte.
const DECLARED_BODY_LEN: usize = 65536;

#[derive(Debug, PartialEq)]
struct FinishCall {
    request_body_read: usize,
    response_ended: bool,
}

/// A custom downstream whose client has sent the start of a larger declared request body.
/// It records the response the proxy writes and each call to `finish`.
struct RecordingDownstream {
    request: RequestHeader,
    sent_unread: VecDeque<Bytes>,
    body_read: usize,
    response: Option<ResponseHeader>,
    response_body: Vec<u8>,
    response_ended: bool,
    finishes: Vec<FinishCall>,
}

struct CustomDownstreamProxy(Option<Arc<AtomicUsize>>);

#[async_trait]
impl ProxyHttp<RecordingDownstream> for CustomDownstreamProxy {
    type CTX = ();

    fn new_ctx(&self) {}

    async fn upstream_peer(
        &self,
        _session: &mut Session<RecordingDownstream>,
        _ctx: &mut (),
    ) -> Result<Box<HttpPeer>> {
        unreachable!("test calls custom_bidirection_down_to_up directly")
    }

    async fn request_body_filter(
        &self,
        _session: &mut Session<RecordingDownstream>,
        body: &mut Option<Bytes>,
        _end: bool,
        _ctx: &mut (),
    ) -> Result<()> {
        if let Some(filter_calls) = &self.0 {
            let call = filter_calls.fetch_add(1, Ordering::SeqCst);
            if (1..=TASK_BUFFER_SIZE).contains(&call) {
                *body = Some(Bytes::new());
            }
        }
        Ok(())
    }
}

/// A session whose client has sent `pieces` one-byte pieces of the declared body.
fn custom_upload_session(pieces: usize) -> Session<RecordingDownstream> {
    let mut request = RequestHeader::build("POST", b"/upload", None).unwrap();
    request
        .insert_header("content-length", DECLARED_BODY_LEN.to_string())
        .unwrap();
    let downstream = RecordingDownstream {
        request,
        sent_unread: std::iter::repeat_n(Bytes::from_static(b"u"), pieces).collect(),
        body_read: 0,
        response: None,
        response_body: Vec::new(),
        response_ended: false,
        finishes: Vec::new(),
    };
    let modules = HttpModules::new();
    Session::new(
        Box::new(HttpSession::new_custom(downstream)),
        &modules,
        #[cfg(feature = "upstream_modules")]
        &modules,
        Arc::new(AtomicBool::new(false)),
    )
}

async fn proxy_custom_upload(
    session: &mut Session<RecordingDownstream>,
    rx: mpsc::Receiver<HttpTask>,
    failed: Arc<tokio::sync::Notify>,
) -> Result<bool> {
    proxy_custom_upload_with_filter(session, rx, failed, None).await
}

async fn proxy_custom_upload_with_filter(
    session: &mut Session<RecordingDownstream>,
    rx: mpsc::Receiver<HttpTask>,
    failed: Arc<tokio::sync::Notify>,
    filter_calls: Option<Arc<AtomicUsize>>,
) -> Result<bool> {
    let proxy = HttpProxy::<_, (), RecordingDownstream>::new_custom(
        CustomDownstreamProxy(filter_calls),
        Arc::new(ServerConf::default()),
        (),
        None,
        None,
        None,
    );
    let mut writer: Box<dyn BodyWrite> = Box::new(StoppedUploadWriter(failed));
    // No custom messages remain in either direction.
    let up_rx = mpsc::channel(1).1;
    let down_rx = mpsc::channel(1).1;
    let (cancel_tx, _) = oneshot::channel();
    let state = Arc::new(AtomicU8::new(PipeState::Active as u8));
    let result = proxy
        .custom_bidirection_down_to_up(
            session,
            &mut writer,
            rx,
            &mut (),
            up_rx,
            down_rx,
            true,
            cancel_tx,
            None,
            state.clone(),
        )
        .await;
    assert_eq!(
        PipeState::is_downstream_complete(state.load(Ordering::Acquire)),
        result.is_ok()
    );
    result
}

async fn respond_after_upload_failure(tx: mpsc::Sender<HttpTask>, failed: &tokio::sync::Notify) {
    failed.notified().await;
    for task in rejection_tasks() {
        if tx.send(task).await.is_err() {
            break;
        }
    }
}

#[tokio::test]
async fn custom_downstream_finishes_complete_response_with_request_body_unread() {
    tokio::time::timeout(Duration::from_secs(3), async {
        let mut session = custom_upload_session(1);
        let failed = Arc::new(tokio::sync::Notify::new());
        let (tx, rx) = mpsc::channel(TASK_BUFFER_SIZE);
        let (result, ()) = tokio::join!(
            proxy_custom_upload(&mut session, rx, failed.clone()),
            respond_after_upload_failure(tx, &failed)
        );
        assert!(
            matches!(result, Ok(true)),
            "complete response must survive upload failure: {result:?}"
        );
        assert!(!session.downstream_session.is_body_done());
        let downstream = session.downstream_session.as_custom().unwrap();
        assert_eq!(
            downstream
                .response
                .as_ref()
                .map(|header| header.status.as_u16()),
            Some(413)
        );
        assert_eq!(downstream.response_body, [b'x'; 200]);
        assert!(downstream.response_ended);
        assert_eq!(
            downstream.finishes,
            [FinishCall {
                request_body_read: 1,
                response_ended: true,
            }]
        );
    })
    .await
    .expect("custom downstream must finish promptly");
}

/// After an upload failure the downstream is read only as far as the HTTP/1 path reads it: that
/// path stops once its task pipe, which nothing drains after the failure, is full.
#[tokio::test(start_paused = true)]
async fn custom_downstream_reads_as_far_as_h1_after_upload_failure() {
    tokio::time::timeout(Duration::from_secs(3), async {
        let mut session = custom_upload_session(10);
        let failed = Arc::new(tokio::sync::Notify::new());
        let (tx, rx) = mpsc::channel(TASK_BUFFER_SIZE);
        let respond = async {
            failed.notified().await;
            // Virtual time advances only after the loop has read everything it may read.
            tokio::time::sleep(Duration::from_millis(50)).await;
            for task in rejection_tasks() {
                tx.send(task).await.unwrap();
            }
        };
        let (result, ()) = tokio::join!(
            proxy_custom_upload(&mut session, rx, failed.clone()),
            respond
        );
        assert!(
            matches!(result, Ok(true)),
            "complete response must survive upload failure: {result:?}"
        );
        // The failed piece, then as many as fill the HTTP/1 task pipe.
        let read = 1 + TASK_BUFFER_SIZE;
        let downstream = session.downstream_session.as_custom().unwrap();
        assert_eq!(downstream.body_read, read);
        assert_eq!(
            downstream.finishes,
            [FinishCall {
                request_body_read: read,
                response_ended: true,
            }]
        );
    })
    .await
    .expect("custom downstream must finish promptly");
}

#[tokio::test(start_paused = true)]
async fn filtered_empty_pieces_do_not_fill_h1_upload_failure_budget() {
    tokio::time::timeout(Duration::from_secs(3), async {
        let mut session = custom_upload_session(1 + 2 * TASK_BUFFER_SIZE + 1);
        let failed = Arc::new(tokio::sync::Notify::new());
        let filter_calls = Arc::new(AtomicUsize::new(0));
        let (tx, rx) = mpsc::channel(TASK_BUFFER_SIZE);
        let respond = async {
            failed.notified().await;
            tokio::time::sleep(Duration::from_millis(50)).await;
            for task in rejection_tasks() {
                tx.send(task).await.unwrap();
            }
        };
        let (result, ()) = tokio::join!(
            proxy_custom_upload_with_filter(
                &mut session,
                rx,
                failed.clone(),
                Some(filter_calls.clone()),
            ),
            respond
        );
        assert!(
            matches!(result, Ok(true)),
            "complete response was lost: {result:?}"
        );
        let read = 1 + 2 * TASK_BUFFER_SIZE;
        assert_eq!(filter_calls.load(Ordering::SeqCst), read);
        let downstream = session.downstream_session.as_custom().unwrap();
        assert_eq!(downstream.body_read, read);
        assert_eq!(downstream.finishes[0].request_body_read, read);
    })
    .await
    .expect("filtered upload must finish promptly");
}

struct H2Upload {
    session: Session,
    response: h2::client::ResponseFuture,
    request_body: h2::SendStream<Bytes>,
    _requests: h2::client::SendRequest<Bytes>,
}

/// Open an HTTP/2 connection whose client sends the first byte of a larger declared upload and
/// leaves its request stream open.
async fn h2_upload_session() -> H2Upload {
    let (client_io, server_io) = tokio::io::duplex(65536);
    let (mut requests, connection) = h2::client::handshake(client_io).await.unwrap();
    tokio::spawn(async move {
        let _ = connection.await;
    });
    let mut server_connection = h2_server::handshake(Box::new(server_io), None)
        .await
        .unwrap();
    let request = http::Request::builder()
        .method("POST")
        .uri("https://example.com/upload")
        .header("content-length", DECLARED_BODY_LEN.to_string())
        .body(())
        .unwrap();
    let (response, mut request_body) = requests.send_request(request, false).unwrap();
    request_body
        .send_data(Bytes::from_static(b"u"), false)
        .unwrap();
    let digest = Arc::new(Digest::default());
    let Some(H2Accept::Session(http)) =
        SessionV2::from_h2_conn(&mut server_connection, digest.clone())
            .await
            .unwrap()
    else {
        panic!("expected the H2 upload request")
    };
    tokio::spawn(async move {
        while let Ok(Some(_)) =
            SessionV2::from_h2_conn(&mut server_connection, digest.clone()).await
        {}
    });
    let modules = HttpModules::new();
    let session = Session::new(
        Box::new(HttpSession::new_http2(http)),
        &modules,
        #[cfg(feature = "upstream_modules")]
        &modules,
        Arc::new(AtomicBool::new(false)),
    );
    H2Upload {
        session,
        response,
        request_body,
        _requests: requests,
    }
}

#[tokio::test]
async fn h2_downstream_delivers_complete_response_with_request_body_unread() {
    tokio::time::timeout(Duration::from_secs(3), async {
        let mut upload = h2_upload_session().await;
        let failed = Arc::new(tokio::sync::Notify::new());
        let (tx, rx) = mpsc::channel(TASK_BUFFER_SIZE);
        let (result, ()) = tokio::join!(
            proxy_upload(&mut upload.session, rx, failed.clone()),
            respond_after_upload_failure(tx, &failed)
        );
        assert!(
            matches!(result, Ok(true)),
            "complete response must survive upload failure: {result:?}"
        );
        let downstream = &mut upload.session.downstream_session;
        assert!(downstream.is_response_complete());
        assert!(!downstream.is_body_done());
        assert_eq!(downstream.body_bytes_read(), 1);

        let response = upload.response.await.unwrap();
        assert_eq!(response.status(), 413);
        let mut body = response.into_body();
        let mut received = Vec::new();
        while let Some(chunk) = body.data().await {
            received.extend_from_slice(&chunk.unwrap());
        }
        assert_eq!(received, [b'x'; 200]);
        assert!(body.is_end_stream());

        let mut reset = std::future::poll_fn(|cx| upload.request_body.poll_reset(cx));
        assert!(
            (&mut reset).now_or_never().is_none(),
            "the request stream must stay open while the session is held"
        );
        drop(upload.session);
        assert_eq!(reset.await.unwrap(), h2::Reason::NO_ERROR);
    })
    .await
    .expect("H2 downstream must finish promptly");
}

impl DownstreamSession for RecordingDownstream {
    fn req_header(&self) -> &RequestHeader {
        &self.request
    }

    fn req_header_mut(&mut self) -> &mut RequestHeader {
        &mut self.request
    }

    async fn read_body_bytes(&mut self) -> Result<Option<Bytes>> {
        let Some(chunk) = self.sent_unread.pop_front() else {
            return std::future::pending().await;
        };
        self.body_read += chunk.len();
        Ok(Some(chunk))
    }

    async fn drain_request_body(&mut self) -> Result<()> {
        unreachable!("server session: drain_request_body")
    }

    async fn write_response_header(&mut self, resp: Box<ResponseHeader>, end: bool) -> Result<()> {
        self.response = Some(*resp);
        self.response_ended |= end;
        Ok(())
    }

    async fn write_response_header_ref(&mut self, resp: &ResponseHeader, end: bool) -> Result<()> {
        self.write_response_header(Box::new(resp.clone()), end)
            .await
    }

    async fn write_body(&mut self, data: Bytes, end: bool) -> Result<()> {
        self.response_body.extend_from_slice(&data);
        self.response_ended |= end;
        Ok(())
    }

    async fn write_trailers(&mut self, _trailers: http::HeaderMap) -> Result<()> {
        self.response_ended = true;
        Ok(())
    }

    async fn response_duplex_vec(&mut self, tasks: Vec<HttpTask>) -> Result<bool> {
        for task in tasks {
            match task {
                HttpTask::Header(header, end) => self.write_response_header(header, end).await?,
                HttpTask::Body(data, end) => self.write_body(data.unwrap_or_default(), end).await?,
                HttpTask::Trailer(_) | HttpTask::Done => self.response_ended = true,
                HttpTask::UpgradedBody(..) => unreachable!("server session: upgraded body"),
                HttpTask::Failed(e) => return Err(e),
            }
        }
        Ok(self.response_ended)
    }

    fn set_read_timeout(&mut self, _timeout: Option<Duration>) {
        unreachable!("server session: set_read_timeout")
    }

    fn get_read_timeout(&self) -> Option<Duration> {
        unreachable!("server session: get_read_timeout")
    }

    fn set_write_timeout(&mut self, _timeout: Option<Duration>) {
        unreachable!("server session: set_write_timeout")
    }

    fn get_write_timeout(&self) -> Option<Duration> {
        unreachable!("server session: get_write_timeout")
    }

    fn set_total_drain_timeout(&mut self, _timeout: Option<Duration>) {
        unreachable!("server session: set_total_drain_timeout")
    }

    fn get_total_drain_timeout(&self) -> Option<Duration> {
        unreachable!("server session: get_total_drain_timeout")
    }

    fn request_summary(&self) -> String {
        "custom upload test".into()
    }

    fn response_written(&self) -> Option<&ResponseHeader> {
        self.response.as_ref()
    }

    async fn shutdown(&mut self, _code: u32, _ctx: &str) {
        unreachable!("server session: shutdown")
    }

    fn is_body_done(&mut self) -> bool {
        self.body_read == DECLARED_BODY_LEN
    }

    async fn finish(&mut self) -> Result<()> {
        self.finishes.push(FinishCall {
            request_body_read: self.body_read,
            response_ended: self.response_ended,
        });
        Ok(())
    }

    fn is_body_empty(&mut self) -> bool {
        false
    }

    async fn read_body_or_idle(&mut self, no_body_expected: bool) -> Result<Option<Bytes>> {
        if no_body_expected {
            std::future::pending().await
        } else {
            self.read_body_bytes().await
        }
    }

    fn body_bytes_sent(&self) -> usize {
        self.response_body.len()
    }

    fn body_bytes_read(&self) -> usize {
        self.body_read
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
        unreachable!("server session: enable_retry_buffering")
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
    ) -> Option<Box<dyn futures::Stream<Item = Result<Bytes>> + Unpin + Send + Sync + 'static>>
    {
        unreachable!("server session: take_custom_message_reader")
    }

    fn restore_custom_message_reader(
        &mut self,
        _reader: Box<dyn futures::Stream<Item = Result<Bytes>> + Unpin + Send + Sync + 'static>,
    ) -> Result<()> {
        unreachable!("server session: restore_custom_message_reader")
    }

    fn take_custom_message_writer(&mut self) -> Option<Box<dyn CustomMessageWrite>> {
        unreachable!("server session: take_custom_message_writer")
    }

    fn restore_custom_message_writer(
        &mut self,
        _writer: Box<dyn CustomMessageWrite>,
    ) -> Result<()> {
        unreachable!("server session: restore_custom_message_writer")
    }
}
