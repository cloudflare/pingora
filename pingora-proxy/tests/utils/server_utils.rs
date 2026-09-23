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

#[cfg(feature = "any_tls")]
use super::cert;
use async_trait::async_trait;
use clap::Parser;
use http::header::{ACCEPT_ENCODING, CONNECTION, CONTENT_LENGTH, TRANSFER_ENCODING, UPGRADE, VARY};
use http::HeaderValue;
use log::error;
use once_cell::sync::Lazy;
use pingora_cache::admission::{AdmissionPolicy, Decision};
use pingora_cache::cache_control::CacheControl;
use pingora_cache::fill_cancellation::is_cache_fill_cancelled;
use pingora_cache::fill_cancellation::ERR_CACHE_FILL_CANCELLED;
use pingora_cache::hashtable::ConcurrentHashTable;
use pingora_cache::key::HashBinary;
use pingora_cache::lock::CacheKeyLockImpl;
use pingora_cache::storage::{
    HandleHit, HandleMiss, MissCompleted, MissCompletion, MissFinishType, Storage,
};
use pingora_cache::{
    eviction::simple_lru::Manager, filters::resp_cacheable, lock::CacheLock, predictor::Predictor,
    set_compression_dict_path, CacheKey, CacheMeta, CacheMetaDefaults, CachePhase, MemCache,
    NoCacheReason, RespCacheable,
};
use pingora_cache::{CacheFillCancellation, CacheFillCancellationWatcher};
use pingora_cache::{
    CacheOptionOverrides, ForcedFreshness, HitHandler, PurgeAction, PurgeType, VarianceBuilder,
};
use pingora_core::apps::{HttpServerApp, HttpServerOptions};
use pingora_core::modules::http::compression::ResponseCompression;
use pingora_core::protocols::{
    http::error_resp::gen_error_response, l4::socket::SocketAddr, Digest,
};
use pingora_core::server::configuration::Opt;
use pingora_core::services::{Service, ServiceWithDependents};
use pingora_core::upstreams::peer::{H1UpgradePolicy, HttpPeer, HttpUpstreamRequestPolicy};
use pingora_core::utils::tls::CertKey;
use pingora_error::ErrorType;
use pingora_error::{Error, ErrorSource, ErrorType::*, Result};
use pingora_http::{RequestHeader, ResponseHeader};
use pingora_proxy::subrequest::pipe::{pipe_subrequest, InputBody, InputBodyType};
use pingora_proxy::subrequest::Ctx as SubrequestCtx;
use pingora_proxy::{FailToProxy, ProxyHttp, ProxyWarnLogContext, Session};
use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use std::thread;
use std::time::{Duration, SystemTime};

pub struct ExampleProxyHttps {}

pub const TEST_PSK_IDENTITY: &str = "test-psk-identity";
pub const TEST_PSK_SECRET: &str = "i2Wx8jrYVi5Vt7HSL/fsk003+PnmfcFuwWMsUyQvcZ4=";

#[allow(clippy::upper_case_acronyms)]
#[derive(Default)]
pub struct CTX {
    conn_reused: bool,
    upstream_client_addr: Option<SocketAddr>,
    upstream_server_addr: Option<SocketAddr>,
}

// Common logic for both ProxyHttp(s) types
fn connected_to_upstream_common(
    reused: bool,
    digest: Option<&Digest>,
    ctx: &mut CTX,
) -> Result<()> {
    ctx.conn_reused = reused;
    let socket_digest = digest
        .expect("upstream connector digest should be set for HTTP sessions")
        .socket_digest
        .as_ref()
        .expect("socket digest should be set for HTTP sessions");
    ctx.upstream_client_addr = socket_digest.local_addr().cloned();
    ctx.upstream_server_addr = socket_digest.peer_addr().cloned();

    Ok(())
}

fn response_filter_common(
    session: &mut Session,
    response: &mut ResponseHeader,
    ctx: &mut CTX,
) -> Result<()> {
    if ctx.conn_reused {
        response.insert_header("x-conn-reuse", "1")?;
    }

    let client_addr = session.client_addr();
    let server_addr = session.server_addr();
    response.insert_header(
        "x-client-addr",
        client_addr.map_or_else(|| "unset".into(), |a| a.to_string()),
    )?;
    response.insert_header(
        "x-server-addr",
        server_addr.map_or_else(|| "unset".into(), |a| a.to_string()),
    )?;

    response.insert_header(
        "x-upstream-client-addr",
        ctx.upstream_client_addr
            .as_ref()
            .map_or_else(|| "unset".into(), |a| a.to_string()),
    )?;
    response.insert_header(
        "x-upstream-server-addr",
        ctx.upstream_server_addr
            .as_ref()
            .map_or_else(|| "unset".into(), |a| a.to_string()),
    )?;

    Ok(())
}

#[async_trait]
#[cfg(feature = "any_tls")]
impl ProxyHttp for ExampleProxyHttps {
    type CTX = CTX;
    fn new_ctx(&self) -> Self::CTX {
        CTX::default()
    }

    async fn upstream_peer(
        &self,
        session: &mut Session,
        _ctx: &mut Self::CTX,
    ) -> Result<Box<HttpPeer>> {
        let session = session.as_downstream();
        let req = session.req_header();

        let port = req
            .headers
            .get("x-port")
            .map_or("8443", |v| v.to_str().unwrap());
        let sni = req.headers.get("sni").map_or("", |v| v.to_str().unwrap());
        let alt = req
            .headers
            .get("alt")
            .map(|v| v.to_str().unwrap().to_string());

        let client_cert = session.get_header_bytes("client_cert");

        let mut peer = Box::new(HttpPeer::new(
            format!("127.0.0.1:{port}"),
            true,
            sni.to_string(),
        ));
        peer.options.alternative_cn = alt;

        let verify = session.get_header_bytes("verify") == b"1";
        peer.options.verify_cert = verify;

        let verify_host = session.get_header_bytes("verify_host") == b"1";
        peer.options.verify_hostname = verify_host;

        if matches!(client_cert, b"1" | b"2") {
            let (mut certs, key) = if client_cert == b"1" {
                (vec![cert::LEAF_CERT.clone()], cert::LEAF_KEY.clone())
            } else {
                (vec![cert::LEAF2_CERT.clone()], cert::LEAF2_KEY.clone())
            };
            if session.get_header_bytes("client_intermediate") == b"1" {
                certs.push(cert::INTERMEDIATE_CERT.clone());
            }
            #[cfg(feature = "s2n")]
            {
                let combined_pem = certs.into_iter().flatten().collect();
                peer.client_cert_key = Some(Arc::new(CertKey::new(combined_pem, key)));
            }
            #[cfg(not(feature = "s2n"))]
            {
                peer.client_cert_key = Some(Arc::new(CertKey::new(certs, key)));
            }
        }

        #[cfg(feature = "s2n")]
        if let Some(psk_identity) = req.headers.get("psk_identity") {
            use pingora_core::{
                protocols::tls::{Psk, PskConfig},
                tls::PskHmac,
            };

            let psk = Psk::new(
                psk_identity.to_str().unwrap().to_string(),
                TEST_PSK_SECRET.as_bytes().to_vec(),
                PskHmac::SHA256,
            );
            peer.options.psk = Some(Arc::new(PskConfig::new(vec![psk])));
        }

        if session.get_header_bytes("x-h2") == b"true" {
            // default is 1, 1
            peer.options.set_http_version(2, 2);
        }

        Ok(peer)
    }

    async fn response_filter(
        &self,
        session: &mut Session,
        upstream_response: &mut ResponseHeader,
        ctx: &mut Self::CTX,
    ) -> Result<()>
    where
        Self::CTX: Send + Sync,
    {
        response_filter_common(session, upstream_response, ctx)
    }

    async fn upstream_request_filter(
        &self,
        session: &mut Session,
        req: &mut RequestHeader,
        _ctx: &mut Self::CTX,
    ) -> Result<()> {
        let host = session.get_header_bytes("host-override");
        if host != b"" {
            req.insert_header("host", host)?;
        }
        Ok(())
    }

    async fn connected_to_upstream(
        &self,
        _http_session: &mut Session,
        reused: bool,
        _peer: &HttpPeer,
        #[cfg(unix)] _fd: std::os::unix::io::RawFd,
        #[cfg(windows)] _sock: std::os::windows::io::RawSocket,
        digest: Option<&Digest>,
        ctx: &mut CTX,
    ) -> Result<()> {
        connected_to_upstream_common(reused, digest, ctx)
    }
}

pub struct ExampleProxyHttp {}

static SUPPRESS_PROXY_WARN_LOG_CALLS: AtomicUsize = AtomicUsize::new(0);

fn enable_proxy_tasks_from_header(session: &mut Session) {
    if session
        .req_header()
        .headers
        .contains_key("x-proxy-tasks-enabled")
    {
        session.downstream_session.set_proxy_tasks_enabled(true);
    }
}

pub fn reset_suppress_proxy_warn_log_calls() {
    SUPPRESS_PROXY_WARN_LOG_CALLS.store(0, Ordering::Relaxed);
}

pub fn suppress_proxy_warn_log_calls() -> usize {
    SUPPRESS_PROXY_WARN_LOG_CALLS.load(Ordering::Relaxed)
}

#[async_trait]
impl ProxyHttp for ExampleProxyHttp {
    type CTX = CTX;
    fn new_ctx(&self) -> Self::CTX {
        CTX::default()
    }

    async fn early_request_filter(
        &self,
        session: &mut Session,
        _ctx: &mut Self::CTX,
    ) -> Result<()> {
        let proxy_tasks_enabled = session
            .req_header()
            .headers
            .get("x-proxy-tasks-enabled")
            .is_some();
        if proxy_tasks_enabled {
            session.downstream_session.set_proxy_tasks_enabled(true);
        }
        let downstream_compression = session
            .req_header()
            .headers
            .get("x-downstream-compression")
            .is_some();
        if downstream_compression {
            session
                .downstream_modules_ctx
                .get_mut::<ResponseCompression>()
                .unwrap()
                .adjust_level(6);
        } else {
            // enable upstream compression for all requests by default
            session.upstream_compression.adjust_level(6);
        }
        Ok(())
    }

    async fn request_filter(&self, session: &mut Session, _ctx: &mut Self::CTX) -> Result<bool> {
        let req = session.req_header();

        let write_timeout = req
            .headers
            .get("x-write-timeout")
            .and_then(|v| v.to_str().ok().and_then(|v| v.parse().ok()));

        let min_rate = req
            .headers
            .get("x-min-rate")
            .and_then(|v| v.to_str().ok().and_then(|v| v.parse().ok()));

        let close_on_response_before_downstream_finish = req
            .headers
            .get("x-close-on-response-before-downstream-finish")
            .is_some();

        let downstream_compression = req.headers.get("x-downstream-compression").is_some();
        if !downstream_compression {
            // enable upstream compression for all requests by default
            session.upstream_compression.adjust_level(6);
            // also disable downstream compression in order to test the upstream one
            session
                .downstream_modules_ctx
                .get_mut::<ResponseCompression>()
                .unwrap()
                .adjust_level(0);
        }

        session.set_min_send_rate(min_rate);
        session.set_write_timeout(write_timeout.map(Duration::from_secs));
        session.set_close_on_response_before_downstream_finish(
            close_on_response_before_downstream_finish,
        );

        Ok(false)
    }

