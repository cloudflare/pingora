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
use crate::test_support::{cache_miss_session_for_request, WRITER_DEMAND_CACHE};
use bytes::Buf;
use pingora_cache::{storage::Storage, trace::Span, CacheMeta, RespCacheable};
use std::sync::atomic::AtomicUsize;

mod filter_execution;
mod harness;
use harness::*;

/// Caches every response and counts the hooks the tests observe.
#[derive(Clone, Default)]
struct CacheFillCallbackProxy {
    calls: Arc<AtomicUsize>,
    calls_with_pending_tasks: Arc<AtomicUsize>,
    upstream_custom_message_calls: Arc<AtomicUsize>,
    downstream_custom_message_calls: Arc<AtomicUsize>,
    trailer_filter_calls: Arc<AtomicUsize>,
    downstream_informational_response_calls: Arc<AtomicUsize>,
    downstream_error_notify: Arc<tokio::sync::Notify>,
    trailer_filter_notify: Arc<tokio::sync::Notify>,
    upstream_informational_response_notify: Arc<tokio::sync::Notify>,
}

#[async_trait]
impl ProxyHttp for CacheFillCallbackProxy {
    type CTX = ();

    fn new_ctx(&self) -> Self::CTX {}

    async fn upstream_peer(
        &self,
        _session: &mut Session,
        _ctx: &mut Self::CTX,
    ) -> Result<Box<HttpPeer>> {
        unreachable!("test calls custom_bidirection_down_to_up directly")
    }

    fn response_cache_filter(
        &self,
        _session: &Session,
        response: &ResponseHeader,
        _ctx: &mut Self::CTX,
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

    fn downstream_abandoned_during_cache_fill(&self, session: &Session, _ctx: &mut Self::CTX) {
        self.calls.fetch_add(1, Ordering::Relaxed);
        if session.has_pending_downstream_tasks() {
            self.calls_with_pending_tasks
                .fetch_add(1, Ordering::Relaxed);
        }
        // As an application whose storage tracks writer demand does.
        session.cache.release_miss_writer_demand();
    }

    async fn upstream_response_filter(
        &self,
        _session: &mut Session,
        response: &mut ResponseHeader,
        _ctx: &mut Self::CTX,
    ) -> Result<()> {
        if response.status.is_informational() {
            self.upstream_informational_response_notify.notify_one();
        }
        Ok(())
    }

    async fn response_filter(
        &self,
        _session: &mut Session,
        response: &mut ResponseHeader,
        _ctx: &mut Self::CTX,
    ) -> Result<()> {
        if response.status.is_informational() {
            self.downstream_informational_response_calls
                .fetch_add(1, Ordering::Relaxed);
        }
        Ok(())
    }

    async fn upstream_response_trailer_filter(
        &self,
        _session: &mut Session,
        _upstream_trailers: &mut http::HeaderMap,
        _ctx: &mut Self::CTX,
    ) -> Result<()> {
        self.trailer_filter_calls.fetch_add(1, Ordering::Relaxed);
        self.trailer_filter_notify.notify_one();
        Ok(())
    }

    async fn upstream_custom_message_proxy_filter(
        &self,
        _session: &mut Session,
        custom_message: Bytes,
        _ctx: &mut Self::CTX,
        _final_hop: bool,
    ) -> Result<Option<Bytes>> {
        self.upstream_custom_message_calls
            .fetch_add(1, Ordering::Relaxed);
        Ok(Some(custom_message))
    }

    async fn downstream_custom_message_proxy_filter(
        &self,
        _session: &mut Session,
        custom_message: Bytes,
        _ctx: &mut Self::CTX,
        _final_hop: bool,
    ) -> Result<Option<Bytes>> {
        self.downstream_custom_message_calls
            .fetch_add(1, Ordering::Relaxed);
        Ok(Some(custom_message))
    }

    fn suppress_proxy_warn_log(
        &self,
        _session: &Session,
        _ctx: &Self::CTX,
        _error: &Error,
        context: ProxyWarnLogContext,
    ) -> bool {
        if context == ProxyWarnLogContext::DownstreamCache {
            self.downstream_error_notify.notify_one();
        }
        false
    }
}

/// The value of one of the counters the proxies of these tests keep.
fn count(counter: &AtomicUsize) -> usize {
    counter.load(Ordering::Relaxed)
}

struct NoopBodyWriter;

#[async_trait]
impl BodyWrite for NoopBodyWriter {
    async fn write_all_buf(&mut self, data: &mut Bytes) -> Result<()> {
        data.advance(data.len());
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

async fn read_response_head(stream: &mut tokio::io::DuplexStream) {
    let mut received = Vec::new();
    while !received.windows(4).any(|window| window == b"\r\n\r\n") {
        let mut buf = [0; 256];
        let read = stream
            .read(&mut buf)
            .await
            .expect("response header should be readable");
        assert!(read > 0, "response ended before its header completed");
        received.extend_from_slice(&buf[..read]);
    }
    assert!(received.starts_with(b"HTTP/1.1 200"));
}

mod failures;
mod success;
