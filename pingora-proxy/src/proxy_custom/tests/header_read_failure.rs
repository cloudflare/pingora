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
use http::HeaderMap;
use pingora_core::connectors::http::custom::{
    Connection as CustomConnection, Connector as CustomConnector,
};
use pingora_core::protocols::{l4::socket::SocketAddr, UniqueIDType};

const HEADER_READ_FAILURE: &str = "upstream closed before the response header";

/// A custom upstream that accepts the request header, then fails to read the response header
/// once the request body writer has been used.
struct HeaderReadFailureUpstream {
    body_writer: Option<Box<dyn BodyWrite>>,
    body_written: Arc<tokio::sync::Notify>,
    upgraded_response: Option<PendingUpgradedResponse>,
    framed_response: Option<FramedResponse>,
}

struct PendingUpgradedResponse {
    header: ResponseHeader,
    body_sent: bool,
    downstream_closed: Arc<tokio::sync::Notify>,
}

#[derive(Clone, Copy, Debug)]
enum FramedTail {
    BodyPending,
    TrailersPending,
    ReadError,
}

struct FramedResponse {
    header: ResponseHeader,
    body: Option<Bytes>,
    tail: FramedTail,
}

/// Exercise the real producer and join, not just the downstream task consumer. The upstream
/// leaves its response open after sending all bytes promised by Content-Length.
#[tokio::test]
async fn framed_response_finishes_without_upstream_eos_after_upload_failure() {
    for tail in [FramedTail::BodyPending, FramedTail::TrailersPending] {
        let (mut session, mut client) = upload_session().await;
        client.write_all(b"u").await.unwrap();
        let failed = Arc::new(tokio::sync::Notify::new());
        let mut header = ResponseHeader::build(413, None).unwrap();
        header.insert_header("content-length", "200").unwrap();
        let mut upstream = HeaderReadFailureUpstream {
            body_writer: Some(Box::new(StoppedUploadWriter(failed.clone()))),
            body_written: failed,
            upgraded_response: None,
            framed_response: Some(FramedResponse {
                header,
                body: Some(Bytes::from(vec![b'x'; 200])),
                tail,
            }),
        };
        let proxy = HttpProxy::<_, HeaderReadFailureConnector>::new_custom(
            TestProxy,
            Arc::new(ServerConf::default()),
            HeaderReadFailureConnector,
            None,
            None,
            None,
        );
        let peer = HttpPeer::new(("127.0.0.1", 1), false, String::new());
        let (reuse, error) = tokio::time::timeout(
            Duration::from_secs(1),
            proxy.proxy_to_custom_upstream(
                &mut session,
                &mut upstream,
                false,
                &peer,
                &mut (),
                &mut false,
            ),
        )
        .await
        .expect("complete downstream response must cancel the stalled response reader");
        assert!(error.is_none(), "{tail:?}: {error:?}");
        assert!(reuse);
        read_rejection(&mut client, 413).await;
    }
}

#[tokio::test]
async fn partial_framed_response_still_reports_upload_failure() {
    let (mut session, mut client) = upload_session().await;
    client.write_all(b"u").await.unwrap();
    let failed = Arc::new(tokio::sync::Notify::new());
    let mut header = ResponseHeader::build(413, None).unwrap();
    header.insert_header("content-length", "200").unwrap();
    let mut upstream = HeaderReadFailureUpstream {
        body_writer: Some(Box::new(StoppedUploadWriter(failed.clone()))),
        body_written: failed,
        upgraded_response: None,
        framed_response: Some(FramedResponse {
            header,
            body: Some(Bytes::from_static(b"partial")),
            tail: FramedTail::ReadError,
        }),
    };
    let proxy = HttpProxy::<_, HeaderReadFailureConnector>::new_custom(
        TestProxy,
        Arc::new(ServerConf::default()),
        HeaderReadFailureConnector,
        None,
        None,
        None,
    );
    let peer = HttpPeer::new(("127.0.0.1", 1), false, String::new());
    let (reuse, error) = tokio::time::timeout(
        Duration::from_secs(1),
        proxy.proxy_to_custom_upstream(
            &mut session,
            &mut upstream,
            false,
            &peer,
            &mut (),
            &mut false,
        ),
    )
    .await
    .unwrap();
    assert!(!reuse);
    let error = error.expect("partial response must fail");
    assert_eq!(error.etype(), &WriteError);
    assert_eq!(error.esource, ErrorSource::Upstream);
}

struct HeaderReadFailureConnector;

/// Accepts every piece of the request body.
struct AcceptingUploadWriter(Arc<tokio::sync::Notify>);

/// Leaves the upload pending even after the response reader fails, like a connector with no
/// write timeout whose peer stopped reading. Dropping the proxy future must cancel this write.
struct BlockedUploadWriter(Arc<tokio::sync::Notify>);

