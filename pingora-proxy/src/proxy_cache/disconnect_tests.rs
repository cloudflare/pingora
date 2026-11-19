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
use crate::test_support::{next_session, run_ready_tasks};
use bytes::BytesMut;
use pingora_cache::{storage::HandleHit, CacheKey, CacheMeta, HitStatus, MemCache};
use pingora_error::ErrorSource;
use std::any::Any;
use std::sync::{Arc, LazyLock};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::sync::Notify;

static STORAGE: LazyLock<MemCache> = LazyLock::new(MemCache::new);

/// Make `hit` the fresh cache hit that `session` serves, with a 4-byte cached body.
fn cache_found(session: &mut Session, key: &str, hit: impl HandleHit + Send + Sync + 'static) {
    session.cache.enable(&*STORAGE, None, None, None, None);
    session.cache.set_cache_key(CacheKey::new(key, ""));
    let now = SystemTime::now();
    let mut header = ResponseHeader::build(200, None).unwrap();
    header.insert_header("Content-Length", "4").unwrap();
    session.cache.cache_found(
        CacheMeta::new(now + Duration::from_secs(60), now, 0, 0, header),
        Box::new(hit),
        HitStatus::Fresh,
    );
}

/// Read from `client` until it holds a whole response carrying the 4-byte cached body.
async fn read_cached_response(client: &mut (impl AsyncRead + Unpin)) -> Vec<u8> {
    let mut response = Vec::new();
    while !response.ends_with(b"\r\n\r\nbody") {
        let mut buf = [0; 1024];
        let read = client.read(&mut buf).await.unwrap();
        assert_ne!(read, 0, "the response ended early: {response:?}");
        response.extend_from_slice(&buf[..read]);
    }
    response
}

struct StalledHit {
    reading: Arc<Notify>,
    dropped: Arc<Notify>,
}

#[async_trait]
impl HandleHit for StalledHit {
    async fn read_body(&mut self) -> Result<Option<Bytes>> {
        self.reading.notify_one();
        std::future::pending().await
    }

    async fn finish(
        self: Box<Self>,
        _storage: &'static (dyn pingora_cache::storage::Storage + Sync),
        _key: &CacheKey,
        _trace: &pingora_cache::trace::SpanHandle,
    ) -> Result<()> {
        Ok(())
    }

    fn as_any(&self) -> &(dyn Any + Send + Sync) {
        self
    }

    fn as_any_mut(&mut self) -> &mut (dyn Any + Send + Sync) {
        self
    }
}

impl Drop for StalledHit {
    fn drop(&mut self) {
        self.dropped.notify_one();
    }
}

struct CacheHitProxy;

#[async_trait]
impl ProxyHttp for CacheHitProxy {
    type CTX = ();

    fn new_ctx(&self) -> Self::CTX {}

    async fn upstream_peer(
        &self,
        _session: &mut Session,
        _ctx: &mut Self::CTX,
    ) -> Result<Box<HttpPeer>> {
        unreachable!("this test serves a cache hit")
    }
}

#[tokio::test]
async fn client_disconnect_drops_a_stalled_cache_reader() {
    let (mut client, server) = tokio::io::duplex(1024);
    client
        .write_all(b"GET / HTTP/1.1\r\nHost: test\r\n\r\n")
        .await
        .unwrap();
    let mut session = Session::new_h1(Box::new(server));
    session.handles_downstream_abandonment = true;
    session.read_request().await.unwrap();
    let reading = Arc::new(Notify::new());
    let dropped = Arc::new(Notify::new());
    cache_found(
        &mut session,
        "stalled",
        StalledHit {
            reading: reading.clone(),
            dropped: dropped.clone(),
        },
    );
    let proxy = HttpProxy::new(CacheHitProxy, Arc::new(ServerConf::default()));
    let serve = tokio::spawn(async move { proxy.proxy_cache_hit(&mut session, &mut ()).await });
    tokio::time::timeout(Duration::from_secs(1), reading.notified())
        .await
        .unwrap();
    drop(client);
    let CacheHitOutcome {
        reuse: reusable,
        error,
        client_left,
    } = tokio::time::timeout(Duration::from_secs(1), serve)
        .await
        .expect("the disconnected hit kept waiting for its cache writer")
        .unwrap();
    assert!(!reusable);
    assert!(client_left, "the client watch ended the hit");
    let error = error.expect("client closure must end the response");
    assert_eq!(error.esource, ErrorSource::Downstream);
    assert_eq!(error.etype, ConnectionClosed);
    tokio::time::timeout(Duration::from_secs(1), dropped.notified())
        .await
        .expect("the disconnected request retained its cache reader");
}

