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

//! Cache Put module

use crate::max_file_size::ERR_RESPONSE_TOO_LARGE;
use crate::*;
use bytes::Bytes;
use http::header;
use pingora_core::protocols::http::{
    custom::server::Session as DownstreamSession, v1::common::header_value_content_length,
    HttpTask, ServerSession,
};
use pingora_error::Error;

/// The interface to define cache put behavior
pub trait CachePut {
    /// Return whether to cache the asset according to the given response header.
    fn cacheable(&self, response: ResponseHeader) -> RespCacheable {
        let cc = cache_control::CacheControl::from_resp_headers(&response);
        filters::resp_cacheable(cc.as_ref(), response, false, Self::cache_defaults())
    }

    /// Return the [CacheMetaDefaults]
    fn cache_defaults() -> &'static CacheMetaDefaults;

    /// Put interesting things in the span given the parsed response header.
    fn trace_header(&mut self, _response: &ResponseHeader) {}
}

use parse_response::ResponseParse;

/// The cache put context
pub struct CachePutCtx<C: CachePut> {
    cache_put: C, // the user defined cache put behavior
    key: CacheKey,
    storage: &'static (dyn storage::Storage + Sync), // static for now
    eviction: Option<&'static (dyn eviction::EvictionManager + Sync)>,
    miss_handler: Option<MissHandler>,
    max_file_size_tracker: Option<MaxFileSizeTracker>,
    meta: Option<CacheMeta>,
    parser: ResponseParse,
    // FIXME: cache put doesn't have cache lock but some storage cannot handle concurrent put
    // to the same asset.
    trace: trace::Span,
}

impl<C: CachePut> CachePutCtx<C> {
    /// Create a new [CachePutCtx]
    pub fn new(
        cache_put: C,
        key: CacheKey,
        storage: &'static (dyn storage::Storage + Sync),
        eviction: Option<&'static (dyn eviction::EvictionManager + Sync)>,
        trace: trace::Span,
    ) -> Self {
        CachePutCtx {
            cache_put,
            key,
            storage,
            eviction,
            miss_handler: None,
            max_file_size_tracker: None,
            meta: None,
            parser: ResponseParse::new(),
            trace,
        }
    }

    /// Set the max cacheable size limit
    pub fn set_max_file_size_bytes(&mut self, max_file_size_bytes: usize) {
        self.max_file_size_tracker = Some(MaxFileSizeTracker::new(max_file_size_bytes));
    }

    /// Set the maximum number of fields accepted in the response head.
    ///
    /// When cache puts consume a response independently from an upstream
    /// [`ConnectorOptions`](pingora_core::connectors::ConnectorOptions), keep
    /// this limit aligned with
    /// [`ConnectorOptions::h1_response_parser_config`](pingora_core::connectors::ConnectorOptions::h1_response_parser_config).
    pub fn set_header_count_limit(&mut self, limit: crate::HeaderLimit) {
        self.parser.set_header_count_limit(limit);
    }

    /// Set the HTTP/1 response parser configuration.
    ///
    /// When cache puts consume a response independently from an upstream
    /// [`ConnectorOptions`](pingora_core::connectors::ConnectorOptions), keep
    /// this configuration aligned with
    /// [`ConnectorOptions::h1_response_parser_config`](pingora_core::connectors::ConnectorOptions::h1_response_parser_config).
    pub fn set_response_parser_config(&mut self, config: crate::ResponseParserConfig) {
        self.parser.set_response_parser_config(config);
    }

    async fn put_header(&mut self, meta: CacheMeta) -> Result<()> {
        #[cfg_attr(not(feature = "trace"), allow(unused_mut))]
        let mut trace = self.trace.child("cache put header", |o| o.start());
        let miss_handler = self
            .storage
            .get_miss_handler(&self.key, &meta, &trace.handle())
            .await?;
        trace::tag_span_with_meta(&mut trace, &meta);
        self.miss_handler = Some(miss_handler);
        self.meta = Some(meta);
        Ok(())
    }

    async fn put_body(&mut self, data: Bytes, eof: bool) -> Result<()> {
        // fail if writing the body would exceed the max_file_size_bytes
        if let Some(size_tracker) = self.max_file_size_tracker.as_mut() {
            let body_size_allowed = size_tracker.add_body_bytes(data.len());
            if !body_size_allowed {
                return Error::e_explain(
                    ERR_RESPONSE_TOO_LARGE,
                    format!(
                        "writing data of size {} bytes would exceed max file size of {} bytes",
                        data.len(),
                        size_tracker.max_file_size_bytes(),
                    ),
                );
            }
        }

        let miss_handler = self.miss_handler.as_mut().unwrap();
        miss_handler.write_body(data, eof).await
    }