    async fn response_filter(
        &self,
        session: &mut Session,
        upstream_response: &mut ResponseHeader,
        ctx: &mut Self::CTX,
    ) -> Result<()> {
        response_filter_common(session, upstream_response, ctx)
    }

    async fn request_body_filter(
        &self,
        session: &mut Session,
        body: &mut Option<bytes::Bytes>,
        _end_of_stream: bool,
        _ctx: &mut Self::CTX,
    ) -> Result<()> {
        if session
            .req_header()
            .headers
            .contains_key("x-upstream-discard-body")
        {
            *body = None;
        }
        Ok(())
    }

    async fn upstream_request_filter(
        &self,
        session: &mut Session,
        req: &mut RequestHeader,
        _ctx: &mut Self::CTX,
    ) -> Result<()> {
        let host = session.get_header_bytes("host-override");
        if !host.is_empty() {
            req.insert_header("host", host)?;
        }
        if session
            .req_header()
            .headers
            .contains_key("x-upstream-delete-host")
        {
            req.remove_header(&http::header::HOST);
        }

        // Test-only hook: deliberately declare a larger outbound body than the valid
        // downstream HTTP request contains. Built-in HTTP downstream parsing would reject
        // a client that directly ended a shorter-than-declared body; this hook lets tests
        // exercise defense in depth for downstream sessions that do not report incomplete
        // body errors or upstream request mutations that reach that state.
        if let Some(content_length) = session
            .req_header()
            .headers
            .get("x-upstream-content-length")
        {
            req.insert_header(CONTENT_LENGTH, content_length.clone())?;
            req.remove_header(&TRANSFER_ENCODING);
        }
        if session
            .req_header()
            .headers
            .contains_key("x-upstream-strip-framing")
        {
            req.remove_header(&CONTENT_LENGTH);
            req.remove_header(&TRANSFER_ENCODING);
        }
        if session
            .req_header()
            .headers
            .contains_key("x-upstream-add-upgrade")
        {
            req.insert_header(CONNECTION, "Upgrade")?;
            req.insert_header(UPGRADE, "websocket")?;
        }
        Ok(())
    }

    async fn upstream_peer(
        &self,
        session: &mut Session,
        _ctx: &mut Self::CTX,
    ) -> Result<Box<HttpPeer>> {
        let req = session.req_header();
        #[cfg(unix)]
        if req.headers.contains_key("x-uds-peer") {
            return Ok(Box::new(HttpPeer::new_uds(
                "/tmp/pingora_nginx_test.sock",
                false,
                "".to_string(),
            )?));
        }
        let port = req
            .headers
            .get("x-port")
            .map_or("8000", |v| v.to_str().unwrap());

        let mut peer = Box::new(HttpPeer::new(
            format!("127.0.0.1:{port}"),
            false,
            "".to_string(),
        ));

        if session.get_header_bytes("x-h2") == b"true" {
            // default is 1, 1
            peer.options.set_http_version(2, 2);
        }

        if req
            .headers
            .contains_key("x-preserve-upstream-request-headers")
        {
            peer.options.http_upstream_request_policy = HttpUpstreamRequestPolicy::preserve();
        } else if req.headers.contains_key("x-preserve-upstream-upgrade") {
            peer.options.http_upstream_request_policy.h1_upgrade = H1UpgradePolicy::Preserve;
        } else if req.headers.contains_key("x-preserve-connection-nominated") {
            peer.options
                .http_upstream_request_policy
                .strip_connection_nominated = false;
        }

        if let Some(ms) = req
            .headers
            .get("x-read-timeout-ms")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok())
        {
            peer.options.read_timeout = Some(std::time::Duration::from_millis(ms));
        }

        Ok(peer)
    }

    async fn connected_to_upstream(
        &self,
        _http_session: &mut Session,
        reused: bool,
        _peer: &HttpPeer,
        #[cfg(unix)] _fd: std::os::unix::io::RawFd,
        #[cfg(windows)] _sock: std::os::windows::io::RawSocket,
        digest: Option<&Digest>,
        ctx: &mut CTX,
    ) -> Result<()> {
        connected_to_upstream_common(reused, digest, ctx)
    }

    fn suppress_proxy_warn_log(
        &self,
        session: &Session,
        _ctx: &Self::CTX,
        _error: &Error,
        context: ProxyWarnLogContext,
    ) -> bool {
        if session.get_header_bytes("x-test-suppress-proxy-warn-log") == b"true"
            && context == ProxyWarnLogContext::UpstreamRetry
        {
            SUPPRESS_PROXY_WARN_LOG_CALLS.fetch_add(1, Ordering::Relaxed);
            true
        } else {
            false
        }
    }
}

static CACHE_BACKEND: Lazy<MemCache> = Lazy::new(MemCache::new);
static CACHE_FINISH_FAIL_BACKEND: FinishFailCache = FinishFailCache;
static CACHE_STREAMING_FINISH_FAIL_BACKEND: FailingStreamingCache =
    FailingStreamingCache(MissFailure::Finish);
static CACHE_WRITE_FAIL_BACKEND: FailingStreamingCache = FailingStreamingCache(MissFailure::Write);
static CACHE_DEMAND_COUNTING_BACKEND: DemandCountingCache = DemandCountingCache;
static CACHE_FILL_CANCELLATION_BACKEND: FillCancellationCache = FillCancellationCache;
static CACHE_CANCEL_FIRST_MISS_HANDLER_BACKEND: CancelFirstMissHandlerCache =
    CancelFirstMissHandlerCache;
/// Readers of a fill whose writer is dropped unfinished fail instead of seeing the body end.
static CACHE_FAIL_READERS_OF_DROPPED_WRITES_BACKEND: Lazy<MemCache> =
    Lazy::new(|| MemCache::new().fail_readers_of_dropped_writes(true));
const CACHE_DEFAULT: CacheMetaDefaults =
    CacheMetaDefaults::new(|_| Some(Duration::from_secs(1)), 1, 1);
static CACHE_PREDICTOR: Lazy<Predictor<32>> = Lazy::new(|| Predictor::new(5, None));
static EVICTION_MANAGER: Lazy<Manager> = Lazy::new(|| Manager::new(8192)); // 8192 bytes
static CACHE_LOCK: Lazy<Box<CacheKeyLockImpl>> =
    Lazy::new(|| CacheLock::new_boxed(std::time::Duration::from_secs(2)));
static DEFER_ADMISSION_POLICY: DeferAdmissionPolicy = DeferAdmissionPolicy;
// Example of how one might restrict which fields can be varied on.
static CACHE_VARY_ALLOWED_HEADERS: Lazy<Option<HashSet<&str>>> =
    Lazy::new(|| Some(vec!["accept", "accept-encoding"].into_iter().collect()));

struct DeferAdmissionPolicy;

impl AdmissionPolicy for DeferAdmissionPolicy {
    fn observe(&self, _key: &CacheKey) -> Decision {
        Decision::Defer { observed: 1 }
    }
}

struct FinishFailCache;

#[async_trait]
impl Storage for FinishFailCache {
    async fn lookup(
        &'static self,
        _key: &CacheKey,
        _trace: &pingora_cache::trace::SpanHandle,
    ) -> Result<Option<(CacheMeta, HitHandler)>> {
        Ok(None)
    }

    async fn get_miss_handler(
        &'static self,
        _key: &CacheKey,
        _meta: &CacheMeta,
        _trace: &pingora_cache::trace::SpanHandle,
    ) -> Result<pingora_cache::MissHandler> {
        Ok(Box::new(FinishFailMissHandler))
    }

    async fn purge(
        &'static self,
        _target: pingora_cache::PurgeTarget<'_>,
        _purge_type: PurgeType,
        _trace: &pingora_cache::trace::SpanHandle,
    ) -> Result<pingora_cache::storage::PurgeOutcome> {
        Ok(pingora_cache::storage::PurgeOutcome::NotFound)
    }

    async fn update_meta(
        &'static self,
        _key: &CacheKey,
        _meta: &CacheMeta,
        _trace: &pingora_cache::trace::SpanHandle,
    ) -> Result<bool> {
        Ok(false)
    }

    fn as_any(&self) -> &(dyn std::any::Any + Send + Sync + 'static) {
        self
    }
}

struct FinishFailMissHandler;

#[async_trait]
impl HandleMiss for FinishFailMissHandler {
    async fn write_body(&mut self, _data: bytes::Bytes, _eof: bool) -> Result<()> {
        Ok(())
    }

    async fn finish(self: Box<Self>) -> Result<MissFinishType> {
        Error::e_explain(FileWriteError, "cache miss finalization failed")
    }
}

/// Which [`HandleMiss`] call of a [`FailingStreamingCache`] fill fails.
#[derive(Clone, Copy)]
enum MissFailure {
    /// Every body write.
    Write,
    /// The finish of the entry.
    Finish,
}

/// Streaming storage over [`CACHE_BACKEND`] whose fills fail at one [`HandleMiss`] call.
struct FailingStreamingCache(MissFailure);

#[async_trait]
impl Storage for FailingStreamingCache {
    async fn lookup(
        &'static self,
        key: &CacheKey,
        trace: &pingora_cache::trace::SpanHandle,
    ) -> Result<Option<(CacheMeta, HitHandler)>> {
        CACHE_BACKEND.lookup(key, trace).await
    }

    async fn lookup_streaming_write(
        &'static self,
        key: &CacheKey,
        streaming_write_tag: Option<&[u8]>,
        trace: &pingora_cache::trace::SpanHandle,
    ) -> Result<Option<(CacheMeta, HitHandler)>> {
        CACHE_BACKEND
            .lookup_streaming_write(key, streaming_write_tag, trace)
            .await
    }

    async fn get_miss_handler(
        &'static self,
        key: &CacheKey,
        meta: &CacheMeta,
        trace: &pingora_cache::trace::SpanHandle,
    ) -> Result<pingora_cache::MissHandler> {
        let inner = CACHE_BACKEND.get_miss_handler(key, meta, trace).await?;
        Ok(Box::new(FailingMissHandler {
            inner,
            failure: self.0,
        }))
    }

    async fn purge(
        &'static self,
        target: pingora_cache::PurgeTarget<'_>,
        purge_type: PurgeType,
        trace: &pingora_cache::trace::SpanHandle,
    ) -> Result<pingora_cache::storage::PurgeOutcome> {
        CACHE_BACKEND.purge(target, purge_type, trace).await
    }

    async fn update_meta(
        &'static self,
        key: &CacheKey,
        meta: &CacheMeta,
        trace: &pingora_cache::trace::SpanHandle,
    ) -> Result<bool> {
        CACHE_BACKEND.update_meta(key, meta, trace).await
    }

    fn support_streaming_partial_write(&self) -> bool {
        true
    }

    fn as_any(&self) -> &(dyn std::any::Any + Send + Sync + 'static) {
        self
    }
}

struct FailingMissHandler {
    inner: pingora_cache::MissHandler,
    failure: MissFailure,
}

#[async_trait]
impl HandleMiss for FailingMissHandler {
    async fn write_body(&mut self, data: bytes::Bytes, eof: bool) -> Result<()> {
        match self.failure {
            MissFailure::Write => Error::e_explain(FileWriteError, "cache body write failed"),
            MissFailure::Finish => self.inner.write_body(data, eof).await,
        }
    }