/// Serves its body once `release` fires. It notifies `reading` when it starts to wait, and each
/// later time the hit polls it, it wakes whoever waits on `reading` by then.
struct ResumableHit {
    reading: Arc<Notify>,
    release: Arc<Notify>,
    served: bool,
}

#[async_trait]
impl HandleHit for ResumableHit {
    async fn read_body(&mut self) -> Result<Option<Bytes>> {
        if self.served {
            return Ok(None);
        }
        let released = self.release.notified();
        tokio::pin!(released);
        let mut first_poll = true;
        std::future::poll_fn(|context| {
            if std::mem::take(&mut first_poll) {
                self.reading.notify_one();
            } else {
                self.reading.notify_waiters();
            }
            released.as_mut().poll(context)
        })
        .await;
        // Set after the wait, so a dropped read does not end the body early.
        self.served = true;
        Ok(Some(Bytes::from_static(b"body")))
    }

    async fn finish(
        self: Box<Self>,
        _storage: &'static (dyn pingora_cache::storage::Storage + Sync),
        _key: &CacheKey,
        _trace: &pingora_cache::trace::SpanHandle,
    ) -> Result<()> {
        Ok(())
    }

    fn as_any(&self) -> &(dyn Any + Send + Sync) {
        self
    }

    fn as_any_mut(&mut self) -> &mut (dyn Any + Send + Sync) {
        self
    }
}

/// Serve a stalled hit on `request`, then once the hit waits on its reader send `before_close` and
/// close the client, then release the hit. Such a hit does not watch its client, so the close is
/// noticed only once a response write fails, and nothing the client sent after its request header
/// is read.
async fn assert_client_disconnect_is_unobserved(
    handles_downstream_abandonment: bool,
    request: &[u8],
    before_close: &[u8],
) {
    let (mut client, server) = tokio::io::duplex(1024);
    client.write_all(request).await.unwrap();
    let mut session = Session::new_h1(Box::new(server));
    session.handles_downstream_abandonment = handles_downstream_abandonment;
    session.read_request().await.unwrap();
    let reading = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    cache_found(
        &mut session,
        "unwatched-disconnect",
        ResumableHit {
            reading: reading.clone(),
            release: release.clone(),
            served: false,
        },
    );
    let proxy = HttpProxy::new(CacheHitProxy, Arc::new(ServerConf::default()));
    let serve = tokio::spawn(async move {
        let served = proxy.proxy_cache_hit(&mut session, &mut ()).await;
        (served, session)
    });
    tokio::time::timeout(Duration::from_secs(1), reading.notified())
        .await
        .unwrap();
    client.write_all(before_close).await.unwrap();
    drop(client);
    run_ready_tasks().await;
    assert!(
        !serve.is_finished(),
        "a hit that does not watch its client must keep waiting for its reader"
    );
    release.notify_one();
    let (
        CacheHitOutcome {
            reuse: reusable,
            error,
            client_left,
        },
        session,
    ) = tokio::time::timeout(Duration::from_secs(1), serve)
        .await
        .expect("the hit did not end after its response write failed")
        .unwrap();
    assert!(!reusable);
    assert!(!client_left);
    let error = error.expect("the failed response write must end the hit");
    assert_eq!(error.esource, ErrorSource::Downstream);
    assert_eq!(error.etype, WriteError);
    assert_eq!(
        session.body_bytes_read(),
        0,
        "a hit that does not watch its client must not read the request body"
    );
}

const REQUEST_WITH_UNREAD_BODY: &[u8] =
    b"GET / HTTP/1.1\r\nHost: test\r\nContent-Length: 6\r\n\r\nclient";

// Without the opt-in, a hit never reads its client.
#[tokio::test]
async fn client_disconnect_is_unobserved_during_a_stalled_cache_hit_without_the_opt_in() {
    assert_client_disconnect_is_unobserved(false, REQUEST_WITH_UNREAD_BODY, b"").await;
}

// A hit whose request body is not complete when it starts is not watched, and never drains that
// body, so it behaves as without the opt-in.
#[tokio::test]
async fn client_disconnect_is_unobserved_during_a_stalled_cache_hit_with_an_unread_request_body() {
    assert_client_disconnect_is_unobserved(true, REQUEST_WITH_UNREAD_BODY, b"").await;
}

// The following two tests pin the half-close policy of the streaming
// cache-hit wait: a client that shuts down only its write side (while still
// reading) must be treated exactly like the fully-closed case above by
// default, and only keeps its response when the application opts out via
// `set_abort_on_close(false)`.

