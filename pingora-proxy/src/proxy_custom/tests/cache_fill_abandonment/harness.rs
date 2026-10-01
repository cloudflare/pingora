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
use std::sync::atomic::AtomicBool;

type FilterMessage = (Bytes, oneshot::Sender<Option<Bytes>>);

pub(super) struct CustomLoop {
    tasks: mpsc::Receiver<HttpTask>,
    upstream_messages: mpsc::Receiver<FilterMessage>,
    downstream_messages: mpsc::Receiver<FilterMessage>,
    cancel_downstream_reader: oneshot::Sender<()>,
    cancel_upstream_reader: oneshot::Sender<()>,
    pipe_state: Arc<AtomicU8>,
}

pub(super) struct CustomLoopDriver {
    pub tasks: mpsc::Sender<HttpTask>,
    pub upstream_messages: mpsc::Sender<FilterMessage>,
    pub downstream_messages: mpsc::Sender<FilterMessage>,
    pub downstream_reader_cancelled: oneshot::Receiver<()>,
    pub upstream_reader_cancelled: oneshot::Receiver<()>,
    pub pipe_state: Arc<AtomicU8>,
}

impl CustomLoop {
    pub fn new() -> (Self, CustomLoopDriver) {
        let (task_tx, task_rx) = mpsc::channel(TASK_BUFFER_SIZE);
        let (upstream_tx, upstream_rx) = mpsc::channel(1);
        let (downstream_tx, downstream_rx) = mpsc::channel(1);
        let (cancel_downstream_tx, cancel_downstream_rx) = oneshot::channel();
        let (cancel_upstream_tx, cancel_upstream_rx) = oneshot::channel();
        let pipe_state = Arc::new(AtomicU8::new(PipeState::Active as u8));
        (
            Self {
                tasks: task_rx,
                upstream_messages: upstream_rx,
                downstream_messages: downstream_rx,
                cancel_downstream_reader: cancel_downstream_tx,
                cancel_upstream_reader: cancel_upstream_tx,
                pipe_state: Arc::clone(&pipe_state),
            },
            CustomLoopDriver {
                tasks: task_tx,
                upstream_messages: upstream_tx,
                downstream_messages: downstream_tx,
                downstream_reader_cancelled: cancel_downstream_rx,
                upstream_reader_cancelled: cancel_upstream_rx,
                pipe_state,
            },
        )
    }

    /// Run `proxy`'s transport loop for `session` over these channels, uploading the request
    /// body to `request_body`, with the loop's share of the cancel senders that the proxy gives it.
    pub async fn run<SV>(
        self,
        proxy: &HttpProxy<SV>,
        session: &mut Session,
        mut request_body: Box<dyn BodyWrite>,
    ) -> Result<bool>
    where
        SV: ProxyHttp + Send + Sync,
        SV::CTX: Send + Sync,
    {
        let (loop_cancel_upstream_reader, _attempt_cancel_upstream_reader) =
            split_cancel_upstream_reader(
                session.handles_downstream_abandonment(),
                self.cancel_upstream_reader,
            );
        let mut ctx = proxy.inner.new_ctx();
        proxy
            .custom_bidirection_down_to_up(
                session,
                &mut request_body,
                self.tasks,
                &mut ctx,
                self.upstream_messages,
                self.downstream_messages,
                false,
                self.cancel_downstream_reader,
                loop_cancel_upstream_reader,
                self.pipe_state,
            )
            .await
    }
}

/// The entry the cache holds for `path`, as its metadata and whole body.
pub(super) async fn cached_response(path: &str) -> Option<(CacheMeta, Vec<u8>)> {
    let key = CacheKey::new(path, "");
    let trace = Span::inactive();
    let (meta, mut hit_handler) = WRITER_DEMAND_CACHE
        .lookup(&key, &trace.handle())
        .await
        .unwrap()?;
    let mut body = Vec::new();
    while let Some(chunk) = hit_handler.read_body().await.unwrap() {
        body.extend_from_slice(&chunk);
    }
    hit_handler
        .finish(&*WRITER_DEMAND_CACHE, &key, &trace.handle())
        .await
        .unwrap();
    Some((meta, body))
}