    async fn finish(self: Box<Self>) -> Result<MissFinishType> {
        match self.failure {
            MissFailure::Write => self.inner.finish().await,
            MissFailure::Finish => {
                Error::e_explain(FileWriteError, "cache miss finalization failed")
            }
        }
    }

    async fn finish_with_eviction(
        self: Box<Self>,
        completion: MissCompletion,
    ) -> Result<MissCompleted> {
        match self.failure {
            MissFailure::Write => self.inner.finish_with_eviction(completion).await,
            MissFailure::Finish => {
                Error::e_explain(FileWriteError, "cache miss finalization failed")
            }
        }
    }

    fn streaming_write_tag(&self) -> Option<&[u8]> {
        self.inner.streaming_write_tag()
    }

    fn entry_id(&self) -> Option<pingora_cache::eviction::CacheEntryId> {
        self.inner.entry_id()
    }
}

/// The key request-lifecycle instrumentation records under: the request target as sent, including
/// any query string. Two requests differing only by query are distinct requests, and the cache key
/// already distinguishes them, so the counters must agree.
fn request_lifecycle_key(req: &RequestHeader) -> String {
    req.uri
        .path_and_query()
        .map_or_else(|| req.uri.path().to_owned(), |pq| pq.as_str().to_owned())
}

/// What [`DemandCountingCache`] observed for the latest fill of one cache key.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DemandFillObservations {
    /// Whether the writer still holds its claim on the fill.
    pub writer_claim: bool,
    /// `release_writer_demand` calls that released the writer's claim.
    pub writer_demand_releases: usize,
    /// Readers of the in-progress fill that still hold their claim.
    pub reader_claims: usize,
    /// Readers that attached to the in-progress fill.
    pub readers_attached: usize,
    /// Whether every claim was gone at some point, which stops the fill for good.
    pub stopped: bool,
    /// Body writes the storage accepted, and their bytes.
    pub body_writes: usize,
    pub body_bytes: usize,
    /// Body writes the storage failed because the fill had stopped, and their bytes.
    pub failed_writes: usize,
    pub failed_bytes: usize,
    /// Whether the fill finished and was admitted.
    pub finished: bool,
}

impl DemandFillObservations {
    fn update_stopped(&mut self) {
        if !self.writer_claim && self.reader_claims == 0 {
            self.stopped = true;
        }
    }
}

/// One fill's claims, shared by its writer and the readers streaming from it.
#[derive(Default)]
struct FillDemand(Mutex<DemandFillObservations>);

/// Fills in progress, by the cache key hash the underlying storage uses.
static ACTIVE_DEMAND_FILLS: Lazy<Mutex<HashMap<String, Arc<FillDemand>>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));
/// The latest fill of each primary cache key, kept for observation after it ends.
static DEMAND_FILLS_BY_PRIMARY_KEY: Lazy<Mutex<HashMap<String, Arc<FillDemand>>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

/// What the demand-counting storage observed for the latest fill of `primary_key` (the test
/// proxy's cache key is the request's host followed by its path).
pub fn demand_fill_observations(primary_key: &str) -> DemandFillObservations {
    DEMAND_FILLS_BY_PRIMARY_KEY
        .lock()
        .unwrap()
        .get(primary_key)
        .map(|fill| *fill.0.lock().unwrap())
        .unwrap_or_default()
}

/// Streaming storage that stops a fill nobody wants any more, like a storage that serves byte
/// ranges to coalesced readers.
///
/// A fill starts with its writer's claim. Every reader streaming from the fill while it is in
/// progress holds a claim until it drops its hit handler. The writer's claim goes away through
/// [`HandleMiss::release_writer_demand`]. Once no claim is left the fill has stopped: its next
/// body write fails, and its readers then see an error instead of the end of the body. The
/// writer's own read-back of its fill holds no claim.
struct DemandCountingCache;

impl DemandCountingCache {
    fn active_fill(key: &CacheKey) -> Option<Arc<FillDemand>> {
        use pingora_cache::key::CacheHashKey;
        ACTIVE_DEMAND_FILLS
            .lock()
            .unwrap()
            .get(&key.combined())
            .cloned()
    }
}

#[async_trait]
impl Storage for DemandCountingCache {
    async fn lookup(
        &'static self,
        key: &CacheKey,
        trace: &pingora_cache::trace::SpanHandle,
    ) -> Result<Option<(CacheMeta, HitHandler)>> {
        let hit = CACHE_BACKEND.lookup(key, trace).await?;
        let Some((meta, inner)) = hit else {
            return Ok(None);
        };
        let Some(fill) = Self::active_fill(key) else {
            return Ok(Some((meta, inner)));
        };
        Ok(Some((
            meta,
            Box::new(DemandClaimingHitHandler {
                inner,
                claim: ReaderClaim::new(fill),
            }),
        )))
    }

    async fn lookup_streaming_write(
        &'static self,
        key: &CacheKey,
        streaming_write_tag: Option<&[u8]>,
        trace: &pingora_cache::trace::SpanHandle,
    ) -> Result<Option<(CacheMeta, HitHandler)>> {
        CACHE_BACKEND
            .lookup_streaming_write(key, streaming_write_tag, trace)
            .await
    }

    async fn get_miss_handler(
        &'static self,
        key: &CacheKey,
        meta: &CacheMeta,
        trace: &pingora_cache::trace::SpanHandle,
    ) -> Result<pingora_cache::MissHandler> {
        use pingora_cache::key::CacheHashKey;
        let inner = CACHE_BACKEND.get_miss_handler(key, meta, trace).await?;
        let fill = Arc::new(FillDemand(Mutex::new(DemandFillObservations {
            writer_claim: true,
            ..Default::default()
        })));
        let hash = key.combined();
        ACTIVE_DEMAND_FILLS
            .lock()
            .unwrap()
            .insert(hash.clone(), fill.clone());
        DEMAND_FILLS_BY_PRIMARY_KEY.lock().unwrap().insert(
            key.primary_key_str().unwrap_or_default().to_owned(),
            fill.clone(),
        );
        Ok(Box::new(DemandCountingMissHandler {
            inner,
            registration: ActiveFill { fill, hash },
        }))
    }

    async fn purge(
        &'static self,
        target: pingora_cache::PurgeTarget<'_>,
        purge_type: PurgeType,
        trace: &pingora_cache::trace::SpanHandle,
    ) -> Result<pingora_cache::storage::PurgeOutcome> {
        CACHE_BACKEND.purge(target, purge_type, trace).await
    }

    async fn update_meta(
        &'static self,
        key: &CacheKey,
        meta: &CacheMeta,
        trace: &pingora_cache::trace::SpanHandle,
    ) -> Result<bool> {
        CACHE_BACKEND.update_meta(key, meta, trace).await
    }

    fn support_streaming_partial_write(&self) -> bool {
        true
    }

    fn as_any(&self) -> &(dyn std::any::Any + Send + Sync + 'static) {
        self
    }
}

struct DemandCountingMissHandler {
    inner: pingora_cache::MissHandler,
    registration: ActiveFill,
}

impl DemandCountingMissHandler {
    fn fill(&self) -> &FillDemand {
        &self.registration.fill
    }
}

/// A fill's entry in [`ACTIVE_DEMAND_FILLS`], removed when its writer goes.
struct ActiveFill {
    fill: Arc<FillDemand>,
    hash: String,
}

impl Drop for ActiveFill {
    fn drop(&mut self) {
        let mut active = ACTIVE_DEMAND_FILLS.lock().unwrap();
        if active
            .get(&self.hash)
            .is_some_and(|fill| Arc::ptr_eq(fill, &self.fill))
        {
            active.remove(&self.hash);
        }
    }
}

#[async_trait]
impl HandleMiss for DemandCountingMissHandler {
    async fn write_body(&mut self, data: bytes::Bytes, eof: bool) -> Result<()> {
        // An upstream read that ends inside an HTTP/1 chunk head reaches storage as an empty
        // write. Tests count the chunks a fill takes, so such a write counts nowhere, even after
        // the fill stopped.
        if data.is_empty() && !eof {
            return self.inner.write_body(data, eof).await;
        }
        {
            let mut state = self.fill().0.lock().unwrap();
            if state.stopped {
                state.failed_writes += 1;
                state.failed_bytes += data.len();
                return Error::e_explain(FileWriteError, "no claim left on the cache fill");
            }
        }
        let len = data.len();
        self.inner.write_body(data, eof).await?;
        let mut state = self.fill().0.lock().unwrap();
        state.body_writes += 1;
        state.body_bytes += len;
        Ok(())
    }

    async fn finish(self: Box<Self>) -> Result<MissFinishType> {
        let Self {
            inner,
            registration,
        } = *self;
        let finished = inner.finish().await?;
        registration.fill.0.lock().unwrap().finished = true;
        Ok(finished)
    }

    async fn finish_with_eviction(
        self: Box<Self>,
        completion: MissCompletion,
    ) -> Result<MissCompleted> {
        let Self {
            inner,
            registration,
        } = *self;
        let completed = inner.finish_with_eviction(completion).await?;
        registration.fill.0.lock().unwrap().finished = true;
        Ok(completed)
    }

    fn streaming_write_tag(&self) -> Option<&[u8]> {
        self.inner.streaming_write_tag()
    }

    fn release_writer_demand(&self) -> bool {
        let mut state = self.fill().0.lock().unwrap();
        if !state.writer_claim {
            return false;
        }
        state.writer_claim = false;
        state.writer_demand_releases += 1;
        state.update_stopped();
        true
    }
}

/// A reader's claim on an in-progress fill, released when dropped.
struct ReaderClaim(Arc<FillDemand>);

impl ReaderClaim {
    fn new(fill: Arc<FillDemand>) -> Self {
        {
            let mut state = fill.0.lock().unwrap();
            state.reader_claims += 1;
            state.readers_attached += 1;
        }
        Self(fill)
    }
}

impl Drop for ReaderClaim {
    fn drop(&mut self) {
        let mut state = self.0 .0.lock().unwrap();
        state.reader_claims -= 1;
        state.update_stopped();
    }
}

/// A reader of an in-progress [`DemandCountingCache`] fill, holding a claim on it.
struct DemandClaimingHitHandler {
    inner: HitHandler,
    claim: ReaderClaim,
}

#[async_trait]
impl HandleHit for DemandClaimingHitHandler {
    async fn read_body(&mut self) -> Result<Option<bytes::Bytes>> {
        match self.inner.read_body().await? {
            Some(body) => Ok(Some(body)),
            // The in-memory storage ends a partial read the same way whether its writer
            // finished or failed; a fill that stopped must not look complete.
            None if self.claim.0 .0.lock().unwrap().failed_writes > 0 => {
                Error::e_explain(ReadError, "the cache fill stopped before completion")
            }
            None => Ok(None),
        }
    }

    async fn finish(
        self: Box<Self>,
        storage: &'static (dyn Storage + Sync),
        key: &CacheKey,
        trace: &pingora_cache::trace::SpanHandle,
    ) -> Result<()> {
        self.inner.finish(storage, key, trace).await
    }

    fn should_count_access(&self) -> bool {
        self.inner.should_count_access()
    }

    fn as_any(&self) -> &(dyn std::any::Any + Send + Sync) {
        self
    }

    fn as_any_mut(&mut self) -> &mut (dyn std::any::Any + Send + Sync) {
        self
    }
}