#[async_trait]
impl BodyWrite for BlockedUploadWriter {
    async fn write_all_buf(&mut self, _data: &mut Bytes) -> Result<()> {
        self.0.notify_one();
        std::future::pending().await
    }

    async fn finish(&mut self) -> Result<()> {
        unreachable!("blocked write cannot finish")
    }

    async fn cleanup(&mut self) -> Result<()> {
        panic!("failed response must cancel the upload without cleanup")
    }

    fn upgrade_body_writer(&mut self) {}
}

#[async_trait]
impl BodyWrite for AcceptingUploadWriter {
    async fn write_all_buf(&mut self, data: &mut Bytes) -> Result<()> {
        *data = Bytes::new();
        self.0.notify_one();
        Ok(())
    }

    async fn finish(&mut self) -> Result<()> {
        Ok(())
    }

    async fn cleanup(&mut self) -> Result<()> {
        Ok(())
    }

    fn upgrade_body_writer(&mut self) {}
}

/// Proxy an upload whose client sent its first byte to an upstream whose response header read
/// fails, through the real custom upstream path, and return the reported error.
async fn upload_with_header_read_failure(
    writer: impl FnOnce(Arc<tokio::sync::Notify>) -> Box<dyn BodyWrite>,
) -> Box<Error> {
    upload_with_header_read_failure_on_retry(writer, false).await
}

async fn upload_with_header_read_failure_on_retry(
    writer: impl FnOnce(Arc<tokio::sync::Notify>) -> Box<dyn BodyWrite>,
    replay_buffer: bool,
) -> Box<Error> {
    let (mut session, mut client) = upload_session().await;
    client.write_all(b"u").await.unwrap();
    if replay_buffer {
        session.as_mut().enable_retry_buffering();
        let body = session
            .downstream_session
            .read_request_body()
            .await
            .unwrap();
        assert_eq!(body, Some(Bytes::from_static(b"u")));
        assert_eq!(session.as_mut().get_retry_buffer(), body);
    }
    let body_written = Arc::new(tokio::sync::Notify::new());
    let mut upstream = HeaderReadFailureUpstream {
        body_writer: Some(writer(body_written.clone())),
        body_written,
        upgraded_response: None,
        framed_response: None,
    };
    let proxy = HttpProxy::<_, HeaderReadFailureConnector>::new_custom(
        TestProxy,
        Arc::new(ServerConf::default()),
        HeaderReadFailureConnector,
        None,
        None,
        None,
    );
    let peer = HttpPeer::new(("127.0.0.1", 1), false, String::new());
    let mut request_write_attempted = false;
    let (reuse, error) = proxy
        .proxy_to_custom_upstream(
            &mut session,
            &mut upstream,
            false,
            &peer,
            &mut (),
            &mut request_write_attempted,
        )
        .await;
    assert!(request_write_attempted);
    assert!(!reuse);
    error.expect("a response header read failure must fail the request")
}

/// An upload failure is reported over a later failure to read the response header, as on HTTP/1.
#[tokio::test]
async fn upload_error_is_reported_over_header_read_failure() {
    tokio::time::timeout(Duration::from_secs(3), async {
        let (_, _, _, _, h1_error) = h1_upload_response(&[], false).await;
        let h1_error = h1_error.expect("H1 must report the failed upload");

        let error =
            upload_with_header_read_failure(|failed| Box::new(StoppedUploadWriter(failed))).await;

        assert_eq!(
            error.etype(),
            &WriteError,
            "the upload error must win: {error}"
        );
        assert_eq!(error.esource, ErrorSource::Upstream);
        assert!(
            error.context.as_ref().is_some_and(|context| context
                .as_str()
                .contains("upstream stopped accepting the upload")),
            "{error}"
        );
        assert_eq!(error.etype(), h1_error.etype(), "H1: {h1_error}");
        assert_eq!(error.esource, h1_error.esource, "H1: {h1_error}");
    })
    .await
    .expect("custom upstream must report the failure promptly");
}

/// Without an upload failure, the response header read failure is reported as it is.
#[tokio::test]
async fn header_read_failure_is_reported_without_upload_failure() {
    tokio::time::timeout(Duration::from_secs(3), async {
        let error =
            upload_with_header_read_failure(|written| Box::new(AcceptingUploadWriter(written)))
                .await;

        assert_eq!(error.etype(), &ConnectionClosed, "{error}");
        assert_eq!(error.esource, ErrorSource::Upstream);
        assert_eq!(
            error.context.as_ref().map(|context| context.as_str()),
            Some(HEADER_READ_FAILURE)
        );
    })
    .await
    .expect("custom upstream must report the failure promptly");
}