pub(super) fn cacheable_response_header() -> HttpTask {
    let mut response = ResponseHeader::build(200, Some(1)).unwrap();
    response
        .insert_header(http::header::CACHE_CONTROL, "max-age=60")
        .unwrap();
    HttpTask::Header(Box::new(response), false)
}

/// What a custom upstream sends after the body task that ends its response.
#[derive(Clone, Copy)]
pub(super) enum AfterBodyEnd {
    /// A trailer, then `Done`, as the producer does after reading trailers.
    TrailerThenDone,
    /// `Failed`, as the producer does when reading what follows the body fails.
    Failed,
}

/// Run a streaming cache miss for a client that stays connected and reads the whole response:
/// the upstream sends a header and the body task that ends its response, then `after_body_end`,
/// then closes its task channel. Checks that the loop is still running at each step, then
/// returns its result and how many trailers it filtered.
pub(super) async fn run_connected_miss(
    path: &str,
    handles_abandonment: bool,
    after_body_end: AfterBodyEnd,
) -> (Result<bool>, usize) {
    const BODY: &[u8] = b"cached body";

    let (mut session, mut client) = cache_miss_session_for_request(
        &format!("GET {path} HTTP/1.1\r\nHost: example.com\r\n\r\n"),
        handles_abandonment,
    )
    .await;
    let observed = CacheFillCallbackProxy::default();
    let proxy = HttpProxy::new(observed.clone(), Arc::new(ServerConf::default()));
    let (channels, CustomLoopDriver { tasks: task_tx, .. }) = CustomLoop::new();
    let loop_finished = AtomicBool::new(false);

    let proxy_loop = async {
        let result = channels
            .run(&proxy, &mut session, Box::new(NoopBodyWriter))
            .await;
        loop_finished.store(true, Ordering::Relaxed);
        result
    };
    let drive = async {
        let mut header = ResponseHeader::build(200, None).unwrap();
        header
            .insert_header(http::header::CACHE_CONTROL, "max-age=60")
            .unwrap();
        header
            .insert_header(http::header::CONTENT_LENGTH, BODY.len())
            .unwrap();
        task_tx
            .send(HttpTask::Header(Box::new(header), false))
            .await
            .unwrap();
        task_tx
            .send(HttpTask::Body(Some(Bytes::from_static(BODY)), true))
            .await
            .unwrap();
        let mut head = Vec::new();
        while !head.ends_with(b"\r\n\r\n") {
            head.push(client.read_u8().await.unwrap());
        }
        assert!(head.starts_with(b"HTTP/1.1 200"));
        let mut body = [0; BODY.len()];
        client.read_exact(&mut body).await.unwrap();
        assert_eq!(body, BODY);
        assert!(
            !loop_finished.load(Ordering::Relaxed),
            "the loop ended at the body end, before the rest of the upstream response"
        );

        match after_body_end {
            AfterBodyEnd::TrailerThenDone => {
                task_tx
                    .send(HttpTask::Trailer(Some(Box::default())))
                    .await
                    .unwrap();
                task_tx.send(HttpTask::Done).await.unwrap();
                tokio::time::timeout(
                    Duration::from_secs(5),
                    observed.trailer_filter_notify.notified(),
                )
                .await
                .expect("the trailer after the body end was not filtered");
                assert!(
                    !loop_finished.load(Ordering::Relaxed),
                    "the loop ended before its upstream task channel closed"
                );
            }
            AfterBodyEnd::Failed => {
                task_tx
                    .send(HttpTask::Failed(Error::explain(
                        ReadError,
                        "injected upstream failure after the body end",
                    )))
                    .await
                    .unwrap();
            }
        }
        drop(task_tx);
    };

    let (result, ()) = tokio::time::timeout(Duration::from_secs(10), async {
        tokio::join!(proxy_loop, drive)
    })
    .await
    .expect("custom proxy loop did not finish");
    assert_eq!(count(&observed.calls), 0);
    assert_eq!(WRITER_DEMAND_CACHE.released_writers(path), 0);
    (result, count(&observed.trailer_filter_calls))
}