/// Storage whose miss handlers offer a [`CacheFillCancellationWatcher`], so tests can end an
/// abandoned fill without waiting for the upstream to finish.
///
/// [`cancellation_for`](Self::cancellation_for) returns the cancellation for a request path,
/// created on first use. The entry is removed once the fixture observes that path's fill reach
/// a terminal state - its miss handler's `finish` returning `Ok` - so a later, unrelated fill of
/// the same path starts from a fresh, uncancelled value instead of inheriting one a previous
/// fill left behind. A miss handler dropped without `finish`, including one a cancellation
/// ended, leaves the entry in place, so a retried upstream attempt of the same logical fill
/// still shares one value; see [`HandleMiss::cancellation_watcher`].
/// [`reset_cancellation`](Self::reset_cancellation) drops an entry unconditionally, for a test
/// that wants to start a genuinely new logical fill right after one that was cancelled.
struct FillCancellationCache;

#[async_trait]
impl Storage for FillCancellationCache {
    async fn lookup(
        &'static self,
        key: &CacheKey,
        trace: &pingora_cache::trace::SpanHandle,
    ) -> Result<Option<(CacheMeta, HitHandler)>> {
        CACHE_BACKEND.lookup(key, trace).await
    }

    async fn lookup_streaming_write(
        &'static self,
        key: &CacheKey,
        streaming_write_tag: Option<&[u8]>,
        trace: &pingora_cache::trace::SpanHandle,
    ) -> Result<Option<(CacheMeta, HitHandler)>> {
        CACHE_BACKEND
            .lookup_streaming_write(key, streaming_write_tag, trace)
            .await
    }

    async fn get_miss_handler(
        &'static self,
        key: &CacheKey,
        meta: &CacheMeta,
        trace: &pingora_cache::trace::SpanHandle,
    ) -> Result<pingora_cache::MissHandler> {
        let inner = CACHE_BACKEND.get_miss_handler(key, meta, trace).await?;
        let cache_key = cache_key_full(key);
        let cancellation = self.cancellation_for(&cache_key);
        CACHE_FILL_CANCELLATION_OBSERVATIONS
            .lock()
            .unwrap()
            .entry(cache_key.clone())
            .or_default()
            .handlers += 1;
        Ok(Box::new(FillCancellationMissHandler {
            inner: Some(inner),
            cache_key,
            cancellation,
        }))
    }

    async fn purge(
        &'static self,
        target: pingora_cache::PurgeTarget<'_>,
        purge_type: PurgeType,
        trace: &pingora_cache::trace::SpanHandle,
    ) -> Result<pingora_cache::storage::PurgeOutcome> {
        CACHE_BACKEND.purge(target, purge_type, trace).await
    }

    async fn expire(
        &'static self,
        target: pingora_cache::PurgeTarget<'_>,
        trace: &pingora_cache::trace::SpanHandle,
    ) -> Result<pingora_cache::storage::PurgeOutcome> {
        CACHE_BACKEND.expire(target, trace).await
    }

    async fn update_meta(
        &'static self,
        key: &CacheKey,
        meta: &CacheMeta,
        trace: &pingora_cache::trace::SpanHandle,
    ) -> Result<bool> {
        CACHE_BACKEND.update_meta(key, meta, trace).await
    }

    fn support_streaming_partial_write(&self) -> bool {
        true
    }

    fn as_any(&self) -> &(dyn std::any::Any + Send + Sync + 'static) {
        self
    }
}

/// The full primary cache key (`{host}{path_and_query}`, see `cache_key_callback`). Keys state
/// that identifies one cache entry, so two hosts serving the same path never share it.
fn cache_key_full(key: &CacheKey) -> String {
    key.primary_key_str()
        .expect("test cache keys are UTF-8")
        .to_owned()
}

struct FillCancellationMissHandler {
    /// `None` once `finish` consumed it, which is how `Drop` tells an unfinished handler apart.
    inner: Option<pingora_cache::MissHandler>,
    /// The full primary cache key this fill's cancellation and observations are keyed under.
    cache_key: String,
    cancellation: Arc<CacheFillCancellation>,
}

#[async_trait]
impl HandleMiss for FillCancellationMissHandler {
    async fn write_body(&mut self, data: bytes::Bytes, eof: bool) -> Result<()> {
        CACHE_FILL_CANCELLATION_OBSERVATIONS
            .lock()
            .unwrap()
            .entry(self.cache_key.clone())
            .or_default()
            .body_bytes += data.len();
        self.inner
            .as_mut()
            .expect("write_body never follows finish")
            .write_body(data, eof)
            .await
    }

    async fn finish(mut self: Box<Self>) -> Result<MissFinishType> {
        let inner = self.take_for_finish();
        let result = inner.finish().await;
        if result.is_ok() {
            self.end_cancellation();
        }
        result
    }

    async fn finish_with_eviction(
        mut self: Box<Self>,
        completion: MissCompletion,
    ) -> Result<MissCompleted> {
        let inner = self.take_for_finish();
        let completed = inner.finish_with_eviction(completion).await?;
        self.end_cancellation();
        Ok(completed)
    }

    fn streaming_write_tag(&self) -> Option<&[u8]> {
        self.inner.as_ref()?.streaming_write_tag()
    }

    fn cancellation_watcher(&self) -> Option<CacheFillCancellationWatcher> {
        Some(self.cancellation.watcher())
    }

    fn entry_id(&self) -> Option<pingora_cache::eviction::CacheEntryId> {
        self.inner.as_ref()?.entry_id()
    }
}

impl FillCancellationMissHandler {
    /// Takes the wrapped handler for its finish, which counts as one finish attempt.
    fn take_for_finish(&mut self) -> pingora_cache::MissHandler {
        let inner = self.inner.take().expect("finish runs once");
        CACHE_FILL_CANCELLATION_OBSERVATIONS
            .lock()
            .unwrap()
            .entry(self.cache_key.clone())
            .or_default()
            .finished += 1;
        inner
    }

    /// The fill reached a terminal state: drop its cancellation so a later, unrelated fill of the
    /// same cache entry does not inherit it. See `FillCancellationCache`'s doc.
    fn end_cancellation(&self) {
        CACHE_FILL_CANCELLATION_BACKEND.finish_cancellation(&self.cache_key, &self.cancellation);
    }
}

impl Drop for FillCancellationMissHandler {
    fn drop(&mut self) {
        if self.inner.is_some() {
            CACHE_FILL_CANCELLATION_OBSERVATIONS
                .lock()
                .unwrap()
                .entry(self.cache_key.clone())
                .or_default()
                .dropped_unfinished += 1;
        }
    }
}

#[derive(Default)]
struct CacheFillCancellationObservations {
    /// Miss handlers created by the cancellation-aware storage.
    handlers: usize,
    /// Body bytes written through those handlers.
    body_bytes: usize,
    /// Handlers that reached `finish`.
    finished: usize,
    /// Handlers dropped without `finish`, which storage treats as a failed write.
    dropped_unfinished: usize,
}

/// Per-cache-entry fill observations (handlers created, bytes written, finishes, drops), keyed
/// by the full primary cache key: two hosts filling the same path are different cache entries.
static CACHE_FILL_CANCELLATION_OBSERVATIONS: Lazy<
    Mutex<HashMap<String, CacheFillCancellationObservations>>,
> = Lazy::new(|| Mutex::new(HashMap::new()));

/// Cancellations by full primary cache key, not by path alone — two hosts filling the same path
/// are different cache entries and must never share one. Created on first use so a cancellation
/// can be requested before the fill has a writer.
static CACHE_FILL_CANCELLATIONS: Lazy<Mutex<HashMap<String, Arc<CacheFillCancellation>>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

impl FillCancellationCache {
    /// The cancellation for `cache_key`, created on first use so a cancellation can be requested
    /// before the fill has a writer.
    fn cancellation_for(&self, cache_key: &str) -> Arc<CacheFillCancellation> {
        Arc::clone(
            CACHE_FILL_CANCELLATIONS
                .lock()
                .unwrap()
                .entry(cache_key.to_owned())
                .or_default(),
        )
    }

    /// Drop `cache_key`'s cancellation unconditionally. `finish` already clears an entry whose
    /// fill reached a terminal state; this is for a test that wants a fresh, uncancelled value
    /// after a fill that was cancelled and therefore never finished.
    fn reset_cancellation(&self, cache_key: &str) {
        CACHE_FILL_CANCELLATIONS.lock().unwrap().remove(cache_key);
    }

    /// Drop `cache_key`'s cancellation now that `cancellation`'s own fill has reached a terminal
    /// state, so a later unrelated fill of the same cache entry starts out uncancelled.
    ///
    /// Removal is by identity, not by cache key: a second logical fill of the same cache entry
    /// installs its own value, and removing that one here would leave it permanently
    /// uncancellable. The lookup and the removal share one lock so the comparison cannot race
    /// `cancellation_for`.
    fn finish_cancellation(&self, cache_key: &str, cancellation: &Arc<CacheFillCancellation>) {
        let mut cancellations = CACHE_FILL_CANCELLATIONS.lock().unwrap();
        if cancellations
            .get(cache_key)
            .is_some_and(|current| Arc::ptr_eq(current, cancellation))
        {
            cancellations.remove(cache_key);
        }
    }
}

/// The `Host` header the only current callers of the path-keyed accessors below send: see
/// `tests/test_basic/cache_fill_abandonment/fill_cancellation.rs`. The accessors below keep
/// taking a bare request path, matching `cache_key_callback`'s production-facing shape, but the
/// maps underneath are keyed by the full primary cache key, so this reconstructs it. A caller
/// under a different host needs its own accessor; nothing today does.
const CANCELLATION_TEST_HOST: &str = "fill-cancellation.example";

/// The full primary cache key `path` maps to for the accessors below (see
/// `CANCELLATION_TEST_HOST`).
fn cancellation_test_key(path: &str) -> String {
    format!("{CANCELLATION_TEST_HOST}{path}")
}

/// Cancel the cache fill for this path. Returns `true` on the first request.
pub fn cancel_cache_fill(path: &str) -> bool {
    CACHE_FILL_CANCELLATION_BACKEND
        .cancellation_for(&cancellation_test_key(path))
        .cancel()
}

/// Drop `path`'s cancellation unconditionally, so a test can start a genuinely new logical
/// fill of the same path after one that was cancelled.
pub fn reset_cancellation(path: &str) {
    CACHE_FILL_CANCELLATION_BACKEND.reset_cancellation(&cancellation_test_key(path));
}

/// Whether a cancellation was requested for this path's cache fill.
pub fn cache_fill_cancellation_requested(path: &str) -> bool {
    CACHE_FILL_CANCELLATIONS
        .lock()
        .unwrap()
        .get(&cancellation_test_key(path))
        .is_some_and(|cancellation| cancellation.is_cancelled())
}

fn cache_fill_cancellation_observation(
    path: &str,
    field: fn(&CacheFillCancellationObservations) -> usize,
) -> usize {
    CACHE_FILL_CANCELLATION_OBSERVATIONS
        .lock()
        .unwrap()
        .get(&cancellation_test_key(path))
        .map(field)
        .unwrap_or_default()
}

/// How many miss handlers the cancellation-aware storage created for this path.
pub fn cache_fill_cancellation_handlers(path: &str) -> usize {
    cache_fill_cancellation_observation(path, |observations| observations.handlers)
}

/// How many body bytes the cancellation-aware storage received for this path.
pub fn cache_fill_cancellation_body_bytes(path: &str) -> usize {
    cache_fill_cancellation_observation(path, |observations| observations.body_bytes)
}