    async fn finish(&mut self) -> Result<()> {
        let Some(miss_handler) = self.miss_handler.take() else {
            // no miss_handler, uncacheable
            return Ok(());
        };
        // Prepared before `finish` consumes the miss handler, which applies it to eviction once
        // the write succeeds.
        let completion = crate::storage::MissCompletion::new(
            crate::eviction::CacheEntryKey::from_entry_id(
                self.key.to_compact(),
                miss_handler.entry_id(),
            ),
            self.meta
                .as_ref()
                .expect("meta set with the miss handler")
                .0
                .internal
                .fresh_until,
            self.eviction,
            self.storage,
            self.trace.handle(),
            "cache put eviction",
        );
        miss_handler.finish_with_eviction(completion).await?;
        Ok(())
    }

    fn trace_header(&mut self, header: &ResponseHeader) {
        self.trace.set_tag(|| {
            Tag::new(
                "cache-control",
                header
                    .headers
                    .get_all(http::header::CACHE_CONTROL)
                    .into_iter()
                    .map(|v| String::from_utf8_lossy(v.as_bytes()).to_string())
                    .collect::<Vec<_>>()
                    .join(","),
            )
        });
    }

    async fn do_cache_put(&mut self, data: &[u8]) -> Result<Option<NoCacheReason>> {
        let tasks = self.parser.inject_data(data)?;
        for task in tasks {
            match task {
                HttpTask::Header(header, _eos) => {
                    self.trace_header(&header);
                    match self.cache_put.cacheable(*header) {
                        RespCacheable::Cacheable(meta) => {
                            if let Some(max_file_size_tracker) = &self.max_file_size_tracker {
                                let content_length_hdr = meta.headers().get(header::CONTENT_LENGTH);
                                if let Some(content_length) =
                                    header_value_content_length(content_length_hdr)
                                {
                                    if content_length > max_file_size_tracker.max_file_size_bytes()
                                    {
                                        return Ok(Some(NoCacheReason::ResponseTooLarge));
                                    }
                                }
                            }

                            self.put_header(meta).await?;
                        }
                        RespCacheable::Uncacheable(reason) => {
                            return Ok(Some(reason));
                        }
                    }
                }
                HttpTask::Body(data, eos) => {
                    if let Some(data) = data {
                        self.put_body(data, eos).await?;
                    }
                }
                _ => {
                    panic!("unexpected HttpTask during cache put {task:?}");
                }
            }
        }
        Ok(None)
    }

    /// Start the cache put logic for the given request
    ///
    /// This function will start to read the request body to put into cache.
    /// Return:
    /// - `Ok(None)` when the payload will be cache.
    /// - `Ok(Some(reason))` when the payload is not cacheable
    pub async fn cache_put<DS: DownstreamSession>(
        &mut self,
        session: &mut ServerSession<DS>,
    ) -> Result<Option<NoCacheReason>> {
        let mut no_cache_reason = None;
        while let Some(data) = session.read_request_body().await? {
            if no_cache_reason.is_some() {
                // even uncacheable, the entire body needs to be drains for 1. downstream
                // not throwing errors 2. connection reuse
                continue;
            }
            no_cache_reason = self.do_cache_put(&data).await?
        }
        self.parser.finish()?;
        self.finish().await?;

        if let Some(reason) = no_cache_reason {
            self.trace
                .set_tag(|| Tag::new("uncacheable_reason", reason.as_str()));
        }

        Ok(no_cache_reason)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::eviction::CacheEntryKey;
    use crate::tests::{
        test_meta, DetachedMissHandler, PurgeRecordingStorage, RecordingEviction,
        ScriptedMissHandler,
    };
    use crate::trace::Span;
    use once_cell::sync::Lazy;
    use pingora_error::ErrorType;
    use tokio::sync::oneshot;

    struct TestCachePut();
    impl CachePut for TestCachePut {
        fn cache_defaults() -> &'static CacheMetaDefaults {
            const DEFAULT: CacheMetaDefaults =
                CacheMetaDefaults::new(|_| Some(Duration::from_secs(1)), 1, 1);
            &DEFAULT
        }
    }

    type TestCachePutCtx = CachePutCtx<TestCachePut>;
    static CACHE_BACKEND: Lazy<MemCache> = Lazy::new(MemCache::new);

