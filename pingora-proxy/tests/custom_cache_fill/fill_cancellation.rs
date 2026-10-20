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

//! Cancelling an abandoned cache fill whose upstream is a custom session.

use super::*;
use pingora_cache::fill_cancellation::ERR_CACHE_FILL_CANCELLED;
use pingora_cache::storage::{HandleMiss, MissCompleted, MissCompletion, MissFinishType};
use pingora_cache::CacheFillCancellationWatcher;
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use upstream::ProducerOutcome;

/// [`MemCache`] whose miss handlers offer the cancellation watcher registered for their request path.
///
/// Each entry is removed once the fixture observes the fill reach a terminal state, which is the
/// miss handler's [`finish`](HandleMiss::finish) returning `Ok`, and only if the entry still
/// holds the same cancellation the handler was created with: a fill that finished after a newer
/// logical fill installed its own value leaves that newer value intact. A miss handler dropped
/// without `finish` leaves its entry in place, so a retried upstream attempt of the same logical
/// fill still shares one value; [`reset_cancellation`](Self::reset_cancellation) drops an entry
/// unconditionally so a test can start a genuinely new logical fill.
struct CancellationWatchedCache {
    inner: MemCache,
    cancellations: Mutex<HashMap<String, Arc<CacheFillCancellation>>>,
}

static CANCELLATION_WATCHED_CACHE: LazyLock<CancellationWatchedCache> =
    LazyLock::new(|| CancellationWatchedCache {
        inner: MemCache::new(),
        cancellations: Mutex::new(HashMap::new()),
    });

impl CancellationWatchedCache {
    /// The cancellation for a path, created on first use.
    fn cancellation_for(&self, path: &str) -> Arc<CacheFillCancellation> {
        Arc::clone(
            self.cancellations
                .lock()
                .unwrap()
                .entry(path.to_owned())
                .or_default(),
        )
    }

    /// Drop the entry for `path` unconditionally, so a test can start a genuinely new logical fill.
    fn reset_cancellation(&self, path: &str) {
        self.cancellations.lock().unwrap().remove(path);
    }

    /// Remove `path`'s entry only if it still holds `expected`, so a fill that finished after a
    /// newer logical fill installed its own value never deletes that newer value.
    fn remove_cancellation_if(&self, path: &str, expected: &Arc<CacheFillCancellation>) {
        let mut cancellations = self.cancellations.lock().unwrap();
        if cancellations
            .get(path)
            .is_some_and(|stored| Arc::ptr_eq(stored, expected))
        {
            cancellations.remove(path);
        }
    }
}

#[async_trait]
impl Storage for CancellationWatchedCache {
    async fn lookup(
        &'static self,
        key: &CacheKey,
        trace: &pingora_cache::trace::SpanHandle,
    ) -> Result<Option<(CacheMeta, pingora_cache::HitHandler)>> {
        self.inner.lookup(key, trace).await
    }

    async fn lookup_streaming_write(
        &'static self,
        key: &CacheKey,
        streaming_write_tag: Option<&[u8]>,
        trace: &pingora_cache::trace::SpanHandle,
    ) -> Result<Option<(CacheMeta, pingora_cache::HitHandler)>> {
        self.inner
            .lookup_streaming_write(key, streaming_write_tag, trace)
            .await
    }

    async fn get_miss_handler(
        &'static self,
        key: &CacheKey,
        meta: &CacheMeta,
        trace: &pingora_cache::trace::SpanHandle,
    ) -> Result<pingora_cache::MissHandler> {
        let inner = self.inner.get_miss_handler(key, meta, trace).await?;
        let path = key.primary_key_str().expect("test cache keys are paths");
        let cancellation = self.cancellation_for(path);
        Ok(Box::new(CancellationWatchedMissHandler {
            inner,
            cache: self,
            path: path.to_owned(),
            cancellation,
        }))
    }

    async fn purge(
        &'static self,
        target: pingora_cache::PurgeTarget<'_>,
        purge_type: pingora_cache::PurgeType,
        trace: &pingora_cache::trace::SpanHandle,
    ) -> Result<pingora_cache::PurgeOutcome> {
        self.inner.purge(target, purge_type, trace).await
    }