/// How many cancellation-aware miss handlers for this path finished their admission.
pub fn cache_fill_cancellation_handlers_finished(path: &str) -> usize {
    cache_fill_cancellation_observation(path, |observations| observations.finished)
}

/// How many cancellation-aware miss handlers for this path were dropped without finishing.
pub fn cache_fill_cancellation_handlers_dropped_unfinished(path: &str) -> usize {
    cache_fill_cancellation_observation(path, |observations| observations.dropped_unfinished)
}

/// Storage that fails the first [`Storage::get_miss_handler`] call for each cache key with
/// [`ERR_CACHE_FILL_CANCELLED`] and otherwise delegates to the in-memory cache: a storage that
/// cancels a fill while its writer still holds the cache lock.
struct CancelFirstMissHandlerCache;

/// Full primary cache keys whose first miss handler [`CancelFirstMissHandlerCache`] refused.
static CANCELLED_FIRST_MISS_HANDLER_KEYS: Lazy<Mutex<HashSet<String>>> =
    Lazy::new(|| Mutex::new(HashSet::new()));

#[async_trait]
impl Storage for CancelFirstMissHandlerCache {
    async fn lookup(
        &'static self,
        key: &CacheKey,
        trace: &pingora_cache::trace::SpanHandle,
    ) -> Result<Option<(CacheMeta, HitHandler)>> {
        CACHE_BACKEND.lookup(key, trace).await
    }

    async fn lookup_streaming_write(
        &'static self,
        key: &CacheKey,
        streaming_write_tag: Option<&[u8]>,
        trace: &pingora_cache::trace::SpanHandle,
    ) -> Result<Option<(CacheMeta, HitHandler)>> {
        CACHE_BACKEND
            .lookup_streaming_write(key, streaming_write_tag, trace)
            .await
    }

    async fn get_miss_handler(
        &'static self,
        key: &CacheKey,
        meta: &CacheMeta,
        trace: &pingora_cache::trace::SpanHandle,
    ) -> Result<pingora_cache::MissHandler> {
        if CANCELLED_FIRST_MISS_HANDLER_KEYS
            .lock()
            .unwrap()
            .insert(cache_key_full(key))
        {
            return Error::e_explain(
                ERR_CACHE_FILL_CANCELLED,
                "storage cancelled the first fill of this cache key",
            );
        }
        CACHE_BACKEND.get_miss_handler(key, meta, trace).await
    }

    async fn purge(
        &'static self,
        target: pingora_cache::PurgeTarget<'_>,
        purge_type: PurgeType,
        trace: &pingora_cache::trace::SpanHandle,
    ) -> Result<pingora_cache::storage::PurgeOutcome> {
        CACHE_BACKEND.purge(target, purge_type, trace).await
    }

    async fn update_meta(
        &'static self,
        key: &CacheKey,
        meta: &CacheMeta,
        trace: &pingora_cache::trace::SpanHandle,
    ) -> Result<bool> {
        CACHE_BACKEND.update_meta(key, meta, trace).await
    }

    fn support_streaming_partial_write(&self) -> bool {
        true
    }

    fn as_any(&self) -> &(dyn std::any::Any + Send + Sync + 'static) {
        self
    }
}

// #[allow(clippy::upper_case_acronyms)]
pub struct CacheCTX {
    upstream_status: Option<u16>,
}

pub struct ExampleProxyCache {}

static DOWNSTREAM_CACHE_WARN_LOG_CALLS: AtomicUsize = AtomicUsize::new(0);
static DOWNSTREAM_CACHE_WARN_LOG_CALLS_BY_PATH: Lazy<Mutex<HashMap<String, usize>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));
#[derive(Default)]
struct CacheFillAbandonmentObservations {
    calls: usize,
    calls_with_pending_tasks: usize,
}

static DOWNSTREAM_ABANDONED_CACHE_FILL_CALLS: Lazy<
    Mutex<HashMap<String, CacheFillAbandonmentObservations>>,
> = Lazy::new(|| Mutex::new(HashMap::new()));
static RECORDED_UPSTREAM_ATTEMPTS: Lazy<Mutex<HashMap<String, usize>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));
static RECORDED_UPSTREAM_REQUEST_FILTERS: Lazy<Mutex<HashMap<String, usize>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));
static RECORDED_REQUEST_COMPLETIONS: Lazy<Mutex<HashMap<String, usize>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));
static RECORDED_REQUEST_ERRORS: Lazy<Mutex<HashMap<String, Option<ErrorType>>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));
static RECORDED_CACHE_REASONS: Lazy<Mutex<HashMap<String, Option<NoCacheReason>>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));
/// `FailToProxy::error_code` reported by the cache proxy for instrumented requests.
static RECORDED_FAIL_TO_PROXY_CODES: Lazy<Mutex<HashMap<String, u16>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));
/// `suppress_error_log` calls for instrumented requests.
static RECORDED_ERROR_LOG_OFFERS: Lazy<Mutex<HashMap<String, usize>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
/// Upstream response hook calls recorded by the test proxy.
pub struct UpstreamResponseHookCalls {
    /// Response header calls.
    pub headers: usize,
    /// Response body calls with a body chunk.
    pub body_chunks: usize,
    /// Response trailer calls.
    pub trailers: usize,
}

static UPSTREAM_RESPONSE_HOOK_CALLS: Lazy<Mutex<HashMap<String, UpstreamResponseHookCalls>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));
#[derive(Default)]
struct ForcedRetryHookObservations {
    calls: usize,
    calls_with_retry_disabled: usize,
}

static FORCED_RETRY_ERROR_HOOK_CALLS: Lazy<Mutex<HashMap<String, ForcedRetryHookObservations>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));
static RECORDED_CACHE_LOOKUPS: Lazy<Mutex<HashMap<String, usize>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

/// What the cache proxy's subrequest pipe observed for a request that asked for one with
/// `x-pipe-subrequest`.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PipedSubrequest {
    /// How `pipe_subrequest` ended: `None` while it runs, then `Ok` or the error's type.
    pub outcome: Option<std::result::Result<(), ErrorType>>,
}

static PIPED_SUBREQUESTS: Lazy<Mutex<HashMap<String, PipedSubrequest>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

/// How many ignored downstream cache errors the proxy reported for this path.
pub fn downstream_cache_warn_log_calls_for(path: &str) -> usize {
    DOWNSTREAM_CACHE_WARN_LOG_CALLS_BY_PATH
        .lock()
        .unwrap()
        .get(path)
        .copied()
        .unwrap_or_default()
}

/// How many forced-retry hook calls received an error with retry disabled.
pub fn forced_retry_error_hook_calls_with_retry_disabled(path: &str) -> usize {
    FORCED_RETRY_ERROR_HOOK_CALLS
        .lock()
        .unwrap()
        .get(path)
        .map(|observations| observations.calls_with_retry_disabled)
        .unwrap_or_default()
}

/// How many times the cache proxy was notified that the downstream was abandoned while a cache
/// fill continued.
pub fn downstream_abandoned_during_cache_fill_calls(path: &str) -> usize {
    DOWNSTREAM_ABANDONED_CACHE_FILL_CALLS
        .lock()
        .unwrap()
        .get(path)
        .map(|observations| observations.calls)
        .unwrap_or_default()
}

/// How many cache-fill callbacks for this path ran with a queued downstream task.
pub fn downstream_abandoned_with_pending_tasks_calls(path: &str) -> usize {
    DOWNSTREAM_ABANDONED_CACHE_FILL_CALLS
        .lock()
        .unwrap()
        .get(path)
        .map(|observations| observations.calls_with_pending_tasks)
        .unwrap_or_default()
}

/// How many upstream attempts the cache proxy started for this path.
pub fn recorded_upstream_attempts(path: &str) -> usize {
    RECORDED_UPSTREAM_ATTEMPTS
        .lock()
        .unwrap()
        .get(path)
        .copied()
        .unwrap_or_default()
}

/// How many upstream request filters completed for this path.
pub fn recorded_upstream_request_filters(path: &str) -> usize {
    RECORDED_UPSTREAM_REQUEST_FILTERS
        .lock()
        .unwrap()
        .get(path)
        .copied()
        .unwrap_or_default()
}

/// How many instrumented requests completed for this path.
pub fn recorded_request_completions(path: &str) -> usize {
    RECORDED_REQUEST_COMPLETIONS
        .lock()
        .unwrap()
        .get(path)
        .copied()
        .unwrap_or_default()
}

/// The final proxy error recorded for an instrumented request.
pub fn recorded_request_error(path: &str) -> Option<ErrorType> {
    RECORDED_REQUEST_ERRORS
        .lock()
        .unwrap()
        .get(path)
        .cloned()
        .flatten()
}

/// The disabled-cache reason at the end of an instrumented request.
pub fn recorded_cache_reason(path: &str) -> Option<NoCacheReason> {
    RECORDED_CACHE_REASONS
        .lock()
        .unwrap()
        .get(path)
        .copied()
        .flatten()
}

/// The `FailToProxy::error_code` the cache proxy reported for an instrumented request.
pub fn recorded_fail_to_proxy_code(path: &str) -> Option<u16> {
    RECORDED_FAIL_TO_PROXY_CODES
        .lock()
        .unwrap()
        .get(path)
        .copied()
}

/// How many final errors of instrumented requests for this path were offered to
/// `suppress_error_log`.
pub fn recorded_error_log_offers(path: &str) -> usize {
    RECORDED_ERROR_LOG_OFFERS
        .lock()
        .unwrap()
        .get(path)
        .copied()
        .unwrap_or_default()
}

/// Upstream response hook calls recorded for this path.
pub fn upstream_response_hook_calls(path: &str) -> UpstreamResponseHookCalls {
    UPSTREAM_RESPONSE_HOOK_CALLS
        .lock()
        .unwrap()
        .get(path)
        .copied()
        .unwrap_or_default()
}

/// What the subrequest pipe observed for this path, once a request asked for one.
pub fn piped_subrequest(path: &str) -> Option<PipedSubrequest> {
    PIPED_SUBREQUESTS.lock().unwrap().get(path).cloned()
}

/// How many instrumented requests for this path reached their cache lookup.
pub fn recorded_cache_lookups(path: &str) -> usize {
    RECORDED_CACHE_LOOKUPS
        .lock()
        .unwrap()
        .get(path)
        .copied()
        .unwrap_or_default()
}

/// How many times the cache proxy has reported a downstream error that it ignored in order
/// to let a cache fill finish.
///
/// Tests use this to wait for the proxy to actually observe a downstream error instead of
/// sleeping for an arbitrary duration.
pub fn downstream_cache_warn_log_calls() -> usize {
    DOWNSTREAM_CACHE_WARN_LOG_CALLS.load(Ordering::Relaxed)
}

/// How many times the test proxy forced an upstream error to be retryable for this path.
pub fn forced_retry_error_hook_calls(path: &str) -> usize {
    FORCED_RETRY_ERROR_HOOK_CALLS
        .lock()
        .unwrap()
        .get(path)
        .map(|observations| observations.calls)
        .unwrap_or_default()
}

#[async_trait]
impl ProxyHttp for ExampleProxyCache {
    type CTX = CacheCTX;
    fn new_ctx(&self) -> Self::CTX {
        CacheCTX {
            upstream_status: None,
        }
    }