    fn put_with_miss_handler(
        handler: MissHandler,
        eviction: Option<&'static RecordingEviction>,
        storage: &'static PurgeRecordingStorage,
    ) -> TestCachePutCtx {
        let mut put = TestCachePutCtx::new(
            TestCachePut(),
            CacheKey::new("completed-put", ""),
            storage,
            eviction.map(|eviction| eviction as &'static (dyn eviction::EvictionManager + Sync)),
            Span::inactive(),
        );
        put.miss_handler = Some(handler);
        put.meta = Some(test_meta(SystemTime::now()));
        put
    }

    #[tokio::test]
    async fn put_completion_applies_only_successful_finishes() {
        for finish in [
            Ok(MissFinishType::Created(3)),
            Ok(MissFinishType::Appended(2, Some(5))),
            Err(Error::new(ErrorType::WriteError)),
        ] {
            let success = finish.is_ok();
            let eviction = Box::leak(Box::new(RecordingEviction::default()));
            let storage = Box::leak(Box::new(PurgeRecordingStorage::default()));
            let handler = Box::new(ScriptedMissHandler(finish));
            let mut put = put_with_miss_handler(handler, Some(eviction), storage);

            assert_eq!(put.finish().await.is_ok(), success);
            assert_eq!(eviction.updates(), usize::from(success));
        }
    }

    /// Finishing a put needs a Tokio runtime only to purge what a successful write evicts.
    #[test]
    fn put_finishes_without_a_runtime_when_nothing_is_purged() {
        let eviction: &'static RecordingEviction = Box::leak(Box::default());
        for (eviction, finish) in [
            (None, Ok(MissFinishType::Created(3))),
            (Some(eviction), Err(Error::new(ErrorType::WriteError))),
        ] {
            let success = finish.is_ok();
            let storage = Box::leak(Box::new(PurgeRecordingStorage::default()));
            let handler = Box::new(ScriptedMissHandler(finish));
            let mut put = put_with_miss_handler(handler, eviction, storage);

            assert_eq!(futures::executor::block_on(put.finish()).is_ok(), success);
        }
    }

    /// A put that its storage finishes on a worker keeps its accounting, and the purges of the
    /// entries that accounting evicts, after the caller's future is dropped.
    #[tokio::test]
    async fn storage_completes_a_put_whose_caller_is_dropped() {
        let victim = CacheEntryKey::key_only(CacheKey::new("victim", "").to_compact());
        let eviction = Box::leak(Box::new(RecordingEviction::evicting(victim.clone())));
        let storage = Box::leak(Box::new(PurgeRecordingStorage::default()));
        let (release, released) = oneshot::channel();
        let handler = Box::new(DetachedMissHandler {
            release: released,
            finish: MissFinishType::Appended(2, None),
        });
        let mut put = put_with_miss_handler(handler, Some(eviction), storage);
        let mut finish = tokio_test::task::spawn(put.finish());

        tokio_test::assert_pending!(finish.poll());
        drop(finish);
        assert_eq!(eviction.updates(), 0);
        release.send(()).unwrap();
        let purged = storage.purged(1).await;
        assert_eq!(eviction.updates(), 1);
        assert!(
            matches!(purged.as_slice(), [(item, PurgeType::Eviction)] if *item == victim),
            "{purged:?}"
        );
    }

    #[tokio::test]
    async fn test_cache_put() {
        let key = CacheKey::new("a", "1");
        let span = Span::inactive();
        let put = TestCachePut();
        let mut ctx = TestCachePutCtx::new(put, key.clone(), &*CACHE_BACKEND, None, span);
        let payload = b"HTTP/1.1 200 OK\r\n\
        Date: Thu, 26 Apr 2018 05:42:05 GMT\r\n\
        Content-Type: text/html; charset=utf-8\r\n\
        Connection: keep-alive\r\n\
        X-Frame-Options: SAMEORIGIN\r\n\
        Cache-Control: public, max-age=1\r\n\
        Server: origin-server\r\n\
        Content-Length: 4\r\n\r\nrust";
        // here we skip mocking a real http session for simplicity
        let res = ctx.do_cache_put(payload).await.unwrap();
        assert!(res.is_none()); // cacheable
        ctx.parser.finish().unwrap();
        ctx.finish().await.unwrap();

        let span = Span::inactive();
        let (meta, mut hit) = CACHE_BACKEND
            .lookup(&key, &span.handle())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            meta.headers().get("date").unwrap(),
            "Thu, 26 Apr 2018 05:42:05 GMT"
        );
        let data = hit.read_body().await.unwrap().unwrap();
        assert_eq!(data, "rust");
    }

    #[tokio::test]
    async fn test_cache_put_uncacheable() {
        let key = CacheKey::new("a", "1");
        let span = Span::inactive();
        let put = TestCachePut();
        let mut ctx = TestCachePutCtx::new(put, key.clone(), &*CACHE_BACKEND, None, span);
        let payload = b"HTTP/1.1 200 OK\r\n\
        Date: Thu, 26 Apr 2018 05:42:05 GMT\r\n\
        Content-Type: text/html; charset=utf-8\r\n\
        Connection: keep-alive\r\n\
        X-Frame-Options: SAMEORIGIN\r\n\
        Cache-Control: no-store\r\n\
        Server: origin-server\r\n\
        Content-Length: 4\r\n\r\nrust";
        // here we skip mocking a real http session for simplicity
        let no_cache = ctx.do_cache_put(payload).await.unwrap().unwrap();
        assert_eq!(no_cache, NoCacheReason::OriginNotCache);
        ctx.parser.finish().unwrap();
        ctx.finish().await.unwrap();
    }