#[tokio::test]
async fn client_half_close_aborts_a_stalled_cache_reader_by_default() {
    let (mut client, server) = tokio::io::duplex(1024);
    client
        .write_all(b"GET / HTTP/1.1\r\nHost: test\r\n\r\n")
        .await
        .unwrap();
    let mut session = Session::new_h1(Box::new(server));
    session.handles_downstream_abandonment = true;
    session.read_request().await.unwrap();
    let reading = Arc::new(Notify::new());
    let dropped = Arc::new(Notify::new());
    cache_found(
        &mut session,
        "stalled-half-close",
        StalledHit {
            reading: reading.clone(),
            dropped: dropped.clone(),
        },
    );
    let proxy = HttpProxy::new(CacheHitProxy, Arc::new(ServerConf::default()));
    let serve = tokio::spawn(async move { proxy.proxy_cache_hit(&mut session, &mut ()).await });
    tokio::time::timeout(Duration::from_secs(1), reading.notified())
        .await
        .unwrap();
    client.shutdown().await.unwrap();
    let CacheHitOutcome {
        reuse: reusable,
        error,
        client_left,
    } = tokio::time::timeout(Duration::from_secs(1), serve)
        .await
        .expect("a half-closed client must still be treated as gone by default")
        .unwrap();
    assert!(!reusable);
    assert!(client_left);
    let error = error.expect("a half-closed client must end the response by default");
    assert_eq!(error.esource, ErrorSource::Downstream);
    assert_eq!(error.etype, ConnectionClosed);
    tokio::time::timeout(Duration::from_secs(1), dropped.notified())
        .await
        .expect("the half-closed request retained its cache reader");
}

#[tokio::test]
async fn client_half_close_still_receives_its_response_when_abort_on_close_is_disabled() {
    let (mut client, server) = tokio::io::duplex(1024);
    client
        .write_all(b"GET / HTTP/1.1\r\nHost: test\r\n\r\n")
        .await
        .unwrap();
    let mut session = Session::new_h1(Box::new(server));
    session.handles_downstream_abandonment = true;
    session.read_request().await.unwrap();
    session.downstream_session.set_abort_on_close(false);
    let reading = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    cache_found(
        &mut session,
        "resumable-half-close",
        ResumableHit {
            reading: reading.clone(),
            release: release.clone(),
            served: false,
        },
    );
    let proxy = HttpProxy::new(CacheHitProxy, Arc::new(ServerConf::default()));
    let serve = tokio::spawn(async move { proxy.proxy_cache_hit(&mut session, &mut ()).await });
    tokio::time::timeout(Duration::from_secs(1), reading.notified())
        .await
        .unwrap();
    client.shutdown().await.unwrap();
    // Let the watch see the close before the hit resumes.
    run_ready_tasks().await;
    release.notify_one();
    let CacheHitOutcome {
        reuse: reusable,
        error,
        client_left,
    } = tokio::time::timeout(Duration::from_secs(1), serve)
        .await
        .expect("a half-closed client must still receive its response")
        .unwrap();
    assert!(error.is_none(), "{error:?}");
    assert!(!client_left);
    assert!(reusable);
    tokio::time::timeout(Duration::from_secs(1), read_cached_response(&mut client))
        .await
        .expect("the half-closed client did not receive the cached response");
}

const NEXT_REQUEST: &[u8] = b"GET /next HTTP/1.1\r\nHost: test\r\n\r\n";

/// Serve a stalled hit, send the next request once the hit waits on its reader, then release the
/// hit. The response must arrive in full and the next session on the connection must read that
/// request. Returns the bytes of that request the hit had already read.
async fn early_next_request_during_a_stalled_cache_hit(
    handles_downstream_abandonment: bool,
    pipelining_enabled: bool,
) -> Option<BytesMut> {
    let (mut client, server) = tokio::io::duplex(1024);
    client
        .write_all(b"GET / HTTP/1.1\r\nHost: test\r\n\r\n")
        .await
        .unwrap();
    let mut session = Session::new_h1(Box::new(server));
    session.handles_downstream_abandonment = handles_downstream_abandonment;
    session
        .downstream_session
        .set_pipelining_enabled(pipelining_enabled);
    session.read_request().await.unwrap();
    let reading = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    cache_found(
        &mut session,
        "early-next-request",
        ResumableHit {
            reading: reading.clone(),
            release: release.clone(),
            served: false,
        },
    );
    let proxy = HttpProxy::new(CacheHitProxy, Arc::new(ServerConf::default()));
    let serve = tokio::spawn(async move {
        let served = proxy.proxy_cache_hit(&mut session, &mut ()).await;
        (served, session)
    });
    tokio::time::timeout(Duration::from_secs(1), reading.notified())
        .await
        .unwrap();
    client.write_all(NEXT_REQUEST).await.unwrap();
    // Let a watching hit read the next request before the hit resumes.
    run_ready_tasks().await;
    release.notify_one();
    let (
        CacheHitOutcome {
            reuse: reusable,
            error,
            client_left,
        },
        session,
    ) = tokio::time::timeout(Duration::from_secs(1), serve)
        .await
        .expect("the hit did not finish after the client sent its next request")
        .unwrap();
    assert!(
        error.is_none(),
        "an early next request must not end the response: {error:?}"
    );
    assert!(!client_left);
    assert!(reusable);
    tokio::time::timeout(Duration::from_secs(1), read_cached_response(&mut client))
        .await
        .expect("the client did not receive the cached response");
    let (next, prefix) = next_session(session).await;
    assert_eq!(next.req_header().uri.path(), "/next");
    prefix
}