    async fn update_meta(
        &'static self,
        key: &CacheKey,
        meta: &CacheMeta,
        trace: &pingora_cache::trace::SpanHandle,
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
    inner: pingora_cache::MissHandler,
    cache: &'static CancellationWatchedCache,
    path: String,
    cancellation: Arc<CacheFillCancellation>,
}

#[async_trait]
impl HandleMiss for CancellationWatchedMissHandler {
    async fn write_body(&mut self, data: Bytes, eof: bool) -> Result<()> {
        self.inner.write_body(data, eof).await
    }

    async fn finish(self: Box<Self>) -> Result<MissFinishType> {
        let result = self.inner.finish().await;
        if result.is_ok() {
            self.cache
                .remove_cancellation_if(&self.path, &self.cancellation);
        }
        result
    }

    async fn finish_with_eviction(
        self: Box<Self>,
        completion: MissCompletion,
    ) -> Result<MissCompleted> {
        let completed = self.inner.finish_with_eviction(completion).await?;
        self.cache
            .remove_cancellation_if(&self.path, &self.cancellation);
        Ok(completed)
    }

    fn streaming_write_tag(&self) -> Option<&[u8]> {
        self.inner.streaming_write_tag()
    }

    fn cancellation_watcher(&self) -> Option<CacheFillCancellationWatcher> {
        Some(self.cancellation.watcher())
    }

    fn entry_id(&self) -> Option<pingora_cache::CacheEntryId> {
        self.inner.entry_id()
    }
}

#[derive(Clone, Copy)]
enum CancellationTiming {
    /// The abandonment callback requests the cancellation.
    InHook,
    /// The test requests the cancellation after the callback ran.
    AfterAbandonment,
    /// Nobody cancels; the watched fill must still run to completion.
    Never,
}

impl CancellationTiming {
    fn path(self) -> &'static str {
        match self {
            Self::InHook => "/custom-cache-fill-cancel-in-hook",
            Self::AfterAbandonment => "/custom-cache-fill-cancel-after-abandonment",
            Self::Never => "/custom-cache-fill-watched-without-cancellation",
        }
    }
}

async fn custom_fill_cancellation(timing: CancellationTiming) {
    let path = timing.path();
    let cancellation = CANCELLATION_WATCHED_CACHE.cancellation_for(path);
    assert!(!cancellation.is_cancelled(), "{path} was not fresh");
    let observed = Arc::new(Observations {
        expected_error: match timing {
            CancellationTiming::Never => None,
            CancellationTiming::InHook | CancellationTiming::AfterAbandonment => {
                Some(ERR_CACHE_FILL_CANCELLED)
            }
        },
        cancel_in_hook: matches!(timing, CancellationTiming::InHook)
            .then(|| Arc::clone(&cancellation)),
        ..Default::default()
    });
    let upstream = CustomUpstream::new();
    let abandon_calls = Arc::clone(&upstream.abandon_calls);
    let request_headers_written = Arc::clone(&upstream.request_headers_written);
    let server = RunningProxy::start(
        CacheFillProxy {
            cache: &*CANCELLATION_WATCHED_CACHE,
            observed: observed.clone(),
            handles_abandonment: true,
        },
        upstream.connector,
    );
    let (release, released) = oneshot::channel();
    let producer = tokio::spawn(upstream.producer.run_watching_reader(released));
    let mut client = within(
        "HTTP client did not connect",
        TcpStream::connect(server.address),
    )
    .await
    .unwrap();
    within(
        "HTTP request was not written",
        client.write_all(
            format!("GET {path} HTTP/1.1\r\nHost: example.com\r\nConnection: close\r\n\r\n")
                .as_bytes(),
        ),
    )
    .await
    .unwrap();
    let mut client = BufReader::new(client);
    within(
        "initial response was not delivered",
        read_initial_response(&mut client),
    )
    .await;
    within(
        "initial custom message was not filtered",
        observed.first_custom_message.notified(),
    )
    .await;
    assert!(observed.initial_admission_active.load(Ordering::Relaxed));

    drop(client);
    within(
        "downstream abandonment was not reported",
        observed.abandoned.notified(),
    )
    .await;
    match timing {
        CancellationTiming::InHook => assert!(cancellation.is_cancelled()),
        CancellationTiming::AfterAbandonment => {
            assert!(
                !observed.completed.load(Ordering::Acquire),
                "the fill ended before anyone cancelled it"
            );
            assert!(cancellation.cancel());
        }
        CancellationTiming::Never => release.send(()).unwrap(),
    }

    let outcome = within("custom producer did not observe the proxy", producer)
        .await
        .unwrap();
    within(
        "custom cache fill did not complete",
        observed.finished.notified(),
    )
    .await;
    assert_eq!(observed.callback_calls.load(Ordering::Relaxed), 1);
    assert_eq!(observed.custom_messages.load(Ordering::Relaxed), 1);
    // `abandon_calls` is a `std::sync::Mutex`, so the guard is scoped to this block: the
    // assertions below it await, and a std guard must not be live across an await point.
    {
        let abandon_calls = abandon_calls.lock().unwrap();
        match timing {
            CancellationTiming::Never => {
                assert!(
                    abandon_calls.is_empty(),
                    "a clean custom attempt must not abandon its session: {abandon_calls:?}"
                );
            }
            CancellationTiming::InHook | CancellationTiming::AfterAbandonment => {
                assert_eq!(
                    request_headers_written.load(Ordering::Relaxed),
                    1,
                    "the cancelled attempt fails only after writing its request"
                );
                assert_eq!(
                    abandon_calls.len(),
                    1,
                    "the errored custom attempt must abandon its session exactly once"
                );
                assert!(
                    abandon_calls[0].starts_with("abandon("),
                    "the attempt must signal abandon, not shutdown: {}",
                    abandon_calls[0]
                );
            }
        }
    }
    match timing {
        CancellationTiming::Never => {
            assert_eq!(
                outcome,
                ProducerOutcome::Released {
                    reader_closed: true
                }
            );
            assert_eq!(
                observed.body_bytes.load(Ordering::Relaxed),
                FIRST_BODY.len() + LAST_BODY.len()
            );
            assert_eq!(
                read_cached_body(&CANCELLATION_WATCHED_CACHE.inner, path)
                    .await
                    .expect("watched fill without a cancellation was not cached"),
                [FIRST_BODY, LAST_BODY].concat()
            );
        }
        CancellationTiming::InHook | CancellationTiming::AfterAbandonment => {
            // The cancellation ended the upstream read while the producer was still holding the rest.
            assert_eq!(outcome, ProducerOutcome::BodyReaderDropped);
            assert_eq!(
                observed.body_bytes.load(Ordering::Relaxed),
                FIRST_BODY.len()
            );
            assert!(
                read_cached_body(&CANCELLATION_WATCHED_CACHE.inner, path)
                    .await
                    .is_none(),
                "a cancelled fill was committed to the cache"
            );
        }
    }
    server.stop().await;
}