    #[tokio::test]
    async fn test_cache_put_204_invalid_body() {
        let key = CacheKey::new("b", "1");
        let span = Span::inactive();
        let put = TestCachePut();
        let mut ctx = TestCachePutCtx::new(put, key.clone(), &*CACHE_BACKEND, None, span);
        let payload = b"HTTP/1.1 204 OK\r\n\
        Date: Thu, 26 Apr 2018 05:42:05 GMT\r\n\
        Content-Type: text/html; charset=utf-8\r\n\
        Connection: keep-alive\r\n\
        X-Frame-Options: SAMEORIGIN\r\n\
        Cache-Control: public, max-age=1\r\n\
        Server: origin-server\r\n\
        Content-Length: 4\r\n\r\n";
        // here we skip mocking a real http session for simplicity
        let res = ctx.do_cache_put(payload).await.unwrap();
        assert!(res.is_none()); // cacheable
                                // 204 should not have body, invalid client input may try to pass one
        let res = ctx.do_cache_put(b"rust").await.unwrap();
        assert!(res.is_none()); // still cacheable
        ctx.parser.finish().unwrap();
        ctx.finish().await.unwrap();

        let span = Span::inactive();
        let (meta, mut hit) = CACHE_BACKEND
            .lookup(&key, &span.handle())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            meta.headers().get("date").unwrap(),
            "Thu, 26 Apr 2018 05:42:05 GMT"
        );
        // just treated as empty body
        // (TODO: should we reset content-length/transfer-encoding
        // headers on 204/304?)
        let data = hit.read_body().await.unwrap().unwrap();
        assert!(data.is_empty());
    }

    #[tokio::test]
    async fn test_cache_put_extra_body() {
        let key = CacheKey::new("c", "1");
        let span = Span::inactive();
        let put = TestCachePut();
        let mut ctx = TestCachePutCtx::new(put, key.clone(), &*CACHE_BACKEND, None, span);
        let payload = b"HTTP/1.1 200 OK\r\n\
        Date: Thu, 26 Apr 2018 05:42:05 GMT\r\n\
        Content-Type: text/html; charset=utf-8\r\n\
        Connection: keep-alive\r\n\
        X-Frame-Options: SAMEORIGIN\r\n\
        Cache-Control: public, max-age=1\r\n\
        Server: origin-server\r\n\
        Content-Length: 4\r\n\r\n";
        // here we skip mocking a real http session for simplicity
        let res = ctx.do_cache_put(payload).await.unwrap();
        assert!(res.is_none()); // cacheable
                                // pass in more extra request body that needs to be drained
        let res = ctx.do_cache_put(b"rustab").await.unwrap();
        assert!(res.is_none()); // still cacheable
        let res = ctx.do_cache_put(b"cdef").await.unwrap();
        assert!(res.is_none()); // still cacheable
        ctx.parser.finish().unwrap();
        ctx.finish().await.unwrap();

        let span = Span::inactive();
        let (meta, mut hit) = CACHE_BACKEND
            .lookup(&key, &span.handle())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            meta.headers().get("date").unwrap(),
            "Thu, 26 Apr 2018 05:42:05 GMT"
        );
        let data = hit.read_body().await.unwrap().unwrap();
        // body only contains specified content-length bounds
        assert_eq!(data, "rust");
    }
}

// maybe this can simplify some logic in pingora::h1

mod parse_response {
    use super::*;
    use bstr::ByteSlice;
    use bytes::BytesMut;
    use pingora_error::{
        Error,
        ErrorType::{self, *},
    };
    use pingora_http::h1::{HeaderLimit, ResponseParserConfig};
    use pingora_http_parser::{
        parse_response, Error as ParseError, HeaderStorage, Status as ParseStatus,
    };

    pub const INCOMPLETE_BODY: ErrorType = ErrorType::new("IncompleteHttpBody");

    const INIT_HEADER_BUF_SIZE: usize = 4096;
    const MAX_HEADER_SIZE: usize = 1_048_575;

    fn response_header_exceeds_size_limit(buf: &[u8]) -> bool {
        if buf.len() <= MAX_HEADER_SIZE {
            return false;
        }

        let bounded = &buf[..MAX_HEADER_SIZE];
        !bounded.iter().enumerate().any(|(index, &byte)| {
            byte == b'\n'
                && matches!(
                    bounded.get(index + 1..),
                    Some([b'\n', ..]) | Some([b'\r', b'\n', ..])
                )
        })
    }

    #[derive(Debug, Clone, Copy, PartialEq)]
    enum ParseState {
        Init,
        PartialHeader,
        PartialBodyContentLength(usize, usize),
        PartialBody(usize),
        Done(usize),
        Invalid(ParseError),
        InvalidHeaderSize,
    }

