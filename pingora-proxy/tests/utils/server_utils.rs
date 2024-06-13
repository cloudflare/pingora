// Copyright 2024 Cloudflare, Inc.
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

use super::cert;
use async_trait::async_trait;
use clap::Parser;
use http::header::VARY;
use http::HeaderValue;
use once_cell::sync::Lazy;
use pingora_cache::cache_control::CacheControl;
use pingora_cache::key::HashBinary;
use pingora_cache::VarianceBuilder;
use pingora_cache::{
    eviction::simple_lru::Manager, filters::resp_cacheable, lock::CacheLock, predictor::Predictor,
    set_compression_dict_path, CacheMeta, CacheMetaDefaults, CachePhase, MemCache, NoCacheReason,
    RespCacheable,
};
use pingora_core::apps::{HttpServerApp, HttpServerOptions};
use pingora_core::modules::http::compression::ResponseCompression;
use pingora_core::protocols::{l4::socket::SocketAddr, Digest};
use pingora_core::server::configuration::Opt;
use pingora_core::services::Service;
use pingora_core::upstreams::peer::HttpPeer;
use pingora_core::utils::CertKey;
use pingora_error::{Error, ErrorSource, Result};
use pingora_http::{RequestHeader, ResponseHeader};
use pingora_proxy::{ProxyHttp, Session};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

pub struct ExampleProxyHttps {}

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
        let alt = req.headers.get("alt").map_or("", |v| v.to_str().unwrap());

        let client_cert = session.get_header_bytes("client_cert");

        let mut peer = Box::new(HttpPeer::new(
            format!("127.0.0.1:{port}"),
            true,
            sni.to_string(),
        ));
        peer.options.alternative_cn = Some(alt.to_string());

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
            peer.client_cert_key = Some(Arc::new(CertKey::new(certs, key)));
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
        _fd: std::os::unix::io::RawFd,
        digest: Option<&Digest>,
        ctx: &mut CTX,
    ) -> Result<()> {
        connected_to_upstream_common(reused, digest, ctx)
    }
}