#[tokio::test]
async fn custom_fill_cancel_in_hook_ends_stalled_fill() {
    custom_fill_cancellation(CancellationTiming::InHook).await;
}

#[tokio::test]
async fn custom_fill_cancel_after_abandonment_ends_stalled_fill() {
    custom_fill_cancellation(CancellationTiming::AfterAbandonment).await;
}

#[tokio::test]
async fn custom_watched_fill_without_cancellation_completes() {
    custom_fill_cancellation(CancellationTiming::Never).await;
}

/// A cancellation requested while the downstream is still connected and reading must be ignored:
/// watching only starts once the downstream is abandoned, so the client receives the complete
/// response, no abandonment hook runs, and the fill commits.
#[tokio::test]
async fn custom_fill_cancel_while_downstream_connected_commits() {
    let path = "/custom-cache-fill-cancel-while-connected";
    CANCELLATION_WATCHED_CACHE.reset_cancellation(path);
    let cancellation = CANCELLATION_WATCHED_CACHE.cancellation_for(path);
    assert!(!cancellation.is_cancelled(), "{path} was not fresh");
    let observed = Arc::new(Observations::default());
    let upstream = CustomUpstream::new();
    let abandon_calls = Arc::clone(&upstream.abandon_calls);
    let server = RunningProxy::start(
        CacheFillProxy {
            cache: &*CANCELLATION_WATCHED_CACHE,
            observed: observed.clone(),
            handles_abandonment: true,
        },
        upstream.connector,
    );
    let (release, released) = oneshot::channel();
    let producer = tokio::spawn(upstream.producer.run(released));
    let mut client = within(
        "HTTP client did not connect",
        TcpStream::connect(server.address),
    )
    .await
    .unwrap();
    within(
        "HTTP request was not written",
        client.write_all(
            format!("GET {path} HTTP/1.1\r\nHost: example.com\r\nConnection: close\r\n\r\n")
                .as_bytes(),
        ),
    )
    .await
    .unwrap();
    let mut client = BufReader::new(client);
    within(
        "initial response was not delivered",
        read_initial_response(&mut client),
    )
    .await;
    within(
        "initial custom message was not filtered",
        observed.first_custom_message.notified(),
    )
    .await;
    assert!(observed.initial_admission_active.load(Ordering::Relaxed));

    // The downstream is still attached and reading: the abandonment gate has not opened, so the
    // cancellation must leave the watcher off and the fill must run to completion.
    assert!(cancellation.cancel());

    release.send(()).unwrap();
    let mut body = Vec::new();
    within(
        "connected client did not receive EOF",
        client.read_to_end(&mut body),
    )
    .await
    .unwrap();
    assert_eq!(body, LAST_BODY);

    let reader_closed = within(
        "custom producer is blocked on a live receiver before the final HTTP body",
        producer,
    )
    .await
    .unwrap();
    assert!(!reader_closed);
    within(
        "custom cache fill did not complete",
        observed.finished.notified(),
    )
    .await;
    assert_eq!(observed.callback_calls.load(Ordering::Relaxed), 0);
    assert_eq!(
        observed.custom_messages.load(Ordering::Relaxed),
        1 + LATE_MESSAGE_COUNT
    );
    assert_eq!(
        observed.body_bytes.load(Ordering::Relaxed),
        FIRST_BODY.len() + LAST_BODY.len()
    );
    assert!(
        abandon_calls.lock().unwrap().is_empty(),
        "a completed custom attempt must not abandon its session"
    );
    let cached_body = read_cached_body(&CANCELLATION_WATCHED_CACHE.inner, path)
        .await
        .expect("completed custom response was not cached");
    assert_eq!(cached_body, [FIRST_BODY, LAST_BODY].concat());
    server.stop().await;
}