// With pipelining enabled, the watch's idle read keeps the first byte of an early next request for
// the next session, as the regular HTTP/1 proxy loops do, and the hit completes.
#[tokio::test]
async fn early_next_request_during_a_stalled_cache_hit_reaches_the_next_session_with_pipelining() {
    let prefix = early_next_request_during_a_stalled_cache_hit(true, true).await;
    assert_eq!(
        prefix.as_deref(),
        Some(&b"G"[..]),
        "the watch must hand the byte it read to the next session"
    );
}

#[tokio::test]
async fn early_next_request_during_a_stalled_cache_hit_with_pipelining_stays_unread_without_the_opt_in(
) {
    let prefix = early_next_request_during_a_stalled_cache_hit(false, true).await;
    assert!(
        prefix.is_none(),
        "a hit without the opt-in never reads its client"
    );
}

#[tokio::test]
async fn early_next_request_during_a_stalled_cache_hit_stays_unread_without_the_opt_in() {
    let prefix = early_next_request_during_a_stalled_cache_hit(false, false).await;
    assert!(
        prefix.is_none(),
        "a hit without the opt-in never reads its client"
    );
}

/// Serve an opted-in stalled hit with pipelining disabled, send the next request once the hit
/// waits on its reader, and close the connection too if `then_close`. The watch's idle read gets
/// the request's first byte before any close, which ends the hit with the `ConnectError` the
/// regular HTTP/1 proxy loops return for it. The client was still connected at that read, so the
/// hit does not report it as gone. Leaving the hit releases its reader.
async fn assert_early_next_request_ends_a_stalled_cache_hit(then_close: bool) {
    let (mut client, server) = tokio::io::duplex(1024);
    client
        .write_all(b"GET / HTTP/1.1\r\nHost: test\r\n\r\n")
        .await
        .unwrap();
    let mut session = Session::new_h1(Box::new(server));
    session.handles_downstream_abandonment = true;
    session.read_request().await.unwrap();
    let reading = Arc::new(Notify::new());
    let dropped = Arc::new(Notify::new());
    cache_found(
        &mut session,
        "early-next-request-ends-hit",
        StalledHit {
            reading: reading.clone(),
            dropped: dropped.clone(),
        },
    );
    let proxy = HttpProxy::new(CacheHitProxy, Arc::new(ServerConf::default()));
    let serve = tokio::spawn(async move { proxy.proxy_cache_hit(&mut session, &mut ()).await });
    tokio::time::timeout(Duration::from_secs(1), reading.notified())
        .await
        .unwrap();
    client.write_all(NEXT_REQUEST).await.unwrap();
    let client = (!then_close).then_some(client);
    let CacheHitOutcome {
        reuse: reusable,
        error,
        client_left,
    } = tokio::time::timeout(Duration::from_secs(1), serve)
        .await
        .expect("the early next request did not end the stalled hit")
        .unwrap();
    assert!(!reusable);
    assert!(
        !client_left,
        "a client that sent its next request has not left"
    );
    let error = error.expect("the early next request must end the hit");
    assert_eq!(error.esource, ErrorSource::Downstream);
    assert_eq!(error.etype, ConnectError);
    tokio::time::timeout(Duration::from_secs(1), dropped.notified())
        .await
        .expect("the ended hit retained its cache reader");
    drop(client);
}

// Without the opt-in the same request stays unread; see
// `early_next_request_during_a_stalled_cache_hit_stays_unread_without_the_opt_in`.
#[tokio::test]
async fn early_next_request_ends_a_stalled_cache_hit_without_pipelining() {
    assert_early_next_request_ends_a_stalled_cache_hit(false).await;
}

