// Pingora Proxy with Caching - Cache Lock Bug Reproduction
//
// This proxy demonstrates the cache lock client disconnect bug.
// When multiple requests hit the same uncached URL:
//   1. First request (writer) acquires the cache lock and fetches from origin
//   2. Subsequent requests (readers) wait on the cache lock
//   3. BUG: If a reader's client disconnects, the server keeps waiting
//      on the cache lock until it times out (default 60s)
//
// With the fix, readers should detect client disconnect and exit early.

use async_trait::async_trait;
use clap::Parser;
use log::info;
use once_cell::sync::Lazy;
use std::time::Duration;

use pingora_cache::cache_control::CacheControl;
use pingora_cache::filters::resp_cacheable;
use pingora_cache::lock::{CacheKeyLockImpl, CacheLock};
use pingora_cache::{CacheMetaDefaults, CacheOptionOverrides, MemCache, RespCacheable};
use pingora_core::server::configuration::Opt;
use pingora_core::server::Server;
use pingora_core::upstreams::peer::HttpPeer;
use pingora_error::Result;
use pingora_http::ResponseHeader;
use pingora_proxy::{ProxyHttp, Session};

#[derive(Parser, Debug)]
#[command(name = "proxy")]
#[command(about = "Pingora proxy with caching for cache lock bug reproduction")]
struct Args {
    /// Port to listen on
    #[arg(short, long, default_value = "6148")]
    port: u16,

    /// Origin server port
    #[arg(short, long, default_value = "8000")]
    origin_port: u16,

    /// Cache lock age timeout in seconds (how long a writer can hold the lock)
    #[arg(long, default_value = "60")]
    lock_age_timeout: u64,

    /// Cache lock wait timeout in seconds (how long readers wait before giving up)
    #[arg(long, default_value = "60")]
    lock_wait_timeout: u64,
}

// Global cache backend (in-memory)
static CACHE_BACKEND: Lazy<MemCache> = Lazy::new(MemCache::new);

// Cache defaults: 60 second TTL
const CACHE_DEFAULT: CacheMetaDefaults =
    CacheMetaDefaults::new(|_| Some(Duration::from_secs(60)), 1, 1);

// Cache lock with configurable timeout
// Note: This is static for simplicity, configured via command line
static CACHE_LOCK: Lazy<Box<CacheKeyLockImpl>> = Lazy::new(|| {
    // Default to 60 second timeout - this shows the bug most clearly
    // In production you might use a shorter timeout like 2s
    CacheLock::new_boxed(Duration::from_secs(60))
});

pub struct CacheProxy {
    origin_port: u16,
}

pub struct CacheProxyCTX {
    upstream_status: Option<u16>,
}

#[async_trait]
impl ProxyHttp for CacheProxy {
    type CTX = CacheProxyCTX;

    fn new_ctx(&self) -> Self::CTX {
        CacheProxyCTX {
            upstream_status: None,
        }
    }

    async fn upstream_peer(
        &self,
        _session: &mut Session,
        _ctx: &mut Self::CTX,
    ) -> Result<Box<HttpPeer>> {
        let peer = Box::new(HttpPeer::new(
            format!("127.0.0.1:{}", self.origin_port),
            false,
            String::new(),
        ));
        Ok(peer)
    }

    fn request_cache_filter(&self, session: &mut Session, _ctx: &mut Self::CTX) -> Result<()> {
        // Check for bypass header
        if session.get_header_bytes("x-bypass-cache") != b"" {
            info!("Cache bypass requested");
            return Ok(());
        }

        // Set up cache lock overrides
        let mut overrides = CacheOptionOverrides::default();
        // Wait timeout controls how long readers wait on the lock
        overrides.wait_timeout = Some(Duration::from_secs(60));

        // Enable caching with the cache lock
        // The cache lock is the key component that causes the bug
        session.cache.enable(
            &*CACHE_BACKEND,
            None, // no eviction manager
            None, // no predictor
            Some(CACHE_LOCK.as_ref()), // IMPORTANT: Enable cache lock
            Some(overrides),
        );

        info!(
            "Cache enabled for request: {}",
            session.req_header().uri.path()
        );

        Ok(())
    }

    fn response_cache_filter(
        &self,
        _session: &Session,
        resp: &ResponseHeader,
        _ctx: &mut Self::CTX,
    ) -> Result<RespCacheable> {
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
        _session: &mut Session,
        _upstream_response: &mut ResponseHeader,
        ctx: &mut Self::CTX,
    ) -> Result<()> {
        ctx.upstream_status = Some(_upstream_response.status.into());
        Ok(())
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
        // Add cache status header for debugging
        if session.cache.enabled() {
            let phase = session.cache.phase();
            let status = format!("{:?}", phase);
            upstream_response.insert_header("x-cache-phase", status)?;
        }

        // Add cache lock duration header if applicable
        if let Some(d) = session.cache.lock_duration() {
            upstream_response
                .insert_header("x-cache-lock-time-ms", format!("{}", d.as_millis()))?;
            info!("Cache lock wait time: {:?}", d);
        }

        // Add upstream status
        if let Some(up_stat) = ctx.upstream_status {
            upstream_response.insert_header("x-upstream-status", up_stat.to_string())?;
        }

        Ok(())
    }

    async fn logging(
        &self,
        session: &mut Session,
        _e: Option<&pingora_error::Error>,
        ctx: &mut Self::CTX,
    ) {
        let path = session.req_header().uri.path().to_string();
        let cache_phase = if session.cache.enabled() {
            format!("{:?}", session.cache.phase())
        } else {
            "disabled".to_string()
        };
        let lock_time = session
            .cache
            .lock_duration()
            .map(|d| format!("{}ms", d.as_millis()))
            .unwrap_or_else(|| "none".to_string());

        info!(
            "Request completed: path={}, cache_phase={}, lock_wait={}, upstream_status={:?}",
            path, cache_phase, lock_time, ctx.upstream_status
        );
    }
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let args = Args::parse();

    info!("Starting Pingora cache proxy");
    info!("  Listening on: 127.0.0.1:{}", args.port);
    info!("  Origin: 127.0.0.1:{}", args.origin_port);
    info!("  Cache lock age timeout: {}s", args.lock_age_timeout);
    info!("  Cache lock wait timeout: {}s", args.lock_wait_timeout);
    info!("");
    info!("BUG REPRODUCTION:");
    info!("  If a client disconnects while waiting on a cache lock,");
    info!("  the server should release resources immediately.");
    info!("  WITHOUT the fix, the server waits for the full lock timeout");
    info!("  (up to {} seconds) before releasing resources.", args.lock_age_timeout);
    info!("");

    // Create server with minimal config
    let opt = Opt::default();
    let mut server = Server::new(Some(opt)).unwrap();
    server.bootstrap();

    let proxy = CacheProxy {
        origin_port: args.origin_port,
    };

    let mut proxy_service = pingora_proxy::http_proxy_service(&server.configuration, proxy);
    proxy_service.add_tcp(&format!("0.0.0.0:{}", args.port));

    server.add_service(proxy_service);
    server.run_forever();
}
