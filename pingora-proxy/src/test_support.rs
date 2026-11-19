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
use bytes::{Bytes, BytesMut};
use pingora_cache::storage::{HandleMiss, MissCompleted, MissCompletion, MissFinishType};
use pingora_cache::trace::SpanHandle;
use pingora_cache::{
    CacheEntryId, CacheFillCancellation, CacheFillCancellationWatcher, CacheKey, CacheMeta,
    HitHandler, MemCache, MissHandler, PurgeOutcome, PurgeTarget, PurgeType, RespCacheable,
    Storage,
};
use pingora_core::protocols::http::custom::CustomMessageWrite;
use pingora_core::protocols::http::ServerSession;
use pingora_core::upstreams::peer::HttpPeer;
use pingora_error::{Error, Result};
use pingora_http::ResponseHeader;
use std::any::Any;
use std::collections::HashMap;
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc, LazyLock, Mutex,
};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};
use tokio::sync::{mpsc, oneshot, Notify};

use crate::{HttpTask, ProxyHttp, ProxyWarnLogContext, Session};

/// Cache storage whose miss writers each hold demand for their fill, as storage that stops fills
/// nobody reads does, and that counts the writers whose demand was released.
///
/// With a [`ProxyHttp::downstream_abandoned_during_cache_fill`] that releases demand, this is the
/// configuration an application rolls back to by turning the opt-in off.
pub(crate) struct WriterDemandCache {
    inner: MemCache,
    /// How many writers had their demand released, by primary key.
    released_writers: Mutex<HashMap<Vec<u8>, usize>>,
}

pub(crate) static WRITER_DEMAND_CACHE: LazyLock<WriterDemandCache> =
    LazyLock::new(|| WriterDemandCache {
        inner: MemCache::new(),
        released_writers: Mutex::default(),
    });

impl WriterDemandCache {
    /// How many writers for the cache key with this primary key had their demand released.
    pub(crate) fn released_writers(&self, primary_key: &str) -> usize {
        self.released_writers
            .lock()
            .unwrap()
            .get(primary_key.as_bytes())
            .copied()
            .unwrap_or_default()
    }
}

struct WriterDemandMissHandler {
    inner: MissHandler,
    primary_key: Vec<u8>,
    has_demand: AtomicBool,
    released_writers: &'static Mutex<HashMap<Vec<u8>, usize>>,
}

#[async_trait]
impl HandleMiss for WriterDemandMissHandler {
    async fn write_body(&mut self, data: Bytes, eof: bool) -> Result<()> {
        self.inner.write_body(data, eof).await
    }

    async fn finish(self: Box<Self>) -> Result<MissFinishType> {
        self.inner.finish().await
    }

    async fn finish_with_eviction(
        self: Box<Self>,
        completion: MissCompletion,
    ) -> Result<MissCompleted> {
        self.inner.finish_with_eviction(completion).await
    }

    fn streaming_write_tag(&self) -> Option<&[u8]> {
        self.inner.streaming_write_tag()
    }

    fn release_writer_demand(&self) -> bool {
        let released = self.has_demand.swap(false, Ordering::AcqRel);
        if released {
            *self
                .released_writers
                .lock()
                .unwrap()
                .entry(self.primary_key.clone())
                .or_default() += 1;
        }
        released
    }

    fn entry_id(&self) -> Option<CacheEntryId> {
        self.inner.entry_id()
    }
}

#[async_trait]
impl Storage for WriterDemandCache {
    async fn lookup(
        &'static self,
        key: &CacheKey,
        trace: &SpanHandle,
    ) -> Result<Option<(CacheMeta, HitHandler)>> {
        self.inner.lookup(key, trace).await
    }

    async fn lookup_streaming_write(
        &'static self,
        key: &CacheKey,
        streaming_write_tag: Option<&[u8]>,
        trace: &SpanHandle,
    ) -> Result<Option<(CacheMeta, HitHandler)>> {
        self.inner
            .lookup_streaming_write(key, streaming_write_tag, trace)
            .await
    }