/// A complete response whose final tasks were already handed to the transport loop commits even
/// though a cancellation is pending, mirroring the H1 batched-response guarantee.
///
/// Synchronisation: once the downstream is abandoned the transport loop parks in `recv_upstream`.
/// Releasing the producer makes the pipe read the final body chunk, queue its `Body` task, and
/// then - in the same non-yielding poll, after reading the terminal `None` - queue `Done`. The
/// `body_drained` notification fires on that terminal read, so once the test observes it the
/// complete response is already handed over. `recv_upstream`'s biased select then always prefers
/// a queued task over the pending cancellation, so the response is never discarded.
#[tokio::test]
async fn custom_batched_complete_response_commits_despite_pending_cancellation() {
    let path = "/custom-cache-fill-batched-commit";
    CANCELLATION_WATCHED_CACHE.reset_cancellation(path);
    let cancellation = CANCELLATION_WATCHED_CACHE.cancellation_for(path);
    assert!(!cancellation.is_cancelled(), "{path} was not fresh");
    let observed = Arc::new(Observations::default());
    let upstream = CustomUpstream::new();
    let abandon_calls = Arc::clone(&upstream.abandon_calls);
    let body_drained = Arc::clone(&upstream.body_drained);
    let server = RunningProxy::start(
        CacheFillProxy {
            cache: &*CANCELLATION_WATCHED_CACHE,
            observed: observed.clone(),
            handles_abandonment: true,
        },
        upstream.connector,
    );
    let (release, released) = oneshot::channel();
    let producer = tokio::spawn(upstream.producer.run_watching_reader(released));
    let mut client = within(
        "HTTP client did not connect",
        TcpStream::connect(server.address),
    )
    .await
    .unwrap();
    within(
        "HTTP request was not written",
        client.write_all(
            format!("GET {path} HTTP/1.1\r\nHost: example.com\r\nConnection: close\r\n\r\n")
                .as_bytes(),
        ),
    )
    .await
    .unwrap();
    let mut client = BufReader::new(client);
    within(
        "initial response was not delivered",
        read_initial_response(&mut client),
    )
    .await;
    within(
        "initial custom message was not filtered",
        observed.first_custom_message.notified(),
    )
    .await;
    assert!(observed.initial_admission_active.load(Ordering::Relaxed));

    drop(client);
    within(
        "downstream abandonment was not reported",
        observed.abandoned.notified(),
    )
    .await;

    // Deliver the rest of the response before requesting the cancellation, so the final tasks are
    // handed over while the cancellation is still pending.
    release.send(()).unwrap();
    within("final body was not consumed", body_drained.notified()).await;
    assert!(cancellation.cancel());

    let outcome = within("custom producer did not observe the proxy", producer)
        .await
        .unwrap();
    within(
        "custom cache fill did not complete",
        observed.finished.notified(),
    )
    .await;
    assert_eq!(observed.callback_calls.load(Ordering::Relaxed), 1);
    assert_eq!(observed.custom_messages.load(Ordering::Relaxed), 1);
    assert_eq!(
        outcome,
        ProducerOutcome::Released {
            reader_closed: true
        }
    );
    assert_eq!(
        observed.body_bytes.load(Ordering::Relaxed),
        FIRST_BODY.len() + LAST_BODY.len()
    );
    // The upstream attempt completed cleanly despite the pending cancellation, so the session is
    // released without an abandon signal.
    assert!(
        abandon_calls.lock().unwrap().is_empty(),
        "a completed custom attempt must not abandon its session"
    );
    let cached_body = read_cached_body(&CANCELLATION_WATCHED_CACHE.inner, path)
        .await
        .expect("completed custom response was not cached");
    assert_eq!(cached_body, [FIRST_BODY, LAST_BODY].concat());
    server.stop().await;
}