    async fn early_request_filter(
        &self,
        session: &mut Session,
        _ctx: &mut Self::CTX,
    ) -> Result<()> {
        enable_proxy_tasks_from_header(session);
        if let Some(timeout_ms) = session
            .req_header()
            .headers
            .get("x-write-timeout-ms")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse().ok())
        {
            session.set_write_timeout(Some(Duration::from_millis(timeout_ms)));
        }
        if let Some(timeout_ms) = session
            .req_header()
            .headers
            .get("x-downstream-read-timeout-ms")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse().ok())
        {
            session
                .downstream_session
                .set_read_timeout(Some(Duration::from_millis(timeout_ms)));
        }
        if session
            .req_header()
            .headers
            .get("x-downstream-compression")
            .is_some()
        {
            session
                .downstream_modules_ctx
                .get_mut::<ResponseCompression>()
                .unwrap()
                .adjust_level(6);
        }
        if session
            .req_header()
            .headers
            .get("x-downstream-decompression")
            .is_some()
        {
            session
                .downstream_modules_ctx
                .get_mut::<ResponseCompression>()
                .unwrap()
                .adjust_decompression(true);
        }
        if session.get_header_bytes("x-abort-on-close") == b"false" {
            session.downstream_session.set_abort_on_close(false);
        }
        if session.get_header_bytes("x-pipelining") == b"true" {
            session.set_pipelining_enabled(true);
        }
        Ok(())
    }

    fn allow_spawning_subrequest(&self, session: &Session, _ctx: &Self::CTX) -> bool {
        session
            .req_header()
            .headers
            .contains_key("x-pipe-subrequest")
    }

    /// `x-pipe-subrequest: no-body` serves an HTTP/1 request by piping a subrequest for the same
    /// target (which proxies it as usual) to the client, with an empty preset request body. The
    /// outcome is recorded for [`piped_subrequest`].
    async fn request_filter(&self, session: &mut Session, _ctx: &mut Self::CTX) -> Result<bool> {
        if session.get_header_bytes("x-pipe-subrequest") != b"no-body"
            || session.subrequest_ctx.is_some()
        {
            // Not a piped request, or the piped subrequest itself.
            return Ok(false);
        }
        let path = request_lifecycle_key(session.req_header());
        PIPED_SUBREQUESTS
            .lock()
            .unwrap()
            .insert(path.clone(), PipedSubrequest::default());
        let (subrequest, handle) = session
            .subrequest_spawner
            .as_ref()
            .expect("allow_spawning_subrequest is set for piped requests")
            .create_subrequest(session.as_downstream(), SubrequestCtx::builder().build());
        let result = pipe_subrequest(
            session,
            subrequest,
            handle,
            |task| Ok(Some(task)),
            InputBodyType::Preset(InputBody::NoBody),
        )
        .await;
        let outcome = Some(
            result
                .as_ref()
                .map(|_| ())
                .map_err(|e| e.error.etype().clone()),
        );
        PIPED_SUBREQUESTS
            .lock()
            .unwrap()
            .insert(path, PipedSubrequest { outcome });
        result.map(|_| true).map_err(|e| e.error)
    }

    async fn upstream_peer(
        &self,
        session: &mut Session,
        _ctx: &mut Self::CTX,
    ) -> Result<Box<HttpPeer>> {
        let req = session.req_header();
        if req.headers.contains_key("x-record-request-lifecycle") {
            *RECORDED_UPSTREAM_ATTEMPTS
                .lock()
                .unwrap()
                .entry(request_lifecycle_key(req))
                .or_default() += 1;
        }
        let port = req
            .headers
            .get("x-port")
            .map_or("8000", |v| v.to_str().unwrap());

        let mut peer = Box::new(HttpPeer::new(
            format!("127.0.0.1:{}", port),
            false,
            "".to_string(),
        ));

        if session.get_header_bytes("x-h2") == b"true" {
            // default is 1, 1
            peer.options.set_http_version(2, 2);
            if let Some(max_streams) = req
                .headers
                .get("x-h2-max-streams")
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.parse().ok())
            {
                peer.options.max_h2_streams = max_streams;
            }
        }
        if let Some(recv_buf) = req
            .headers
            .get("x-upstream-tcp-recv-buf")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse().ok())
        {
            peer.options.tcp_recv_buf = Some(recv_buf);
        }

        Ok(peer)
    }

    fn request_cache_filter(&self, session: &mut Session, _ctx: &mut Self::CTX) -> Result<()> {
        if session
            .req_header()
            .headers
            .contains_key("x-record-request-lifecycle")
        {
            // Nothing between this filter and the cache lookup (or the wait on the cache lock
            // that follows it) waits on I/O, so a test can take this count as "the request has
            // reached its cache lookup".
            *RECORDED_CACHE_LOOKUPS
                .lock()
                .unwrap()
                .entry(request_lifecycle_key(session.req_header()))
                .or_default() += 1;
        }
        // TODO: only allow GET & HEAD

        if session.get_header_bytes("x-bypass-cache") != b"" {
            return Ok(());
        }

        // turn on eviction only for some requests to avoid interference across tests
        let eviction = session.req_header().headers.get("x-eviction").map(|_| {
            &*EVICTION_MANAGER as &'static (dyn pingora_cache::eviction::EvictionManager + Sync)
        });
        let lock = session
            .req_header()
            .headers
            .get("x-lock")
            .map(|_| CACHE_LOCK.as_ref());
        let mut overrides = CacheOptionOverrides::default();
        overrides.wait_timeout = Some(Duration::from_secs(2));
        if let Some(wait_timeout) = session
            .req_header()
            .headers
            .get("x-cache-lock-wait-timeout-ms")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse().ok())
            .map(Duration::from_millis)
        {
            overrides.wait_timeout = Some(wait_timeout);
        }
        let storage = if session
            .req_header()
            .headers
            .contains_key("x-cache-fill-cancellation-watch")
        {
            &CACHE_FILL_CANCELLATION_BACKEND as &'static (dyn Storage + Sync)
        } else if session
            .req_header()
            .headers
            .contains_key("x-cache-cancel-first-miss-handler")
        {
            &CACHE_CANCEL_FIRST_MISS_HANDLER_BACKEND as &'static (dyn Storage + Sync)
        } else if session
            .req_header()
            .headers
            .contains_key("x-cache-fail-write")
        {
            &CACHE_WRITE_FAIL_BACKEND as &'static (dyn Storage + Sync)
        } else if session
            .req_header()
            .headers
            .contains_key("x-cache-fail-finish-streaming")
        {
            &CACHE_STREAMING_FINISH_FAIL_BACKEND as &'static (dyn Storage + Sync)
        } else if session
            .req_header()
            .headers
            .contains_key("x-cache-fail-finish")
        {
            &CACHE_FINISH_FAIL_BACKEND as &'static (dyn Storage + Sync)
        } else if session
            .req_header()
            .headers
            .contains_key("x-cache-demand-counting")
        {
            &CACHE_DEMAND_COUNTING_BACKEND as &'static (dyn Storage + Sync)
        } else if session
            .req_header()
            .headers
            .contains_key("x-cache-fail-readers-of-dropped-writes")
        {
            &*CACHE_FAIL_READERS_OF_DROPPED_WRITES_BACKEND as &'static (dyn Storage + Sync)
        } else {
            &*CACHE_BACKEND as &'static (dyn Storage + Sync)
        };

        session.cache.enable(
            storage,
            eviction,
            Some(&*CACHE_PREDICTOR),
            lock,
            Some(overrides),
        );

        if session
            .req_header()
            .headers
            .contains_key("x-defer-cache-admission")
        {
            session.cache.set_admission_policy(&DEFER_ADMISSION_POLICY);
        }

        if let Some(max_file_size_hdr) = session
            .req_header()
            .headers
            .get("x-cache-max-file-size-bytes")
        {
            let bytes = max_file_size_hdr
                .to_str()
                .unwrap()
                .parse::<usize>()
                .unwrap();
            session.cache.set_max_file_size_bytes(bytes);
        }

        Ok(())
    }

    fn cache_miss(&self, session: &mut Session, _ctx: &mut Self::CTX) {
        if session
            .req_header()
            .headers
            .contains_key("x-test-leave-predictor-bypass")
        {
            return;
        }
        session.cache.cache_miss();
    }

    /// Reference `cache_key_callback` implementation for integration tests.
    ///
    /// Builds the primary key as `{host}{path_and_query}` from the request.
    /// This is **not production ready**: it does not account for `Vary`, custom
    /// request filters, or scheme differences. See the rustdoc on
    /// [`ProxyHttp::cache_key_callback`] for details.
    fn cache_key_callback(&self, session: &Session, _ctx: &mut Self::CTX) -> Result<CacheKey> {
        let req_header = session.req_header();

        let host = req_header
            .headers
            .get(http::header::HOST)
            .and_then(|v| v.to_str().ok())
            .or_else(|| req_header.uri.authority().map(|a| a.as_str()))
            .unwrap_or("");

        let path_and_query = req_header
            .uri
            .path_and_query()
            .map(|pq| pq.as_str())
            .unwrap_or("/");

        Ok(CacheKey::new(
            format!("{host}{path_and_query}"),
            String::new(),
        ))
    }

    async fn cache_hit_filter(
        &self,
        session: &mut Session,
        _meta: &CacheMeta,
        _hit_handler: &mut HitHandler,
        is_fresh: bool,
        _ctx: &mut Self::CTX,
    ) -> Result<Option<ForcedFreshness>> {
        // allow test header to control force expiry/miss
        if session.get_header_bytes("x-force-miss") != b"" {
            return Ok(Some(ForcedFreshness::ForceMiss));
        }

        if !is_fresh {
            if session.get_header_bytes("x-force-fresh") != b"" {
                return Ok(Some(ForcedFreshness::ForceFresh));
            }
            // already expired
            return Ok(None);
        }

        if session.get_header_bytes("x-force-expire") != b"" {
            return Ok(Some(ForcedFreshness::ForceExpired));
        }

        if session.get_header_bytes("x-force-expire-serve-stale") != b"" {
            // `x-expired-secs-ago` backdates the expiry, which is how a test reaches an
            // asset whose serve stale window has already run out at the point it expired.
            let secs_ago = std::str::from_utf8(session.get_header_bytes("x-expired-secs-ago"))
                .ok()
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(0);
            return Ok(Some(ForcedFreshness::ForceExpiredServeStale {
                expired_at: SystemTime::now().checked_sub(Duration::from_secs(secs_ago)),
            }));
        }
        Ok(None)
    }

    fn cache_vary_filter(
        &self,
        meta: &CacheMeta,
        _ctx: &mut Self::CTX,
        req: &RequestHeader,
    ) -> Option<HashBinary> {
        let mut key = VarianceBuilder::new();

        // Vary per header from origin. Target headers are de-duplicated by key logic.
        let vary_headers_lowercased: Vec<String> = meta
            .headers()
            .get_all(VARY)
            .iter()
            // Filter out any unparseable vary headers.
            .flat_map(|vary_header| vary_header.to_str().ok())
            .flat_map(|vary_header| vary_header.split(','))
            .map(|s| s.trim().to_lowercase())
            .filter(|header_name| {
                // Filter only for allowed headers, if restricted.
                CACHE_VARY_ALLOWED_HEADERS
                    .as_ref()
                    .map(|al| al.contains(header_name.as_str()))
                    .unwrap_or(true)
            })
            .collect();

        vary_headers_lowercased.iter().for_each(|header_name| {
            // Add this header and value to be considered in the variance key.
            key.add_value(
                header_name,
                req.headers
                    .get(header_name)
                    .map(|v| v.as_bytes())
                    .unwrap_or(&[]),
            );
        });

        key.finalize()
    }

    async fn upstream_request_filter(
        &self,
        session: &mut Session,
        upstream_request: &mut RequestHeader,
        _ctx: &mut Self::CTX,
    ) -> Result<()> {
        if session
            .req_header()
            .headers
            .contains_key("x-record-request-lifecycle")
        {
            *RECORDED_UPSTREAM_REQUEST_FILTERS
                .lock()
                .unwrap()
                .entry(request_lifecycle_key(session.req_header()))
                .or_default() += 1;
        }
        if let Some(up_accept_encoding) = session
            .req_header()
            .headers
            .get("x-upstream-accept-encoding")
        {
            upstream_request.insert_header(&ACCEPT_ENCODING, up_accept_encoding)?;
        }
        Ok(())
    }

    fn response_cache_filter(
        &self,
        session: &Session,
        resp: &ResponseHeader,
        _ctx: &mut Self::CTX,
    ) -> Result<RespCacheable> {
        // Allow testing the unlikely case of caching a 101 response
        if resp.status == 101
            && session
                .req_header()
                .headers
                .contains_key("x-cache-websocket")
        {
            return Ok(RespCacheable::Cacheable(CacheMeta::new(
                SystemTime::now() + Duration::from_secs(5),
                SystemTime::now(),
                0,
                0,
                resp.clone(),
            )));
        }

        let cc = CacheControl::from_resp_headers(resp);
        Ok(resp_cacheable(
            cc.as_ref(),
            resp.clone(),
            false,
            &CACHE_DEFAULT,
        ))
    }

    async fn upstream_response_filter(
        &self,
        session: &mut Session,
        upstream_response: &mut ResponseHeader,
        ctx: &mut Self::CTX,
    ) -> Result<()> {
        ctx.upstream_status = Some(upstream_response.status.into());
        if session
            .req_header()
            .headers
            .contains_key("x-test-upstream-response-hooks")
        {
            let path = request_lifecycle_key(session.req_header());
            UPSTREAM_RESPONSE_HOOK_CALLS
                .lock()
                .unwrap()
                .entry(path)
                .or_default()
                .headers += 1;
            upstream_response.insert_header("x-upstream-hook-applied", "true")?;
        }
        if session
            .req_header()
            .headers
            .contains_key("x-upstream-fake-http10")
        {
            // TODO to simulate an actual http1.0 origin
            upstream_response.set_version(http::Version::HTTP_10);
            upstream_response.remove_header(&CONTENT_LENGTH);
            upstream_response.remove_header(&TRANSFER_ENCODING);
        }
        // Allow tests to inject Cache-Control into the upstream response
        if let Some(cc) = session.req_header().headers.get("x-set-cache-control") {
            upstream_response
                .insert_header(http::header::CACHE_CONTROL, cc)
                .unwrap();
        }
        Ok(())
    }

    async fn upstream_response_body_filter(
        &self,
        session: &mut Session,
        body: &mut Option<bytes::Bytes>,
        _end_of_stream: bool,
        _ctx: &mut Self::CTX,
    ) -> Result<Option<Duration>> {
        if session
            .req_header()
            .headers
            .contains_key("x-test-upstream-response-hooks")
        {
            if body.is_some() {
                let path = request_lifecycle_key(session.req_header());
                UPSTREAM_RESPONSE_HOOK_CALLS
                    .lock()
                    .unwrap()
                    .entry(path)
                    .or_default()
                    .body_chunks += 1;
            }

            if body.as_deref() == Some(b"after abandonment") {
                *body = Some(bytes::Bytes::from_static(b"after filter data"));
                let delay = session
                    .req_header()
                    .headers
                    .get("x-upstream-hook-delay-ms")
                    .and_then(|value| value.to_str().ok())
                    .and_then(|value| value.parse().ok())
                    .map(Duration::from_millis);
                return Ok(delay);
            }
        }

        if session
            .req_header()
            .headers
            .contains_key("x-retryable-upstream-body-filter-error")
            && body.as_deref() == Some(b"trigger retryable body filter error")
        {
            let mut error = Error::explain(ReadError, "injected upstream body filter error");
            if !session
                .req_header()
                .headers
                .contains_key("x-force-retry-in-error-hook")
            {
                error.set_retry(true);
            }
            return Err(error.into_up());
        }
        Ok(None)
    }

    async fn upstream_response_trailer_filter(
        &self,
        session: &mut Session,
        _upstream_trailers: &mut http::HeaderMap,
        _ctx: &mut Self::CTX,
    ) -> Result<()> {
        if session
            .req_header()
            .headers
            .contains_key("x-test-upstream-response-hooks")
        {
            let path = request_lifecycle_key(session.req_header());
            UPSTREAM_RESPONSE_HOOK_CALLS
                .lock()
                .unwrap()
                .entry(path)
                .or_default()
                .trailers += 1;
        }
        Ok(())
    }

    fn error_while_proxy(
        &self,
        peer: &HttpPeer,
        session: &mut Session,
        error: Box<Error>,
        _ctx: &mut Self::CTX,
        client_reused: bool,
    ) -> Box<Error> {
        let mut error = error.more_context(format!("Peer: {peer}"));
        if session
            .req_header()
            .headers
            .contains_key("x-force-retry-in-error-hook")
        {
            let path = request_lifecycle_key(session.req_header());
            let mut observations_by_path = FORCED_RETRY_ERROR_HOOK_CALLS.lock().unwrap();
            let observations = observations_by_path.entry(path).or_default();
            observations.calls += 1;
            observations.calls_with_retry_disabled += usize::from(matches!(
                error.retry,
                pingora_error::RetryType::Decided(false)
            ));
            error.set_retry(true);
        } else if !session.req_header().method.is_idempotent()
            || session.as_ref().retry_buffer_truncated()
        {
            error.set_retry(false);
        } else {
            error.retry.decide_reuse(client_reused);
        }
        error
    }

    async fn response_filter(
        &self,
        session: &mut Session,
        upstream_response: &mut ResponseHeader,
        ctx: &mut Self::CTX,
    ) -> Result<()>
    where
        Self::CTX: Send + Sync,
    {
        if upstream_response.status.is_informational()
            && session
                .req_header()
                .headers
                .contains_key("x-fail-downstream-informational-filter-after-abandonment")
        {
            return Error::e_explain(
                InternalError,
                "downstream informational response filter ran after abandonment",
            );
        }
        if session.cache.enabled() {
            match session.cache.phase() {
                CachePhase::Hit => upstream_response.insert_header("x-cache-status", "hit")?,
                CachePhase::Miss => upstream_response.insert_header("x-cache-status", "miss")?,
                CachePhase::Stale => upstream_response.insert_header("x-cache-status", "stale")?,
                CachePhase::StaleUpdating => {
                    upstream_response.insert_header("x-cache-status", "stale-updating")?
                }
                CachePhase::Expired => {
                    upstream_response.insert_header("x-cache-status", "expired")?
                }
                CachePhase::Revalidated | CachePhase::RevalidatedNoCache(_) => {
                    upstream_response.insert_header("x-cache-status", "revalidated")?
                }
                _ => upstream_response.insert_header("x-cache-status", "invalid")?,
            }
        } else {
            match session.cache.phase() {
                CachePhase::Disabled(NoCacheReason::Deferred) => {
                    upstream_response.insert_header("x-cache-status", "deferred")?;
                }
                _ => upstream_response.insert_header("x-cache-status", "no-cache")?,
            }
        }
        if let Some(d) = session.cache.lock_duration() {
            upstream_response.insert_header("x-cache-lock-time-ms", format!("{}", d.as_millis()))?
        }
        if let Some(up_stat) = ctx.upstream_status {
            upstream_response.insert_header("x-upstream-status", up_stat.to_string())?;
        }
        Ok(())
    }

    async fn response_body_filter(
        &self,
        session: &mut Session,
        body: &mut Option<bytes::Bytes>,
        _end_of_stream: bool,
        _ctx: &mut Self::CTX,
    ) -> Result<Option<Duration>> {
        if session
            .req_header()
            .headers
            .contains_key("x-fail-downstream-filter-after-abandonment")
            && body.as_deref() == Some(b"after filter data")
        {
            return Error::e_explain(
                InternalError,
                "downstream body filter ran after abandonment",
            );
        }
        Ok(None)
    }

    async fn fail_to_proxy(
        &self,
        session: &mut Session,
        e: &Error,
        _ctx: &mut Self::CTX,
    ) -> FailToProxy
    where
        Self::CTX: Send + Sync,
    {
        // default OSS fail_to_proxy with added headers
        let code = match e.etype() {
            _ if session.downstream_abandoned_during_cache_fill() => 0,
            HTTPStatus(code) => *code,
            _ => {
                match e.esource() {
                    ErrorSource::Upstream => 502,
                    ErrorSource::Downstream => {
                        match e.etype() {
                            WriteError | ReadError | ConnectionClosed => {
                                /* conn already dead */
                                0
                            }
                            _ => 400,
                        }
                    }
                    ErrorSource::Internal | ErrorSource::Unset => 500,
                }
            }
        };
        if session
            .req_header()
            .headers
            .contains_key("x-record-request-lifecycle")
        {
            RECORDED_FAIL_TO_PROXY_CODES
                .lock()
                .unwrap()
                .insert(request_lifecycle_key(session.req_header()), code);
        }
        if session.downstream_abandoned_during_cache_fill() {
            // Nothing reaches a downstream that a cache fill abandoned.
            return FailToProxy {
                error_code: code,
                can_reuse_downstream: false,
            };
        }
        if code > 0 {
            let mut resp = gen_error_response(code);
            // any relevant metadata headers to add
            if let Some(d) = session.cache.lock_duration() {
                resp.insert_header("x-cache-lock-time-ms", format!("{}", d.as_millis()))
                    .unwrap();
            }
            session
                .write_response_header(Box::new(resp), true)
                .await
                .unwrap_or_else(|e| {
                    error!("failed to send error response to downstream: {e}");
                });
        }

        FailToProxy {
            error_code: code,
            // default to no reuse, which is safest
            can_reuse_downstream: false,
        }
    }

    fn should_serve_stale(
        &self,
        _session: &mut Session,
        _ctx: &mut Self::CTX,
        error: Option<&Error>, // None when it is called during stale while revalidate
    ) -> bool {
        // enable serve stale while updating
        error.is_none_or(|e| e.esource() == &ErrorSource::Upstream)
    }

    fn is_purge(&self, session: &Session, _ctx: &Self::CTX) -> bool {
        session.req_header().method == "PURGE"
    }

    fn purge_action(&self, session: &Session, _ctx: &Self::CTX) -> PurgeAction {
        if session.get_header_bytes("x-purge-action") == b"expire" {
            PurgeAction::Expire
        } else {
            PurgeAction::Delete
        }
    }

    fn should_handle_downstream_abandonment(&self, session: &Session, _ctx: &Self::CTX) -> bool {
        session.get_header_bytes("x-handle-downstream-abandonment") == b"true"
    }

    /// Records the call and, as an application whose storage tracks writer demand does, releases
    /// the active miss writer's demand. Storages that don't track demand ignore the release.
    fn downstream_abandoned_during_cache_fill(&self, session: &Session, _ctx: &mut Self::CTX) {
        let key = session.cache.cache_key();
        // Derived from the committed `CacheKey` rather than re-parsed from the request, so this
        // always agrees with the cache key the miss handler cancels under (`cache_key_full`),
        // even when the request carries a query string.
        let cache_key = cache_key_full(key);
        if session
            .req_header()
            .headers
            .contains_key("x-cancel-fill-on-abandon")
        {
            // The cancellation-aware storage keys its cancellation by full cache key, so this
            // works before a miss handler exists too.
            CACHE_FILL_CANCELLATION_BACKEND
                .cancellation_for(&cache_key)
                .cancel();
        }
        // Deliberately request-target-scoped, unlike the cancellation above: tests read this by
        // path alone (`downstream_abandoned_during_cache_fill_calls`).
        let path = request_lifecycle_key(session.req_header());
        {
            let mut observations_by_path = DOWNSTREAM_ABANDONED_CACHE_FILL_CALLS.lock().unwrap();
            let observations = observations_by_path.entry(path).or_default();
            observations.calls += 1;
            if session.has_pending_downstream_tasks() {
                observations.calls_with_pending_tasks += 1;
            }
        }
        session.cache.release_miss_writer_demand();
    }

    async fn logging(&self, session: &mut Session, error: Option<&Error>, _ctx: &mut Self::CTX) {
        if session
            .req_header()
            .headers
            .contains_key("x-record-request-lifecycle")
        {
            let path = request_lifecycle_key(session.req_header());
            RECORDED_REQUEST_ERRORS
                .lock()
                .unwrap()
                .insert(path.clone(), error.map(|error| error.etype().clone()));
            RECORDED_CACHE_REASONS.lock().unwrap().insert(
                path.clone(),
                match session.cache.phase() {
                    CachePhase::Disabled(reason) => Some(reason),
                    _ => None,
                },
            );
            *RECORDED_REQUEST_COMPLETIONS
                .lock()
                .unwrap()
                .entry(path)
                .or_default() += 1;
        }
    }

    /// Counts the final errors offered for instrumented requests and keeps the default answer.
    fn suppress_error_log(&self, session: &Session, _ctx: &Self::CTX, error: &Error) -> bool {
        if session
            .req_header()
            .headers
            .contains_key("x-record-request-lifecycle")
        {
            *RECORDED_ERROR_LOG_OFFERS
                .lock()
                .unwrap()
                .entry(request_lifecycle_key(session.req_header()))
                .or_default() += 1;
        }
        session.downstream_abandoned_during_cache_fill() && is_cache_fill_cancelled(error)
    }

    fn suppress_proxy_warn_log(
        &self,
        _session: &Session,
        _ctx: &Self::CTX,
        _error: &Error,
        context: ProxyWarnLogContext,
    ) -> bool {
        if context == ProxyWarnLogContext::DownstreamCache {
            DOWNSTREAM_CACHE_WARN_LOG_CALLS.fetch_add(1, Ordering::Relaxed);
            let path = request_lifecycle_key(_session.req_header());
            *DOWNSTREAM_CACHE_WARN_LOG_CALLS_BY_PATH
                .lock()
                .unwrap()
                .entry(path)
                .or_default() += 1;
        }
        // only observing, keep the logging behavior unchanged
        false
    }
}

