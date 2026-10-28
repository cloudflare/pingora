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

//! The downstream abandonment of a request outlives the attempt it happened in, so a retried
//! attempt that fails while its custom upstream session is being set up must still be abandoned
//! exactly once.

use super::*;
use futures::Stream;
use http::HeaderMap;
use pingora_core::connectors::http::custom::{Connection, Connector};
use pingora_core::protocols::http::custom::{client::Session, BodyWrite, CustomMessageWrite};
use pingora_core::protocols::{l4::socket::SocketAddr as UpstreamAddr, UniqueIDType};
use pingora_core::upstreams::peer::Peer;
use std::collections::VecDeque;
use std::sync::Mutex;

/// Nothing is admitted into it: no attempt gets as far as a response header.
static RETRY_CACHE: LazyLock<MemCache> = LazyLock::new(MemCache::new);

#[derive(Clone, Copy)]
enum SetupFailure {
    /// The session has no custom message reader to hand out.
    NoReader,
    /// The session has no custom message writer to hand out.
    NoWriter,
    /// The session is fine, but the `custom_forwarding` hook fails.
    ForwardingHook,
}

enum Behavior {
    /// Reads no response header until released, then fails to read it.
    FailHeader(Option<oneshot::Receiver<()>>),
    Setup(SetupFailure),
}

struct AttemptSession {
    behavior: Behavior,
    /// Abandon and shutdown calls this session recorded.
    abandon_calls: Arc<Mutex<Vec<String>>>,
    request_header_written: Arc<Notify>,
}

struct RetryConnector {
    /// The sessions to hand out, one per attempt, in order.
    attempts: Mutex<VecDeque<AttemptSession>>,
    taken: Arc<AtomicUsize>,
}

#[async_trait]
impl Connector for RetryConnector {
    type Session = AttemptSession;