    async fn get_miss_handler(
        &'static self,
        key: &CacheKey,
        meta: &CacheMeta,
        trace: &SpanHandle,
    ) -> Result<MissHandler> {
        Ok(Box::new(WriterDemandMissHandler {
            inner: self.inner.get_miss_handler(key, meta, trace).await?,
            primary_key: key.primary_key().to_vec(),
            has_demand: AtomicBool::new(true),
            released_writers: &self.released_writers,
        }))
    }

    async fn purge(
        &'static self,
        target: PurgeTarget<'_>,
        purge_type: PurgeType,
        trace: &SpanHandle,
    ) -> Result<PurgeOutcome> {
        self.inner.purge(target, purge_type, trace).await
    }

    async fn expire(
        &'static self,
        target: PurgeTarget<'_>,
        trace: &SpanHandle,
    ) -> Result<PurgeOutcome> {
        self.inner.expire(target, trace).await
    }

    async fn update_meta(
        &'static self,
        key: &CacheKey,
        meta: &CacheMeta,
        trace: &SpanHandle,
    ) -> Result<bool> {
        self.inner.update_meta(key, meta, trace).await
    }

    fn support_streaming_partial_write(&self) -> bool {
        self.inner.support_streaming_partial_write()
    }

    fn as_any(&self) -> &(dyn Any + Send + Sync + 'static) {
        self
    }
}

/// [`MemCache`] whose miss handlers offer the cancellation watcher registered for their cache key.
///
/// [`Self::cancellation_for`] returns one [`CacheFillCancellation`] per key, created on first use.
/// The same value is shared by every miss handler of one logical fill, matching the contract
/// [`HandleMiss::cancellation_watcher`] documents: it is removed only when a handler's `finish()`
/// returns `Ok`, so a handler dropped without finishing (a retried upstream attempt) leaves the
/// value in place for the next attempt to observe. There is no implicit clearing beyond that
/// rule; [`Self::reset_cancellation`] drops a value unconditionally, for a test that needs to
/// start a genuinely new logical fill after a cancelled or committed one.
pub(crate) struct CancellationWatchedStorage {
    inner: MemCache,
    cancellations: Mutex<HashMap<String, Arc<CacheFillCancellation>>>,
}

pub(crate) static CANCELLATION_WATCHED_STORAGE: LazyLock<CancellationWatchedStorage> =
    LazyLock::new(|| CancellationWatchedStorage {
        inner: MemCache::new(),
        cancellations: Mutex::new(HashMap::new()),
    });

impl CancellationWatchedStorage {
    /// The cancellation for a primary cache key, created on first use.
    pub(crate) fn cancellation_for(&self, key: &str) -> Arc<CacheFillCancellation> {
        Arc::clone(
            self.cancellations
                .lock()
                .unwrap()
                .entry(key.to_owned())
                .or_default(),
        )
    }

    /// Drops the cancellation for `key` unconditionally.
    ///
    /// Use this to start a genuinely new logical fill after a cancelled or committed one; there
    /// is no other way to clear a stored value.
    pub(crate) fn reset_cancellation(&self, key: &str) {
        self.cancellations.lock().unwrap().remove(key);
    }

    /// Removes the cancellation for `key` if it is still the value passed in.
    ///
    /// A miss handler calls this from `finish()` to drop the cancellation once its fill reaches a
    /// terminal state. The identity check protects a value a newer fill has since installed, via
    /// [`Self::reset_cancellation`] or a fresh `get_miss_handler` call, from being removed by a
    /// handler that belonged to an earlier fill.
    fn remove_if_current(&self, key: &str, cancellation: &Arc<CacheFillCancellation>) {
        let mut cancellations = self.cancellations.lock().unwrap();
        if cancellations
            .get(key)
            .is_some_and(|current| Arc::ptr_eq(current, cancellation))
        {
            cancellations.remove(key);
        }
    }