    impl ParseState {
        fn is_done(&self) -> bool {
            matches!(self, Self::Done(_))
        }
        fn read_header(&self) -> bool {
            matches!(self, Self::Init | Self::PartialHeader)
        }
        fn read_body(&self) -> bool {
            matches!(
                self,
                Self::PartialBodyContentLength(..) | Self::PartialBody(_)
            )
        }
    }

    pub(super) struct ResponseParse {
        state: ParseState,
        buf: BytesMut,
        header_bytes: Bytes,
        parser_config: ResponseParserConfig,
    }

    impl ResponseParse {
        pub fn new() -> Self {
            Self::new_with_parser_config(ResponseParserConfig::default())
        }

        pub fn new_with_parser_config(parser_config: ResponseParserConfig) -> Self {
            ResponseParse {
                state: ParseState::Init,
                buf: BytesMut::with_capacity(INIT_HEADER_BUF_SIZE),
                header_bytes: Bytes::new(),
                parser_config,
            }
        }

        pub fn set_header_count_limit(&mut self, limit: HeaderLimit) {
            self.parser_config.header_limit = limit;
        }

        pub fn set_response_parser_config(&mut self, config: ResponseParserConfig) {
            self.parser_config = config;
        }

        pub fn inject_data(&mut self, data: &[u8]) -> Result<Vec<HttpTask>> {
            if self.state.is_done() {
                // just ignore extra response body after parser is done
                // could be invalid body appended to a no-content status
                // or invalid body after content-length
                // TODO: consider propagating an error to the client
                return Ok(vec![]);
            }

            self.put_data(data);

            let mut tasks = vec![];
            while !self.state.is_done() {
                if self.state.read_header() {
                    let header = self.parse_header()?;
                    let Some(header) = header else {
                        break;
                    };
                    tasks.push(HttpTask::Header(Box::new(header), self.state.is_done()));
                } else if self.state.read_body() {
                    let body = self.parse_body()?;
                    let Some(body) = body else {
                        break;
                    };
                    tasks.push(HttpTask::Body(Some(body), self.state.is_done()));
                } else {
                    break;
                }
            }
            Ok(tasks)
        }

        fn put_data(&mut self, data: &[u8]) {
            use ParseState::*;
            if matches!(self.state, Done(_) | Invalid(_) | InvalidHeaderSize) {
                panic!("Wrong phase {:?}", self.state);
            }
            self.buf.extend_from_slice(data);
        }

        fn parse_header(&mut self) -> Result<Option<ResponseHeader>> {
            if response_header_exceeds_size_limit(&self.buf) {
                self.state = ParseState::InvalidHeaderSize;
                return Error::e_explain(
                    InvalidHTTPHeader,
                    format!("Response header larger than {MAX_HEADER_SIZE}"),
                );
            }

            let mut storage = HeaderStorage::new();
            let res = parse_response(&mut storage, &self.buf, self.parser_config);
            let res = match res {
                Ok(res) => res,
                Err(e) => {
                    self.state = ParseState::Invalid(e);
                    return Error::e_because(
                        InvalidHTTPHeader,
                        format!("buf: {:?}", self.buf.as_bstr()),
                        e,
                    );
                }
            };

            let (split_to, resp) = match res {
                ParseStatus::Complete(s, response) => {
                    if s > MAX_HEADER_SIZE {
                        self.state = ParseState::InvalidHeaderSize;
                        return Error::e_explain(
                            InvalidHTTPHeader,
                            format!("Response header larger than {MAX_HEADER_SIZE}"),
                        );
                    }
                    (s, response)
                }
                ParseStatus::Partial => {
                    self.state = ParseState::PartialHeader;
                    return Ok(None);
                }
            };
            // safe to unwrap, valid response always has code set.
            let mut response =
                ResponseHeader::build(resp.code().unwrap(), Some(resp.header_count())).unwrap();
            for header in resp.headers() {
                // TODO: consider hold a Bytes and all header values can be Bytes referencing the
                // original buffer without reallocation
                let header_value = pingora_http::header_value_from_slice(header.value);
                response.append_header(header.name.to_owned(), header_value)?;
            }
            // TODO: see above, we can make header value `Bytes` referencing header_bytes
            let header_bytes = self.buf.split_to(split_to).freeze();
            self.header_bytes = header_bytes;
            self.state = body_type(&response);

            Ok(Some(response))
        }