pub struct ExampleProxyHttp {}

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
        let req = session.req_header();
        let downstream_compression = req.headers.get("x-downstream-compression").is_some();
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

        if let Some(min_rate) = min_rate {
            session.set_min_send_rate(min_rate);
        }
        if let Some(write_timeout) = write_timeout {
            session.set_write_timeout(Duration::from_secs(write_timeout));
        }

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

    async fn upstream_peer(
        &self,
        session: &mut Session,
        _ctx: &mut Self::CTX,
    ) -> Result<Box<HttpPeer>> {
        let req = session.req_header();
        if req.headers.contains_key("x-uds-peer") {
            return Ok(Box::new(HttpPeer::new_uds(
                "/tmp/nginx-test.sock",
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

        Ok(peer)
    }

    async fn connected_to_upstream(
        &self,
        _http_session: &mut Session,
        reused: bool,
        _peer: &HttpPeer,
        _fd: std::os::unix::io::RawFd,
        digest: Option<&Digest>,
        ctx: &mut CTX,
    ) -> Result<()> {
        connected_to_upstream_common(reused, digest, ctx)
    }
}

static CACHE_BACKEND: Lazy<MemCache> = Lazy::new(MemCache::new);
const CACHE_DEFAULT: CacheMetaDefaults = CacheMetaDefaults::new(|_| Some(1), 1, 1);
static CACHE_PREDICTOR: Lazy<Predictor<32>> = Lazy::new(|| Predictor::new(5, None));
static EVICTION_MANAGER: Lazy<Manager> = Lazy::new(|| Manager::new(8192)); // 8192 bytes
static CACHE_LOCK: Lazy<CacheLock> =
    Lazy::new(|| CacheLock::new(std::time::Duration::from_secs(2)));
// Example of how one might restrict which fields can be varied on.
static CACHE_VARY_ALLOWED_HEADERS: Lazy<Option<HashSet<&str>>> =
    Lazy::new(|| Some(vec!["accept", "accept-encoding"].into_iter().collect()));

// #[allow(clippy::upper_case_acronyms)]
pub struct CacheCTX {
    upstream_status: Option<u16>,
}

pub struct ExampleProxyCache {}

#[async_trait]
impl ProxyHttp for ExampleProxyCache {
    type CTX = CacheCTX;
    fn new_ctx(&self) -> Self::CTX {
        CacheCTX {
            upstream_status: None,
        }
    }

    async fn upstream_peer(
        &self,
        session: &mut Session,
        _ctx: &mut Self::CTX,
    ) -> Result<Box<HttpPeer>> {
        let req = session.req_header();
        let port = req
            .headers
            .get("x-port")
            .map_or("8000", |v| v.to_str().unwrap());
        let peer = Box::new(HttpPeer::new(
            format!("127.0.0.1:{}", port),
            false,
            "".to_string(),
        ));
        Ok(peer)
    }

    fn request_cache_filter(&self, session: &mut Session, _ctx: &mut Self::CTX) -> Result<()> {
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
            .map(|_| &*CACHE_LOCK);
        session
            .cache
            .enable(&*CACHE_BACKEND, eviction, Some(&*CACHE_PREDICTOR), lock);

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

    fn response_cache_filter(
        &self,
        _session: &Session,
        resp: &ResponseHeader,
        _ctx: &mut Self::CTX,
    ) -> Result<RespCacheable> {
        let cc = CacheControl::from_resp_headers(resp);
        Ok(resp_cacheable(cc.as_ref(), resp, false, &CACHE_DEFAULT))
    }

    fn upstream_response_filter(
        &self,
        _session: &mut Session,
        upstream_response: &mut ResponseHeader,
        ctx: &mut Self::CTX,
    ) where
        Self::CTX: Send + Sync,
    {
        ctx.upstream_status = Some(upstream_response.status.into());
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
        if session.cache.enabled() {
            match session.cache.phase() {
                CachePhase::Hit => upstream_response.insert_header("x-cache-status", "hit")?,
                CachePhase::Miss => upstream_response.insert_header("x-cache-status", "miss")?,
                CachePhase::Stale => upstream_response.insert_header("x-cache-status", "stale")?,
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

    fn should_serve_stale(
        &self,
        _session: &mut Session,
        _ctx: &mut Self::CTX,
        error: Option<&Error>, // None when it is called during stale while revalidate
    ) -> bool {
        // enable serve stale while updating
        error.map_or(true, |e| e.esource() == &ErrorSource::Upstream)
    }

    fn is_purge(&self, session: &Session, _ctx: &Self::CTX) -> bool {
        session.req_header().method == "PURGE"
    }
}

fn test_main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let opts: Vec<String> = vec![
        "pingora-proxy".into(),
        "-c".into(),
        "tests/pingora_conf.yaml".into(),
    ];
    let mut my_server = pingora_core::server::Server::new(Some(Opt::parse_from(opts))).unwrap();
    my_server.bootstrap();

    let mut proxy_service_http =
        pingora_proxy::http_proxy_service(&my_server.configuration, ExampleProxyHttp {});
    proxy_service_http.add_tcp("0.0.0.0:6147");
    proxy_service_http.add_uds("/tmp/pingora_proxy.sock", None);

    let mut proxy_service_h2c =
        pingora_proxy::http_proxy_service(&my_server.configuration, ExampleProxyHttp {});

    let http_logic = proxy_service_h2c.app_logic_mut().unwrap();
    let mut http_server_options = HttpServerOptions::default();
    http_server_options.h2c = true;
    http_logic.server_options = Some(http_server_options);
    proxy_service_h2c.add_tcp("0.0.0.0:6146");

    let mut proxy_service_https =
        pingora_proxy::http_proxy_service(&my_server.configuration, ExampleProxyHttps {});
    proxy_service_https.add_tcp("0.0.0.0:6149");
    let cert_path = format!("{}/tests/keys/server.crt", env!("CARGO_MANIFEST_DIR"));
    let key_path = format!("{}/tests/keys/key.pem", env!("CARGO_MANIFEST_DIR"));
    let mut tls_settings =
        pingora_core::listeners::TlsSettings::intermediate(&cert_path, &key_path).unwrap();
    tls_settings.enable_h2();
    proxy_service_https.add_tls_with_settings("0.0.0.0:6150", None, tls_settings);

    let mut proxy_service_cache =
        pingora_proxy::http_proxy_service(&my_server.configuration, ExampleProxyCache {});
    proxy_service_cache.add_tcp("0.0.0.0:6148");

    let services: Vec<Box<dyn Service>> = vec![
        Box::new(proxy_service_h2c),
        Box::new(proxy_service_http),
        Box::new(proxy_service_https),
        Box::new(proxy_service_cache),
    ];

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
        Server {
            handle: server_handle,
        }
    }
}

// FIXME: this still allows multiple servers to spawn across integration tests
pub static TEST_SERVER: Lazy<Server> = Lazy::new(Server::start);
use super::mock_origin::MOCK_ORIGIN;

pub fn init() {
    let _ = *TEST_SERVER;
    let _ = *MOCK_ORIGIN;
}