    /// Whether a completed response is cached under this primary key.
    pub(crate) async fn is_committed(&'static self, key: &str) -> bool {
        let span = pingora_cache::trace::Span::inactive();
        self.inner
            .lookup(&CacheKey::new(key, ""), &span.handle())
            .await
            .unwrap()
            .is_some()
    }
}

#[async_trait]
impl Storage for CancellationWatchedStorage {
    async fn lookup(
        &'static self,
        key: &CacheKey,
        trace: &SpanHandle,
    ) -> Result<Option<(CacheMeta, HitHandler)>> {
        self.inner.lookup(key, trace).await
    }

    async fn lookup_streaming_write(
        &'static self,
        key: &CacheKey,
        streaming_write_tag: Option<&[u8]>,
        trace: &SpanHandle,
    ) -> Result<Option<(CacheMeta, HitHandler)>> {
        self.inner
            .lookup_streaming_write(key, streaming_write_tag, trace)
            .await
    }

    async fn get_miss_handler(
        &'static self,
        key: &CacheKey,
        meta: &CacheMeta,
        trace: &SpanHandle,
    ) -> Result<MissHandler> {
        let inner = self.inner.get_miss_handler(key, meta, trace).await?;
        let primary_key = key
            .primary_key_str()
            .expect("test keys are UTF-8")
            .to_owned();
        let cancellation = self.cancellation_for(&primary_key);
        Ok(Box::new(CancellationWatchedMissHandler {
            inner,
            cancellation,
            storage: self,
            key: primary_key,
        }))
    }

    async fn purge(
        &'static self,
        target: PurgeTarget<'_>,
        purge_type: PurgeType,
        trace: &SpanHandle,
    ) -> Result<PurgeOutcome> {
        self.inner.purge(target, purge_type, trace).await
    }

    async fn expire(
        &'static self,
        target: PurgeTarget<'_>,
        trace: &SpanHandle,
    ) -> Result<PurgeOutcome> {
        self.inner.expire(target, trace).await
    }

    async fn update_meta(
        &'static self,
        key: &CacheKey,
        meta: &CacheMeta,
        trace: &SpanHandle,
    ) -> Result<bool> {
        self.inner.update_meta(key, meta, trace).await
    }

    fn support_streaming_partial_write(&self) -> bool {
        true
    }

    fn as_any(&self) -> &(dyn std::any::Any + Send + Sync + 'static) {
        self
    }
}

struct CancellationWatchedMissHandler {
    inner: MissHandler,
    cancellation: Arc<CacheFillCancellation>,
    storage: &'static CancellationWatchedStorage,
    key: String,
}

#[async_trait]
impl HandleMiss for CancellationWatchedMissHandler {
    async fn write_body(&mut self, data: Bytes, eof: bool) -> Result<()> {
        self.inner.write_body(data, eof).await
    }

    async fn finish(self: Box<Self>) -> Result<MissFinishType> {
        let CancellationWatchedMissHandler {
            inner,
            cancellation,
            storage,
            key,
        } = *self;
        let result = inner.finish().await;
        if result.is_ok() {
            storage.remove_if_current(&key, &cancellation);
        }
        result
    }

    async fn finish_with_eviction(
        self: Box<Self>,
        completion: MissCompletion,
    ) -> Result<MissCompleted> {
        let CancellationWatchedMissHandler {
            inner,
            cancellation,
            storage,
            key,
        } = *self;
        let completed = inner.finish_with_eviction(completion).await?;
        storage.remove_if_current(&key, &cancellation);
        Ok(completed)
    }

    fn streaming_write_tag(&self) -> Option<&[u8]> {
        self.inner.streaming_write_tag()
    }

    fn cancellation_watcher(&self) -> Option<CacheFillCancellationWatcher> {
        Some(self.cancellation.watcher())
    }
}