        fn parse_body(&mut self) -> Result<Option<Bytes>> {
            use ParseState::*;
            if self.buf.is_empty() {
                return Ok(None);
            }
            match self.state {
                Init | PartialHeader | Invalid(_) | InvalidHeaderSize => {
                    panic!("Wrong phase {:?}", self.state);
                }
                Done(_) => Ok(None),
                PartialBodyContentLength(total, mut seen) => {
                    let end = if total < self.buf.len() + seen {
                        // TODO: warn! more data than expected
                        total - seen
                    } else {
                        self.buf.len()
                    };
                    seen += end;
                    if seen >= total {
                        self.state = Done(seen);
                    } else {
                        self.state = PartialBodyContentLength(total, seen);
                    }
                    Ok(Some(self.buf.split_to(end).freeze()))
                }
                PartialBody(seen) => {
                    self.state = PartialBody(seen + self.buf.len());
                    Ok(Some(self.buf.split().freeze()))
                }
            }
        }

        pub fn finish(&mut self) -> Result<()> {
            if let ParseState::PartialBody(seen) = self.state {
                self.state = ParseState::Done(seen);
            }
            if !self.state.is_done() {
                Error::e_explain(INCOMPLETE_BODY, format!("{:?}", self.state))
            } else {
                Ok(())
            }
        }
    }

    fn body_type(resp: &ResponseHeader) -> ParseState {
        use http::StatusCode;

        if matches!(
            resp.status,
            StatusCode::NO_CONTENT | StatusCode::NOT_MODIFIED
        ) {
            // these status codes cannot have body by definition
            return ParseState::Done(0);
        }
        if let Some(cl) = resp.headers.get(http::header::CONTENT_LENGTH) {
            // ignore invalid header value
            if let Some(cl) = std::str::from_utf8(cl.as_bytes())
                .ok()
                .and_then(|cl| cl.parse::<usize>().ok())
            {
                return if cl == 0 {
                    ParseState::Done(0)
                } else {
                    ParseState::PartialBodyContentLength(cl, 0)
                };
            }
        }
        // HTTP/1.0 and chunked encoding are both treated as PartialBody
        // The response body payload should _not_ be chunked encoded
        // even if the Transfer-Encoding: chunked header is added
        ParseState::PartialBody(0)
    }

    #[cfg(test)]
    mod test {
        use super::*;

        #[test]
        fn test_basic_response() {
            let input = b"HTTP/1.1 200 OK\r\n\r\n";
            let mut parser = ResponseParse::new();
            let output = parser.inject_data(input).unwrap();
            assert_eq!(output.len(), 1);
            let HttpTask::Header(header, eos) = &output[0] else {
                panic!("{:?}", output);
            };
            assert_eq!(header.status, 200);
            assert!(!eos);

            let body = b"abc";
            let output = parser.inject_data(body).unwrap();
            assert_eq!(output.len(), 1);
            let HttpTask::Body(data, _eos) = &output[0] else {
                panic!("{:?}", output);
            };
            assert_eq!(data.as_ref().unwrap(), &body[..]);
            parser.finish().unwrap();
        }

        #[test]
        fn test_partial_response_headers() {
            let input = b"HTTP/1.1 200 OK\r\n";
            let mut parser = ResponseParse::new();
            let output = parser.inject_data(input).unwrap();
            // header is not complete
            assert_eq!(output.len(), 0);

            let output = parser
                .inject_data("Server: pingora\r\n\r\n".as_bytes())
                .unwrap();
            assert_eq!(output.len(), 1);
            let HttpTask::Header(header, eos) = &output[0] else {
                panic!("{:?}", output);
            };
            assert_eq!(header.status, 200);
            assert_eq!(header.headers.get("Server").unwrap(), "pingora");
            assert!(!eos);
        }

        #[test]
        fn test_invalid_headers() {
            let input = b"HTP/1.1 200 OK\r\nServer: pingora\r\n\r\n";
            let mut parser = ResponseParse::new();
            let output = parser.inject_data(input);
            // header is not complete
            assert!(output.is_err());
            match parser.state {
                ParseState::Invalid(ParseError::Version) => {}
                _ => panic!("should have failed to parse"),
            }
        }

        #[test]
        fn test_many_response_headers() {
            let mut input = b"HTTP/1.1 200 OK\r\n".to_vec();
            for i in 0..4096 {
                input.extend_from_slice(format!("x-{i}: value-{i}\r\n").as_bytes());
            }
            input.extend_from_slice(b"\r\n");

            let mut parser = ResponseParse::new_with_parser_config(ResponseParserConfig::new(
                HeaderLimit::Limited(4096),
            ));
            let output = parser.inject_data(&input).unwrap();
            let HttpTask::Header(header, _) = &output[0] else {
                panic!("{output:?}");
            };
            assert_eq!(header.headers.len(), 4096);
            assert_eq!(header.headers["x-4095"], "value-4095");
        }

        #[test]
        fn test_partial_response_header_size_limit() {
            let mut input = b"HTTP/1.1 200 OK\r\nx-large: ".to_vec();
            input.resize(MAX_HEADER_SIZE + 1, b'a');
            let mut parser = ResponseParse::new_with_parser_config(ResponseParserConfig::new(
                HeaderLimit::Unlimited,
            ));

            let error = parser.inject_data(&input).unwrap_err();
            assert_eq!(error.etype(), &InvalidHTTPHeader);
            assert_eq!(parser.state, ParseState::InvalidHeaderSize);
        }