fn test_main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let opts: Vec<String> = vec![
        "pingora-proxy".into(),
        "-c".into(),
        "tests/pingora_conf.yaml".into(),
    ];
    let mut my_server =
        pingora_core::server::Server::new(Some(Opt::parse_from_args(opts))).unwrap();
    my_server.bootstrap();

    let mut proxy_service_http =
        pingora_proxy::http_proxy_service(&my_server.configuration, ExampleProxyHttp {});
    proxy_service_http.add_tcp("0.0.0.0:6147");
    #[cfg(unix)]
    proxy_service_http.add_uds("/tmp/pingora_proxy.sock", None);

    let mut proxy_service_http_connect =
        pingora_proxy::http_proxy_service(&my_server.configuration, ExampleProxyHttp {});
    let http_logic = proxy_service_http_connect.app_logic_mut().unwrap();
    let mut http_server_options = HttpServerOptions::default();
    http_server_options.allow_connect_method_proxying = true;
    http_logic.server_options = Some(http_server_options);
    proxy_service_http_connect.add_tcp("0.0.0.0:6160");

    let mut proxy_service_h2c =
        pingora_proxy::http_proxy_service(&my_server.configuration, ExampleProxyHttp {});

    let http_logic = proxy_service_h2c.app_logic_mut().unwrap();
    let mut http_server_options = HttpServerOptions::default();
    http_server_options.h2c = true;
    http_logic.server_options = Some(http_server_options);
    proxy_service_h2c.add_tcp("0.0.0.0:6146");

    let mut proxy_service_https_opt: Option<Box<dyn ServiceWithDependents>> = None;

    #[cfg(feature = "any_tls")]
    {
        let mut proxy_service_https =
            pingora_proxy::http_proxy_service(&my_server.configuration, ExampleProxyHttps {});
        proxy_service_https.add_tcp("0.0.0.0:6149");
        let cert_path = format!("{}/tests/keys/server.crt", env!("CARGO_MANIFEST_DIR"));
        let key_path = format!("{}/tests/keys/key.pem", env!("CARGO_MANIFEST_DIR"));
        let mut tls_settings =
            pingora_core::listeners::tls::TlsSettings::intermediate(&cert_path, &key_path).unwrap();
        tls_settings.enable_h2();
        proxy_service_https.add_tls_with_settings("0.0.0.0:6150", None, tls_settings);
        proxy_service_https_opt = Some(Box::new(proxy_service_https))
    }

    let mut proxy_service_cache =
        pingora_proxy::http_proxy_service(&my_server.configuration, ExampleProxyCache {});
    proxy_service_cache.add_tcp("0.0.0.0:6148");

    // H2C-enabled cache proxy on port 6154
    let mut proxy_service_cache_h2c =
        pingora_proxy::http_proxy_service(&my_server.configuration, ExampleProxyCache {});
    let cache_h2c_logic = proxy_service_cache_h2c.app_logic_mut().unwrap();
    let mut cache_h2c_options = HttpServerOptions::default();
    cache_h2c_options.h2c = true;
    cache_h2c_logic.server_options = Some(cache_h2c_options);
    proxy_service_cache_h2c.add_tcp("0.0.0.0:6154");

    #[cfg(feature = "any_tls")]
    {
        let cert_path = format!("{}/tests/keys/server.crt", env!("CARGO_MANIFEST_DIR"));
        let key_path = format!("{}/tests/keys/key.pem", env!("CARGO_MANIFEST_DIR"));

        let mut tls_settings =
            pingora_core::listeners::tls::TlsSettings::intermediate(&cert_path, &key_path).unwrap();
        tls_settings.enable_h2();
        proxy_service_cache.add_tls_with_settings("0.0.0.0:6153", None, tls_settings);
    }

    let mut services: Vec<Box<dyn ServiceWithDependents>> = vec![
        Box::new(proxy_service_h2c),
        Box::new(proxy_service_http),
        Box::new(proxy_service_http_connect),
        Box::new(proxy_service_cache),
        Box::new(proxy_service_cache_h2c),
    ];

    if let Some(proxy_service_https) = proxy_service_https_opt {
        services.push(proxy_service_https)
    }

    set_compression_dict_path("tests/headers.dict");
    my_server.add_services(services);
    my_server.run_forever();
}

