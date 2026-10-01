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
use pingora_cache::storage::{HandleMiss, MissFinishType};
use pingora_cache::trace::SpanHandle;
use pingora_cache::{
    CacheEntryId, CacheKey, CacheMeta, HitHandler, MemCache, MissHandler, PurgeOutcome,
    PurgeTarget, PurgeType, RespCacheable, Storage,
};
use pingora_core::protocols::http::custom::CustomMessageWrite;
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

pub(crate) enum AbandonmentTiming {
    BeforeAttempt,
    AfterAdmission,
}

pub(crate) async fn cache_miss_session(
    path: &str,
    abandonment: &AbandonmentTiming,
    handles_abandonment: bool,
) -> (Session, DuplexStream) {
    let (mut session, client) = cache_miss_session_for_request(
        &format!("GET {path} HTTP/1.1\r\nHost: example.com\r\n\r\n"),
        handles_abandonment,
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
    let (mut client, server) = tokio::io::duplex(1024);
    client.write_all(request.as_bytes()).await.unwrap();

    let mut session = Session::new_h1(Box::new(server) as pingora_core::protocols::Stream);
    session.set_handles_downstream_abandonment(handles_abandonment);
    session.read_request().await.unwrap();
    session
        .cache
        .enable(&*WRITER_DEMAND_CACHE, None, None, None, None);
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