        #[test]
        fn test_complete_response_header_size_limit() {
            let mut input = b"HTTP/1.1 200 OK\r\n".to_vec();
            for i in 0..4096 {
                input.extend_from_slice(format!("x-{i}: value-{i}\r\n").as_bytes());
            }
            input.extend_from_slice(b"x-large: ");
            input.resize(MAX_HEADER_SIZE - 3, b'a');
            input.extend_from_slice(b"\r\n\r\nbody");

            let mut parser = ResponseParse::new_with_parser_config(ResponseParserConfig::new(
                HeaderLimit::Unlimited,
            ));
            let error = parser.inject_data(&input).unwrap_err();
            assert_eq!(error.etype(), &InvalidHTTPHeader);
            assert_eq!(parser.state, ParseState::InvalidHeaderSize);
        }

        #[test]
        fn test_response_header_at_size_limit() {
            let mut input = b"HTTP/1.1 200 OK\r\nx-large: ".to_vec();
            input.resize(MAX_HEADER_SIZE - 4, b'a');
            input.extend_from_slice(b"\r\n\r\nbody");

            let mut parser = ResponseParse::new_with_parser_config(ResponseParserConfig::new(
                HeaderLimit::Unlimited,
            ));
            let output = parser.inject_data(&input).unwrap();
            assert!(matches!(output[0], HttpTask::Header(..)));
            assert!(matches!(output[1], HttpTask::Body(..)));
        }

        #[test]
        fn test_large_body_does_not_count_toward_header_size_limit() {
            let mut input = b"HTTP/1.1 200 OK\r\nContent-Length: 1048576\r\n\r\n".to_vec();
            input.resize(input.len() + MAX_HEADER_SIZE + 1, b'a');

            let mut parser = ResponseParse::new();
            let output = parser.inject_data(&input).unwrap();
            assert_eq!(output.len(), 2);
            assert!(matches!(output[0], HttpTask::Header(..)));
            assert!(matches!(output[1], HttpTask::Body(..)));
        }

        #[test]
        fn test_response_compatibility_syntax_is_opt_in() {
            let input =
                b"HTTP/1.1 200 OK\r\nServer : pingora\r\n folded\r\nContent-Length: 0\r\n\r\n";
            let mut parser = ResponseParse::new();
            assert!(parser.inject_data(input).is_err());

            let config = ResponseParserConfig::default()
                .with_spaces_after_header_name_in_responses(true)
                .with_obsolete_multiline_headers_in_responses(true);
            let mut parser = ResponseParse::new_with_parser_config(config);
            let output = parser.inject_data(input).unwrap();
            let HttpTask::Header(header, _) = &output[0] else {
                panic!("{output:?}");
            };
            assert_eq!(header.headers["server"], "pingora folded");
        }

        #[test]
        fn test_body_content_length() {
            let input = b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\n\r\nabc";
            let mut parser = ResponseParse::new();
            let output = parser.inject_data(input).unwrap();

            assert_eq!(output.len(), 2);
            let HttpTask::Header(header, _eos) = &output[0] else {
                panic!("{:?}", output);
            };
            assert_eq!(header.status, 200);

            let HttpTask::Body(data, eos) = &output[1] else {
                panic!("{:?}", output);
            };
            assert_eq!(data.as_ref().unwrap(), "abc");
            assert!(!eos);

            let output = parser.inject_data(b"def").unwrap();
            assert_eq!(output.len(), 1);
            let HttpTask::Body(data, eos) = &output[0] else {
                panic!("{:?}", output);
            };
            assert_eq!(data.as_ref().unwrap(), "def");
            assert!(eos);

            parser.finish().unwrap();
        }

        #[test]
        fn test_body_chunked() {
            let input = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nrust";
            let mut parser = ResponseParse::new();
            let output = parser.inject_data(input).unwrap();

            assert_eq!(output.len(), 2);
            let HttpTask::Header(header, _eos) = &output[0] else {
                panic!("{:?}", output);
            };
            assert_eq!(header.status, 200);

            let HttpTask::Body(data, eos) = &output[1] else {
                panic!("{:?}", output);
            };
            assert_eq!(data.as_ref().unwrap(), "rust");
            assert!(!eos);

            parser.finish().unwrap();
        }

        #[test]
        fn test_body_content_length_early() {
            let input = b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\n\r\nabc";
            let mut parser = ResponseParse::new();
            let output = parser.inject_data(input).unwrap();

            assert_eq!(output.len(), 2);
            let HttpTask::Header(header, _eos) = &output[0] else {
                panic!("{:?}", output);
            };
            assert_eq!(header.status, 200);

            let HttpTask::Body(data, eos) = &output[1] else {
                panic!("{:?}", output);
            };
            assert_eq!(data.as_ref().unwrap(), "abc");
            assert!(!eos);

            parser.finish().unwrap_err();
        }