pub struct Server {
    pub handle: thread::JoinHandle<()>,
}

impl Server {
    pub fn start() -> Self {
        let server_handle = thread::spawn(|| {
            test_main();
        });
        let addr = "127.0.0.1:6147".parse().unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            if std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(100)).is_ok() {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "test proxy failed to start within 10s"
            );
            thread::sleep(Duration::from_millis(50));
        }
        Server {
            handle: server_handle,
        }
    }
}

#[cfg(feature = "s2n")]
pub struct PskTlsServer {
    pub handle: thread::JoinHandle<()>,
}

#[cfg(feature = "s2n")]
impl PskTlsServer {
    pub fn start() -> Self {
        use std::sync::mpsc;
        use std::time::Duration;

        // Use a channel to wait for the server to bind its port.
        // A TCP probe can't be used here because the TLS acceptor would
        // try to handshake the probe connection, fail, and panic.
        let (tx, rx) = mpsc::channel();
        let server_handle = thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(Self::run_server(tx));
        });

        // Wait up to 10s for the server to signal it has bound the port.
        rx.recv_timeout(Duration::from_secs(10))
            .expect("PSK TLS server failed to start within 10s");

        PskTlsServer {
            handle: server_handle,
        }
    }

    async fn run_server(ready_tx: std::sync::mpsc::Sender<()>) {
        use pingora_core::{protocols::tls::S2NConnectionBuilder, tls::TlsAcceptor};
        use pingora_core::{
            protocols::tls::{Psk, PskConfig, PskType},
            tls::{Config, PskHmac, S2NPolicy, DEFAULT_TLS13},
        };
        use tokio::net::TcpListener;

        let psk = Psk::new(
            TEST_PSK_IDENTITY.to_string(),
            TEST_PSK_SECRET.as_bytes().to_vec(),
            PskHmac::SHA256,
        );
        let psk_config = Arc::new(PskConfig::new(vec![psk]));

        let addr: std::net::SocketAddr = "127.0.0.1:6151".parse().unwrap();
        let listener = TcpListener::bind(addr).await.unwrap();
        let _ = ready_tx.send(()); // signal: port is bound

        let mut config_builder = Config::builder();
        unsafe {
            config_builder.disable_x509_verification();
        }
        config_builder.set_security_policy(&DEFAULT_TLS13).unwrap();
        let config = config_builder.build().unwrap();

        let connection_builder = S2NConnectionBuilder {
            config: config.clone(),
            psk_config: Some(psk_config.clone()),
            security_policy: None,
        };

        let acceptor = TlsAcceptor::new(connection_builder);

        loop {
            use tokio::io::AsyncWriteExt;
            let (tcp_stream, _) = listener.accept().await.unwrap();
            // Don't panic on handshake failure — a stale connection or probe
            // shouldn't take down the server for subsequent real connections.
            let mut stream = match acceptor.clone().accept(tcp_stream).await {
                Ok(s) => s,
                Err(e) => {
                    log::warn!("PSK TLS server: handshake failed: {e}");
                    continue;
                }
            };
            let response = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello";
            let _ = stream.write_all(response).await;
            let _ = stream.shutdown().await;
        }
    }
}

// FIXME: this still allows multiple servers to spawn across integration tests
pub static TEST_SERVER: Lazy<Server> = Lazy::new(Server::start);
#[cfg(feature = "s2n")]
pub static TEST_PSK_TLS_SERVER: Lazy<PskTlsServer> = Lazy::new(PskTlsServer::start);
use super::mock_origin::MOCK_ORIGIN;

pub async fn init_proxy() {
    tokio::task::spawn_blocking(|| {
        let _ = *TEST_SERVER;
    })
    .await
    .expect("test proxy startup task panicked");
}

pub fn init() {
    let _ = *TEST_SERVER;
    let _ = *MOCK_ORIGIN;
    #[cfg(feature = "s2n")]
    let _ = *TEST_PSK_TLS_SERVER;
}