#[tokio::test]
async fn header_read_failure_cancels_blocked_upload() {
    for replay_buffer in [false, true] {
        let error = tokio::time::timeout(
            Duration::from_secs(1),
            upload_with_header_read_failure_on_retry(
                |writing| Box::new(BlockedUploadWriter(writing)),
                replay_buffer,
            ),
        )
        .await
        .expect(
            "header read failure must cancel a blocked writer without waiting for a write timeout",
        );
        assert_eq!(error.etype(), &ConnectionClosed);
        assert_eq!(error.esource, ErrorSource::Upstream);
        assert_eq!(
            error.context.as_ref().map(|c| c.as_str()),
            Some(HEADER_READ_FAILURE)
        );
    }
}

#[tokio::test]
async fn retry_upload_error_is_reported_over_header_read_failure() {
    let error = tokio::time::timeout(
        Duration::from_secs(1),
        upload_with_header_read_failure_on_retry(
            |failed| Box::new(StoppedUploadWriter(failed)),
            true,
        ),
    )
    .await
    .expect("failed retry upload must still take precedence without hanging");
    assert_eq!(error.etype(), &WriteError);
    assert_eq!(error.esource, ErrorSource::Upstream);
}

/// Once a cached response was selected, a subsequent failed upstream task must not replace it.
/// This also covers a header-read failure queued after an upload failure; without the upload
/// failure the producer returns the header error directly, as before.
#[tokio::test]
async fn cached_response_discards_failed_upstream_like_h1() {
    let proxy = HttpProxy::new(TestProxy, Arc::new(ServerConf::default()));
    let (mut session, _client) = upload_session().await;
    let (_, mut rx) = mpsc::channel(TASK_BUFFER_SIZE);
    let mut cached = ServeFromCache::new();
    cached.enable();
    let mut response_state = ResponseStateMachine::new();
    response_state.enable_cached_response();
    let mut range = proxy_cache::range_filter::RangeBodyFilter::new();
    let mut upload_error = Some(Error::explain(WriteError, "rejected upload").into_up());
    let result = proxy
        .process_upstream_tasks_custom(
            &mut session,
            &mut (),
            HttpTask::Failed(Error::explain(ConnectionClosed, HEADER_READ_FAILURE).into_up()),
            &mut rx,
            &mut cached,
            &mut range,
            &mut response_state,
            &mut upload_error,
        )
        .await;
    assert_eq!(result.unwrap(), None);
    assert!(upload_error.is_none());
    assert!(response_state.upstream_producer_done());
    assert!(!response_state.upstream_response_complete());
    assert!(!response_state.cached_done());
    assert!(session.response_written().is_none());
    assert!(cached.should_discard_upstream());
}

#[tokio::test(start_paused = true)]
async fn full_custom_relay_succeeds_after_upgraded_downstream_half_close() {
    tokio::time::timeout(Duration::from_secs(3), async {
        let (mut session, mut client) = upgrade_session().await;
        let failed = Arc::new(tokio::sync::Notify::new());
        let downstream_closed = Arc::new(tokio::sync::Notify::new());
        let mut header = ResponseHeader::build(101, None).unwrap();
        header.insert_header("connection", "upgrade").unwrap();
        header.insert_header("upgrade", "websocket").unwrap();
        let mut upstream = HeaderReadFailureUpstream {
            body_writer: Some(Box::new(UpgradedUploadWriter {
                fails_first_write: false,
                writes: 0,
                failed: failed.clone(),
            })),
            body_written: failed,
            framed_response: None,
            upgraded_response: Some(PendingUpgradedResponse {
                header,
                body_sent: false,
                downstream_closed: downstream_closed.clone(),
            }),
        };
        let proxy = HttpProxy::<_, HeaderReadFailureConnector>::new_custom(
            TestProxy,
            Arc::new(ServerConf::default()),
            HeaderReadFailureConnector,
            None,
            None,
            None,
        );
        let peer = HttpPeer::new(("127.0.0.1", 1), false, String::new());
        let mut request_write_attempted = false;
        let mut ctx = ();
        let ((_, error), ()) = tokio::join!(
            proxy.proxy_to_custom_upstream(
                &mut session,
                &mut upstream,
                false,
                &peer,
                &mut ctx,
                &mut request_write_attempted,
            ),
            async {
                upgraded_client(&mut client, UpgradedUploadFailure::AfterSwitch).await;
                client.shutdown().await.unwrap();
                downstream_closed.notify_one();
            },
        );
        assert!(request_write_attempted);
        assert!(error.is_none(), "half-close should win: {error:?}");
    })
    .await
    .expect("full custom relay must finish after a late upstream read failure");
}