        #[test]
        fn test_body_content_length_more_data() {
            let input = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nabc";
            let mut parser = ResponseParse::new();
            let output = parser.inject_data(input).unwrap();

            assert_eq!(output.len(), 2);
            let HttpTask::Header(header, _eos) = &output[0] else {
                panic!("{:?}", output);
            };
            assert_eq!(header.status, 200);

            let HttpTask::Body(data, eos) = &output[1] else {
                panic!("{:?}", output);
            };
            assert_eq!(data.as_ref().unwrap(), "ab");
            assert!(eos);

            // extra data is dropped without error
            parser.finish().unwrap();
        }

        #[test]
        fn test_body_chunked_partial_chunk() {
            let input = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nru";
            let mut parser = ResponseParse::new();
            let output = parser.inject_data(input).unwrap();

            assert_eq!(output.len(), 2);
            let HttpTask::Header(header, _eos) = &output[0] else {
                panic!("{:?}", output);
            };
            assert_eq!(header.status, 200);

            let HttpTask::Body(data, eos) = &output[1] else {
                panic!("{:?}", output);
            };
            assert_eq!(data.as_ref().unwrap(), "ru");
            assert!(!eos);

            let output = parser.inject_data(b"st\r\n").unwrap();
            assert_eq!(output.len(), 1);
            let HttpTask::Body(data, eos) = &output[0] else {
                panic!("{:?}", output);
            };
            assert_eq!(data.as_ref().unwrap(), "st\r\n");
            assert!(!eos);
        }

        #[test]
        fn test_no_body_content_length() {
            let input = b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n";
            let mut parser = ResponseParse::new();
            let output = parser.inject_data(input).unwrap();

            assert_eq!(output.len(), 1);
            let HttpTask::Header(header, eos) = &output[0] else {
                panic!("{:?}", output);
            };
            assert_eq!(header.status, 200);
            assert!(eos);

            parser.finish().unwrap();
        }

        #[test]
        fn test_no_body_304_no_content_length() {
            let input = b"HTTP/1.1 304 Not Modified\r\nCache-Control: public, max-age=10\r\n\r\n";
            let mut parser = ResponseParse::new();
            let output = parser.inject_data(input).unwrap();

            assert_eq!(output.len(), 1);
            let HttpTask::Header(header, eos) = &output[0] else {
                panic!("{:?}", output);
            };
            assert_eq!(header.status, 304);
            assert!(eos);

            parser.finish().unwrap();
        }

        #[test]
        fn test_204_with_chunked_body() {
            let input = b"HTTP/1.1 204 No Content\r\nCache-Control: public, max-age=10\r\nTransfer-Encoding: chunked\r\n\r\n";
            let mut parser = ResponseParse::new();
            let output = parser.inject_data(input).unwrap();

            assert_eq!(output.len(), 1);
            let HttpTask::Header(header, eos) = &output[0] else {
                panic!("{:?}", output);
            };
            assert_eq!(header.status, 204);
            assert!(eos);

            // 204 should not have a body, parser ignores bad input
            let output = parser.inject_data(b"4\r\nrust\r\n0\r\n\r\n").unwrap();
            assert!(output.is_empty());
            parser.finish().unwrap();
        }

        #[test]
        fn test_204_with_content_length() {
            let input = b"HTTP/1.1 204 No Content\r\nCache-Control: public, max-age=10\r\nContent-Length: 4\r\n\r\n";
            let mut parser = ResponseParse::new();
            let output = parser.inject_data(input).unwrap();

            assert_eq!(output.len(), 1);
            let HttpTask::Header(header, eos) = &output[0] else {
                panic!("{:?}", output);
            };
            assert_eq!(header.status, 204);
            assert!(eos);

            // 204 should not have a body, parser ignores bad input
            let output = parser.inject_data(b"rust").unwrap();
            assert!(output.is_empty());
            parser.finish().unwrap();
        }

        #[test]
        fn test_200_with_zero_content_length_more_data() {
            let input = b"HTTP/1.1 200 OK\r\nCache-Control: public, max-age=10\r\nContent-Length: 0\r\n\r\n";
            let mut parser = ResponseParse::new();
            let output = parser.inject_data(input).unwrap();

            assert_eq!(output.len(), 1);
            let HttpTask::Header(header, eos) = &output[0] else {
                panic!("{:?}", output);
            };
            assert_eq!(header.status, 200);
            assert!(eos);

            let output = parser.inject_data(b"rust").unwrap();
            assert!(output.is_empty());
            parser.finish().unwrap();
        }
    }
}