// The hit ends at the early request's first byte, so the close that follows does not change how.
#[tokio::test]
async fn early_next_request_then_close_ends_a_stalled_cache_hit_at_the_request() {
    assert_early_next_request_ends_a_stalled_cache_hit(true).await;
}

// Without the opt-in the hit waits for its reader through both, then fails its response write.
#[tokio::test]
async fn early_next_request_then_close_is_unobserved_during_a_stalled_cache_hit_without_the_opt_in()
{
    assert_client_disconnect_is_unobserved(
        false,
        b"GET / HTTP/1.1\r\nHost: test\r\n\r\n",
        NEXT_REQUEST,
    )
    .await;
}

/// Serves its whole body at once but reports the end only once `eof` fires,
/// like a hit streaming a fill whose writer has not finished.
struct LateEofHit {
    at_eof: Arc<Notify>,
    eof: Arc<Notify>,
    served: bool,
}

#[async_trait]
impl HandleHit for LateEofHit {
    async fn read_body(&mut self) -> Result<Option<Bytes>> {
        if !self.served {
            self.served = true;
            return Ok(Some(Bytes::from_static(b"body")));
        }
        self.at_eof.notify_one();
        self.eof.notified().await;
        Ok(None)
    }

    async fn finish(
        self: Box<Self>,
        _storage: &'static (dyn pingora_cache::storage::Storage + Sync),
        _key: &CacheKey,
        _trace: &pingora_cache::trace::SpanHandle,
    ) -> Result<()> {
        Ok(())
    }

    fn as_any(&self) -> &(dyn Any + Send + Sync) {
        self
    }

    fn as_any_mut(&mut self) -> &mut (dyn Any + Send + Sync) {
        self
    }
}

/// A client that waits for each response sends its next request once it has the whole body,
/// which can be before the hit reports its end. That request belongs to the next session on the
/// same connection, with or without the opt-in, so the hit must leave it unread even with
/// pipelining disabled, where reading it would end the hit.
async fn assert_next_request_after_a_complete_cache_hit_stays_unread(
    handles_downstream_abandonment: bool,
) {
    let (mut client, server) = tokio::io::duplex(1024);
    client
        .write_all(b"GET / HTTP/1.1\r\nHost: test\r\n\r\n")
        .await
        .unwrap();
    let mut session = Session::new_h1(Box::new(server));
    session.handles_downstream_abandonment = handles_downstream_abandonment;
    session.read_request().await.unwrap();
    let at_eof = Arc::new(Notify::new());
    let eof = Arc::new(Notify::new());
    cache_found(
        &mut session,
        "next-request-after-hit",
        LateEofHit {
            at_eof: at_eof.clone(),
            eof: eof.clone(),
            served: false,
        },
    );
    let proxy = HttpProxy::new(CacheHitProxy, Arc::new(ServerConf::default()));
    let serve = tokio::spawn(async move {
        let served = proxy.proxy_cache_hit(&mut session, &mut ()).await;
        (served, session)
    });
    tokio::time::timeout(Duration::from_secs(1), read_cached_response(&mut client))
        .await
        .expect("the client did not receive the cached response");
    tokio::time::timeout(Duration::from_secs(1), at_eof.notified())
        .await
        .unwrap();
    client.write_all(NEXT_REQUEST).await.unwrap();
    // Give the hit the chance to read the next request before it ends.
    run_ready_tasks().await;
    eof.notify_one();
    let (
        CacheHitOutcome {
            reuse: reusable,
            error,
            ..
        },
        session,
    ) = tokio::time::timeout(Duration::from_secs(1), serve)
        .await
        .expect("the hit did not finish after its end was released")
        .unwrap();
    assert!(error.is_none(), "the hit must succeed: {error:?}");
    assert!(reusable);
    let (next, prefix) = next_session(session).await;
    assert_eq!(next.req_header().uri.path(), "/next");
    assert!(
        prefix.is_none(),
        "the hit read the next request after its response was complete"
    );
}

// The watch stops once the response is complete, before the hit reads the end of its body.
#[tokio::test]
async fn next_request_after_a_complete_cache_hit_stays_unread_until_the_next_session() {
    assert_next_request_after_a_complete_cache_hit_stays_unread(true).await;
}

#[tokio::test]
async fn next_request_after_a_complete_cache_hit_stays_unread_until_the_next_session_without_the_opt_in(
) {
    assert_next_request_after_a_complete_cache_hit_stays_unread(false).await;
}