/// Without the opt-in the proxy never watches for a cancellation: the abandonment hook that would
/// cancel does not run, a cancellation requested anyway is ignored, the fill commits, and the
/// failed attempt releases its session without an abandon signal, as before cancellation existed.
#[tokio::test]
async fn custom_fill_cancel_ignored_without_abandonment_handling() {
    let path = "/custom-cache-fill-cancel-without-abandonment-handling";
    CANCELLATION_WATCHED_CACHE.reset_cancellation(path);
    let cancellation = CANCELLATION_WATCHED_CACHE.cancellation_for(path);
    assert!(!cancellation.is_cancelled(), "{path} was not fresh");
    let observed = Arc::new(Observations {
        // Without abandonment handling, the loop ends in the error the downstream left.
        expected_error: Some(ErrorType::WriteError),
        cancel_in_hook: Some(Arc::clone(&cancellation)),
        ..Default::default()
    });
    let upstream = CustomUpstream::new();
    let abandon_calls = Arc::clone(&upstream.abandon_calls);
    let server = RunningProxy::start(
        CacheFillProxy {
            cache: &*CANCELLATION_WATCHED_CACHE,
            observed: observed.clone(),
            handles_abandonment: false,
        },
        upstream.connector,
    );
    let (release, released) = oneshot::channel();
    let producer = tokio::spawn(upstream.producer.run(released));
    let mut client = within(
        "HTTP client did not connect",
        TcpStream::connect(server.address),
    )
    .await
    .unwrap();
    within(
        "HTTP request was not written",
        client.write_all(
            format!("GET {path} HTTP/1.1\r\nHost: example.com\r\nConnection: close\r\n\r\n")
                .as_bytes(),
        ),
    )
    .await
    .unwrap();
    let mut client = BufReader::new(client);
    within(
        "initial response was not delivered",
        read_initial_response(&mut client),
    )
    .await;
    within(
        "initial custom message was not filtered",
        observed.first_custom_message.notified(),
    )
    .await;

    drop(client);
    within(
        "the proxy did not ignore the downstream error for the cache fill",
        observed.downstream_error_ignored.notified(),
    )
    .await;
    assert!(
        cancellation.cancel(),
        "the abandonment hook ran for a request that did not opt in"
    );
    release.send(()).unwrap();

    let reader_closed = within("custom producer did not finish the response", producer)
        .await
        .unwrap();
    assert!(!reader_closed);
    within(
        "custom cache fill did not complete",
        observed.finished.notified(),
    )
    .await;
    assert_eq!(observed.callback_calls.load(Ordering::Relaxed), 0);
    assert_eq!(
        observed.custom_messages.load(Ordering::Relaxed),
        1 + LATE_MESSAGE_COUNT
    );
    assert_eq!(
        observed.body_bytes.load(Ordering::Relaxed),
        FIRST_BODY.len() + LAST_BODY.len()
    );
    assert!(
        abandon_calls.lock().unwrap().is_empty(),
        "a failed attempt that did not opt in must not abandon its session"
    );
    let cached_body = read_cached_body(&CANCELLATION_WATCHED_CACHE.inner, path)
        .await
        .expect("a fill that did not opt in was not cached");
    assert_eq!(cached_body, [FIRST_BODY, LAST_BODY].concat());
    server.stop().await;
}