#[async_trait]
impl CustomConnector for HeaderReadFailureConnector {
    type Session = HeaderReadFailureUpstream;

    async fn get_http_session<P: Peer + Send + Sync + 'static>(
        &self,
        _peer: &P,
    ) -> Result<(CustomConnection<Self::Session>, bool)> {
        unreachable!("connector: get_http_session")
    }

    async fn reused_http_session<P: Peer + Send + Sync + 'static>(
        &self,
        _peer: &P,
    ) -> Option<Self::Session> {
        unreachable!("connector: reused_http_session")
    }

    async fn release_http_session<P: Peer + Send + Sync + 'static>(
        &self,
        _session: Self::Session,
        _peer: &P,
        _idle_timeout: Option<Duration>,
    ) {
        unreachable!("connector: release_http_session")
    }
}

#[async_trait]
impl CustomSession for HeaderReadFailureUpstream {
    async fn write_request_header(&mut self, _req: Box<RequestHeader>, _end: bool) -> Result<()> {
        Ok(())
    }

    async fn write_request_body(&mut self, _data: Bytes, _end: bool) -> Result<()> {
        unreachable!("client session: write_request_body")
    }

    async fn finish_request_body(&mut self) -> Result<()> {
        unreachable!("client session: finish_request_body")
    }

    fn set_read_timeout(&mut self, _timeout: Option<Duration>) {}

    fn set_write_timeout(&mut self, _timeout: Option<Duration>) {}

    async fn read_response_header(&mut self) -> Result<()> {
        if self.upgraded_response.is_some() || self.framed_response.is_some() {
            return Ok(());
        }
        self.body_written.notified().await;
        Error::e_explain(ConnectionClosed, HEADER_READ_FAILURE)
    }

    async fn read_response_body(&mut self) -> Result<Option<Bytes>> {
        if let Some(response) = &mut self.framed_response {
            if let Some(body) = response.body.take() {
                self.body_written.notified().await;
                return Ok(Some(body));
            }
            return match response.tail {
                FramedTail::BodyPending => std::future::pending().await,
                FramedTail::TrailersPending => Ok(None),
                FramedTail::ReadError => Error::e_explain(ReadError, "incomplete response"),
            };
        }
        let Some(response) = &mut self.upgraded_response else {
            unreachable!("client session: read_response_body")
        };
        if !response.body_sent {
            self.body_written.notified().await;
            response.body_sent = true;
            return Ok(Some(Bytes::from_static(b"server")));
        }
        response.downstream_closed.notified().await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        Error::e_explain(ReadError, "upgraded response read failed")
    }

    fn response_finished(&self) -> bool {
        false
    }

    async fn shutdown(&mut self, _code: u32, _ctx: &str) {
        unreachable!("client session: shutdown")
    }

    fn response_header(&self) -> Option<&ResponseHeader> {
        self.upgraded_response
            .as_ref()
            .map(|response| &response.header)
            .or_else(|| {
                self.framed_response
                    .as_ref()
                    .map(|response| &response.header)
            })
    }

    fn was_upgraded(&self) -> bool {
        self.upgraded_response.is_some()
    }

    fn digest(&self) -> Option<&Digest> {
        None
    }

    fn digest_mut(&mut self) -> Option<&mut Digest> {
        unreachable!("client session: digest_mut")
    }

    fn server_addr(&self) -> Option<&SocketAddr> {
        unreachable!("client session: server_addr")
    }

    fn client_addr(&self) -> Option<&SocketAddr> {
        unreachable!("client session: client_addr")
    }

    async fn read_trailers(&mut self) -> Result<Option<HeaderMap>> {
        if self.framed_response.is_some() {
            return std::future::pending().await;
        }
        unreachable!("client session: read_trailers")
    }

    fn fd(&self) -> UniqueIDType {
        0
    }

    async fn check_response_end_or_error(&mut self, _headers: bool) -> Result<bool> {
        Ok(false)
    }

    fn take_request_body_writer(&mut self) -> Option<Box<dyn BodyWrite>> {
        self.body_writer.take()
    }

    async fn finish_custom(&mut self) -> Result<()> {
        unreachable!("client session: finish_custom")
    }

    fn take_custom_message_reader(
        &mut self,
    ) -> Option<Box<dyn futures::Stream<Item = Result<Bytes>> + Unpin + Send + Sync + 'static>>
    {
        // The upstream sends no custom messages.
        Some(Box::new(futures::stream::empty()))
    }

    async fn drain_custom_messages(&mut self) -> Result<()> {
        unreachable!("client session: drain_custom_messages")
    }

    fn take_custom_message_writer(&mut self) -> Option<Box<dyn CustomMessageWrite>> {
        Some(Box::new(()))
    }
}