    async fn get_http_session<P: Peer + Send + Sync + 'static>(
        &self,
        _peer: &P,
    ) -> Result<(Connection<Self::Session>, bool)> {
        let session = self
            .attempts
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected extra attempt");
        self.taken.fetch_add(1, Ordering::Relaxed);
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

#[async_trait]
impl Session for AttemptSession {
    async fn write_request_header(
        &mut self,
        _request: Box<RequestHeader>,
        _end: bool,
    ) -> Result<()> {
        self.request_header_written.notify_one();
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
        let Behavior::FailHeader(release) = &mut self.behavior else {
            std::future::pending().await
        };
        if let Some(release) = release.take() {
            release.await.expect("test did not release the attempt");
        }
        // A new error is not retried unless marked, and the retry is what this test needs.
        Error::e_explain(ErrorType::ReadError, "injected response header error").map_err(|mut e| {
            e.set_retry(true);
            e.into_up()
        })
    }

    async fn read_response_body(&mut self) -> Result<Option<Bytes>> {
        unreachable!("no attempt receives a response header")
    }

    fn response_finished(&self) -> bool {
        false
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
        None
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

    fn server_addr(&self) -> Option<&UpstreamAddr> {
        None
    }

    fn client_addr(&self) -> Option<&UpstreamAddr> {
        None
    }

    async fn read_trailers(&mut self) -> Result<Option<HeaderMap>> {
        Ok(None)
    }

    fn fd(&self) -> UniqueIDType {
        -1
    }

    async fn check_response_end_or_error(&mut self, _headers: bool) -> Result<bool> {
        Ok(false)
    }

    fn take_request_body_writer(&mut self) -> Option<Box<dyn BodyWrite>> {
        Some(Box::new(NoRequestBody))
    }

    async fn finish_custom(&mut self) -> Result<()> {
        Ok(())
    }

    fn take_custom_message_reader(
        &mut self,
    ) -> Option<Box<dyn Stream<Item = Result<Bytes>> + Unpin + Send + Sync + 'static>> {
        if matches!(self.behavior, Behavior::Setup(SetupFailure::NoReader)) {
            return None;
        }
        Some(Box::new(futures::stream::pending()))
    }

    async fn drain_custom_messages(&mut self) -> Result<()> {
        Ok(())
    }

    fn take_custom_message_writer(&mut self) -> Option<Box<dyn CustomMessageWrite>> {
        if matches!(self.behavior, Behavior::Setup(SetupFailure::NoWriter)) {
            return None;
        }
        Some(Box::new(()))
    }
}

struct NoRequestBody;

#[async_trait]
impl BodyWrite for NoRequestBody {
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

/// The client leaves while the first attempt waits for its response header, which the proxy
/// reports as a downstream abandonment. The first attempt then fails and the proxy retries, and
/// the retry fails during setup. The first attempt is abandoned once, by the proxy's handling of
/// the abandonment, and so must the retry be.
async fn assert_retried_setup_failure_abandons_once(
    path: &'static str,
    setup_failure: SetupFailure,
    expected_error: ErrorType,
) {
    let observed = Arc::new(Observations {
        expected_error: Some(expected_error),
        fail_custom_forwarding_from: matches!(setup_failure, SetupFailure::ForwardingHook)
            .then_some(1),
        ..Default::default()
    });
    let (fail_first, first_failed) = oneshot::channel();
    let first_calls = Arc::new(Mutex::new(Vec::new()));
    let retry_calls = Arc::new(Mutex::new(Vec::new()));
    let first_request_written = Arc::new(Notify::new());
    let taken = Arc::new(AtomicUsize::new(0));
    let connector = RetryConnector {
        attempts: Mutex::new(VecDeque::from([
            AttemptSession {
                behavior: Behavior::FailHeader(Some(first_failed)),
                abandon_calls: Arc::clone(&first_calls),
                request_header_written: Arc::clone(&first_request_written),
            },
            AttemptSession {
                behavior: Behavior::Setup(setup_failure),
                abandon_calls: Arc::clone(&retry_calls),
                request_header_written: Arc::new(Notify::new()),
            },
        ])),
        taken: Arc::clone(&taken),
    };
    let server = RunningProxy::start(
        CacheFillProxy {
            cache: &*RETRY_CACHE,
            observed: Arc::clone(&observed),
            handles_abandonment: true,
        },
        connector,
    );

    let client = send_request(server.address, path).await;
    within(
        "the first attempt did not write its request",
        first_request_written.notified(),
    )
    .await;
    drop(client);
    within(
        "downstream abandonment was not reported",
        observed.abandoned.notified(),
    )
    .await;
    fail_first.send(()).unwrap();
    within(
        "the retried request did not finish",
        observed.finished.notified(),
    )
    .await;

    assert_eq!(taken.load(Ordering::Relaxed), 2, "the proxy did not retry");
    assert_eq!(observed.callback_calls.load(Ordering::Relaxed), 1);
    assert_eq!(
        *first_calls.lock().unwrap(),
        vec!["abandon(proxy attempt failed)".to_owned()]
    );
    assert_eq!(
        *retry_calls.lock().unwrap(),
        vec!["abandon(proxy attempt failed)".to_owned()],
        "the retried attempt must be abandoned exactly once"
    );
    server.stop().await;
}

#[tokio::test]
async fn retried_attempt_without_custom_reader_abandons_once() {
    assert_retried_setup_failure_abandons_once(
        "/retry-no-reader",
        SetupFailure::NoReader,
        ErrorType::ReadError,
    )
    .await;
}

#[tokio::test]
async fn retried_attempt_without_custom_writer_abandons_once() {
    assert_retried_setup_failure_abandons_once(
        "/retry-no-writer",
        SetupFailure::NoWriter,
        ErrorType::WriteError,
    )
    .await;
}

#[tokio::test]
async fn retried_attempt_with_failed_forwarding_hook_abandons_once() {
    assert_retried_setup_failure_abandons_once(
        "/retry-forwarding-hook",
        SetupFailure::ForwardingHook,
        ErrorType::InternalError,
    )
    .await;
}