#[derive(Default)]
pub(crate) struct CustomWriteCalls {
    writes: AtomicUsize,
    finishes: AtomicUsize,
}

impl CustomWriteCalls {
    pub(crate) fn writes(&self) -> usize {
        self.writes.load(Ordering::Relaxed)
    }

    pub(crate) fn finishes(&self) -> usize {
        self.finishes.load(Ordering::Relaxed)
    }
}

pub(crate) struct RecordingCustomWriter {
    calls: Arc<CustomWriteCalls>,
}

impl RecordingCustomWriter {
    pub(crate) fn new(calls: Arc<CustomWriteCalls>) -> Self {
        Self { calls }
    }
}

#[async_trait]
impl CustomMessageWrite for RecordingCustomWriter {
    fn set_write_timeout(&mut self, _timeout: Option<Duration>) {}

    async fn write_custom_message(&mut self, _msg: Bytes) -> Result<()> {
        self.calls.writes.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    async fn finish_custom(&mut self) -> Result<()> {
        self.calls.finishes.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
}

/// What a [`HeldCustomSenderProxy`] saw.
#[derive(Default)]
pub(crate) struct HeldCustomSenderObservations {
    abandonments: AtomicUsize,
    /// Notified each time the proxy ignores a downstream failure so that the cache fill can
    /// continue.
    downstream_cache_error: Notify,
}

impl HeldCustomSenderObservations {
    pub(crate) fn abandonments(&self) -> usize {
        self.abandonments.load(Ordering::Relaxed)
    }
}

/// How many turns [`run_ready_tasks`] gives the other tasks of a test.
pub(crate) const READY_TASK_TURNS: usize = 128;

/// Give the other tasks of a `#[tokio::test]` [`READY_TASK_TURNS`] turns to act on in-memory
/// events, such as bytes written to a duplex stream, before the test goes on.
///
/// The default test runtime runs every task on the test's thread, so each yield lets every ready
/// task run once more. Unlike a sleep, this does not depend on how fast the machine is. Events
/// from real sockets or timers need their own waits. A task that keeps waking itself runs on
/// every turn, so a spin shows up as that many polls.
pub(crate) async fn run_ready_tasks() {
    for _ in 0..READY_TASK_TURNS {
        tokio::task::yield_now().await;
    }
}

/// Finish `session` as the proxy does, then read the next request from the reused connection.
///
/// Also returns the bytes `session` read that the next request starts with.
pub(crate) async fn next_session(session: Session) -> (ServerSession, Option<BytesMut>) {
    let reusable = session
        .downstream_session
        .finish()
        .await
        .unwrap()
        .expect("the connection must be reusable");
    let (stream, prefix) = reusable.into_parts();
    let mut next = ServerSession::new_http1(stream);
    if let Some(prefix) = &prefix {
        next.set_pipelined_prefix(prefix.clone());
    }
    next.read_request().await.unwrap();
    (next, prefix)
}

pub(crate) struct HeldCustomSenderProxy {
    sender: Mutex<Option<oneshot::Sender<mpsc::Sender<Bytes>>>>,
    observed: Arc<HeldCustomSenderObservations>,
}

impl HeldCustomSenderProxy {
    pub(crate) fn new() -> (
        Self,
        oneshot::Receiver<mpsc::Sender<Bytes>>,
        Arc<HeldCustomSenderObservations>,
    ) {
        let (sender, receiver) = oneshot::channel();
        let observed = Arc::new(HeldCustomSenderObservations::default());
        (
            Self {
                sender: Mutex::new(Some(sender)),
                observed: Arc::clone(&observed),
            },
            receiver,
            observed,
        )
    }
}

#[async_trait]
impl ProxyHttp for HeldCustomSenderProxy {
    type CTX = ();

    fn new_ctx(&self) -> Self::CTX {}

    async fn upstream_peer(
        &self,
        _session: &mut Session,
        _ctx: &mut Self::CTX,
    ) -> Result<Box<HttpPeer>> {
        unreachable!("test calls the transport loop directly")
    }

    async fn custom_forwarding(
        &self,
        session: &mut Session,
        _ctx: &mut Self::CTX,
        _custom_message_to_upstream: Option<mpsc::Sender<Bytes>>,
        custom_message_to_downstream: mpsc::Sender<Bytes>,
    ) -> Result<()> {
        if session.downstream_abandoned_during_cache_fill() {
            assert!(custom_message_to_downstream.is_closed());
        }
        let sender = self.sender.lock().unwrap().take().unwrap();
        assert!(sender.send(custom_message_to_downstream).is_ok());
        Ok(())
    }

    fn response_cache_filter(
        &self,
        _session: &Session,
        response: &ResponseHeader,
        _ctx: &mut (),
    ) -> Result<RespCacheable> {
        let now = std::time::SystemTime::now();
        Ok(RespCacheable::Cacheable(CacheMeta::new(
            now + Duration::from_secs(60),
            now,
            0,
            0,
            response.clone(),
        )))
    }

    fn downstream_abandoned_during_cache_fill(&self, session: &Session, _ctx: &mut ()) {
        self.observed.abandonments.fetch_add(1, Ordering::Relaxed);
        // As an application whose storage tracks writer demand does.
        session.cache.release_miss_writer_demand();
    }

    fn suppress_proxy_warn_log(
        &self,
        _session: &Session,
        _ctx: &(),
        _error: &Error,
        context: ProxyWarnLogContext,
    ) -> bool {
        if context == ProxyWarnLogContext::DownstreamCache {
            self.observed.downstream_cache_error.notify_one();
        }
        false
    }
}

/// Caches every response and signals when the proxy reports downstream abandonment.
#[derive(Default)]
pub(crate) struct AbandonmentSignalProxy {
    pub(crate) abandoned: Arc<tokio::sync::Notify>,
    pub(crate) body_delay: Option<Duration>,
    pub(crate) body_delayed: Arc<tokio::sync::Notify>,
}

#[async_trait]
impl ProxyHttp for AbandonmentSignalProxy {
    type CTX = ();

    fn new_ctx(&self) -> Self::CTX {}

    async fn upstream_peer(
        &self,
        _session: &mut Session,
        _ctx: &mut Self::CTX,
    ) -> Result<Box<HttpPeer>> {
        unreachable!("test calls the transport loop directly")
    }

    fn response_cache_filter(
        &self,
        _session: &Session,
        response: &ResponseHeader,
        _ctx: &mut (),
    ) -> Result<RespCacheable> {
        let now = std::time::SystemTime::now();
        Ok(RespCacheable::Cacheable(CacheMeta::new(
            now + Duration::from_secs(60),
            now,
            0,
            0,
            response.clone(),
        )))
    }

    async fn upstream_response_body_filter(
        &self,
        _session: &mut Session,
        body: &mut Option<Bytes>,
        _end_of_stream: bool,
        _ctx: &mut (),
    ) -> Result<Option<Duration>> {
        if body.as_ref().is_some_and(|body| !body.is_empty()) {
            if let Some(delay) = self.body_delay {
                self.body_delayed.notify_one();
                return Ok(Some(delay));
            }
        }
        Ok(None)
    }

    fn downstream_abandoned_during_cache_fill(&self, _session: &Session, _ctx: &mut ()) {
        self.abandoned.notify_one();
    }
}

pub(crate) enum AbandonmentTiming {
    BeforeAttempt,
    AfterAdmission,
}

pub(crate) async fn cache_miss_session(
    path: &str,
    abandonment: &AbandonmentTiming,
    handles_abandonment: bool,
) -> (Session, DuplexStream) {
    cache_miss_session_on(
        path,
        abandonment,
        handles_abandonment,
        &*WRITER_DEMAND_CACHE,
    )
    .await
}

/// An H1 session in the miss phase for `path` on `storage`, with the client half returned.
pub(crate) async fn cache_miss_session_on(
    path: &str,
    abandonment: &AbandonmentTiming,
    handles_abandonment: bool,
    storage: &'static (dyn Storage + Sync),
) -> (Session, DuplexStream) {
    let (mut session, client) = miss_session(
        &format!("GET {path} HTTP/1.1\r\nHost: example.com\r\n\r\n"),
        handles_abandonment,
        storage,
    )
    .await;
    if matches!(abandonment, AbandonmentTiming::BeforeAttempt) {
        assert!(session.mark_downstream_abandoned_during_cache_fill());
    }
    (session, client)
}

/// An HTTP/1 session that read `request` from its client, opted in to abandonment handling as
/// `handles_abandonment` says, and is a cache miss for the request path on
/// [`WRITER_DEMAND_CACHE`]. Returns the session and the client end of its connection.
pub(crate) async fn cache_miss_session_for_request(
    request: &str,
    handles_abandonment: bool,
) -> (Session, DuplexStream) {
    miss_session(request, handles_abandonment, &*WRITER_DEMAND_CACHE).await
}

/// An HTTP/1 session that read `request` and is a cache miss for the request path on `storage`.
async fn miss_session(
    request: &str,
    handles_abandonment: bool,
    storage: &'static (dyn Storage + Sync),
) -> (Session, DuplexStream) {
    let (mut client, server) = tokio::io::duplex(1024);
    client.write_all(request.as_bytes()).await.unwrap();

    let mut session = Session::new_h1(Box::new(server) as pingora_core::protocols::Stream);
    session.set_handles_downstream_abandonment(handles_abandonment);
    session.read_request().await.unwrap();
    session.cache.enable(storage, None, None, None, None);
    let key = CacheKey::new(session.req_header().uri.path(), "");
    session.cache.set_cache_key(key);
    session.cache.cache_miss();
    (session, client)
}

/// What a transport loop driven by [`drive_custom_sender_while_client_leaves`] did.
pub(crate) struct CustomSenderOutcome {
    /// The loop's result: whether the downstream can be reused.
    pub(crate) result: Result<bool>,
    /// Whether the loop accepted the application's message sent after the client left.
    pub(crate) late_message_accepted: bool,
    pub(crate) custom_writes: usize,
    pub(crate) custom_finishes: usize,
    pub(crate) abandonments: usize,
    pub(crate) released_writers: usize,
}

/// Drive a proxy loop's upstream side while its client leaves, with the application holding the
/// custom-message sender it got from `custom_forwarding`. `sends_done` mirrors producers that end
/// a response with a separate [`HttpTask::Done`] (HTTP/2 and custom upstreams).
///
/// With abandonment handling the loop closes that sender once the client is abandoned, and the
/// application keeps holding it. Without that handling the loop keeps writing the application's
/// messages until the application drops the sender, which it does here once the response is
/// sent. Returns whether the loop accepted a message sent after the client left, and the sender
/// if the application still holds it.
pub(crate) async fn drive_custom_sender_while_client_leaves(
    custom_sender: oneshot::Receiver<mpsc::Sender<Bytes>>,
    mut client: DuplexStream,
    response_tx: mpsc::Sender<HttpTask>,
    abandonment: AbandonmentTiming,
    sends_done: bool,
    handles_abandonment: bool,
    observed: &HeldCustomSenderObservations,
) -> (bool, Option<mpsc::Sender<Bytes>>) {
    let sender = custom_sender.await.unwrap();
    if matches!(abandonment, AbandonmentTiming::AfterAdmission) {
        let header = ResponseHeader::build(200, None).unwrap();
        response_tx
            .send(HttpTask::Header(Box::new(header), false))
            .await
            .unwrap();
        let mut response = Vec::new();
        while !response.ends_with(b"\r\n\r\n") {
            response.push(client.read_u8().await.unwrap());
        }
    }
    drop(client);
    if handles_abandonment {
        tokio::time::timeout(Duration::from_secs(1), sender.closed())
            .await
            .expect("abandonment did not release the custom-message producer");
    } else {
        tokio::time::timeout(
            Duration::from_secs(5),
            observed.downstream_cache_error.notified(),
        )
        .await
        .expect("the loop did not ignore the client's departure for the cache fill");
    }
    let late_message_accepted = sender.try_send(Bytes::from_static(b"late message")).is_ok();
    let end = match abandonment {
        AbandonmentTiming::BeforeAttempt => HttpTask::Done,
        AbandonmentTiming::AfterAdmission => {
            HttpTask::Body(Some(Bytes::from_static(b"cached body")), true)
        }
    };
    response_tx.send(end).await.unwrap();
    if sends_done && matches!(abandonment, AbandonmentTiming::AfterAdmission) {
        response_tx.send(HttpTask::Done).await.unwrap();
    }
    (late_message_accepted, handles_abandonment.then_some(sender))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal cacheable response for exercising [`Storage::get_miss_handler`] directly.
    fn test_meta() -> CacheMeta {
        let now = std::time::SystemTime::now();
        let header = ResponseHeader::build(200, None).unwrap();
        CacheMeta::new(now + Duration::from_secs(60), now, 0, 0, header)
    }

    // The cancellation for a path is shared across upstream attempts of one logical fill, and is
    // removed only once a miss handler commits - never merely by being dropped mid-attempt.
    #[tokio::test]
    async fn cancellation_survives_a_retry_and_clears_on_commit() {
        let path = "/test-support-lifecycle-commit";
        CANCELLATION_WATCHED_STORAGE.reset_cancellation(path);
        let key = CacheKey::new(path, "");
        let span = pingora_cache::trace::Span::inactive();

        let first = CANCELLATION_WATCHED_STORAGE.cancellation_for(path);
        assert!(
            !first.is_cancelled(),
            "a fresh cancellation must not start cancelled"
        );

        // A miss handler dropped without finish() models a retried upstream attempt: the
        // fixture must leave the cancellation in place for that retry to observe.
        drop(
            CANCELLATION_WATCHED_STORAGE
                .get_miss_handler(&key, &test_meta(), &span.handle())
                .await
                .unwrap(),
        );
        let after_drop = CANCELLATION_WATCHED_STORAGE.cancellation_for(path);
        assert!(
            Arc::ptr_eq(&first, &after_drop),
            "a handler dropped without finish() must not clear the cancellation"
        );

        // The retry's own handler commits successfully: the fill has reached a terminal state
        // and the value must be gone, so the next logical fill of this path starts fresh.
        CANCELLATION_WATCHED_STORAGE
            .get_miss_handler(&key, &test_meta(), &span.handle())
            .await
            .unwrap()
            .finish()
            .await
            .unwrap();
        let after_commit = CANCELLATION_WATCHED_STORAGE.cancellation_for(path);
        assert!(
            !Arc::ptr_eq(&first, &after_commit),
            "a committed fill's cancellation must be replaced by a fresh value"
        );
        assert!(!after_commit.is_cancelled());
    }

    #[tokio::test]
    async fn reset_cancellation_replaces_a_cancelled_value() {
        let path = "/test-support-lifecycle-reset";
        CANCELLATION_WATCHED_STORAGE.reset_cancellation(path);

        let cancelled = CANCELLATION_WATCHED_STORAGE.cancellation_for(path);
        assert!(cancelled.cancel());

        CANCELLATION_WATCHED_STORAGE.reset_cancellation(path);
        let fresh = CANCELLATION_WATCHED_STORAGE.cancellation_for(path);
        assert!(
            !Arc::ptr_eq(&cancelled, &fresh),
            "reset_cancellation must install a new value"
        );
        assert!(
            !fresh.is_cancelled(),
            "the value after reset must be uncancelled"
        );
    }
}
