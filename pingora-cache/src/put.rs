// Copyright 2025 Cloudflare, Inc.
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

use crate::*;
use bytes::Bytes;
use http::header;
use log::warn;
use pingora_core::protocols::http::{
    v1::common::header_value_content_length, HttpTask, ServerSession,
};

/// The interface to define cache put behavior
pub trait CachePut {
    /// Return whether to cache the asset according to the given response header.
    fn cacheable(&self, response: ResponseHeader) -> RespCacheable {
        let cc = cache_control::CacheControl::from_resp_headers(&response);
        filters::resp_cacheable(cc.as_ref(), response, false, Self::cache_defaults())
    }

    /// Return the [CacheMetaDefaults]
    fn cache_defaults() -> &'static CacheMetaDefaults;
}

use parse_response::ResponseParse;

/// The cache put context
pub struct CachePutCtx<C: CachePut> {
    cache_put: C, // the user defined cache put behavior
    key: CacheKey,
    storage: &'static (dyn storage::Storage + Sync), // static for now
    eviction: Option<&'static (dyn eviction::EvictionManager + Sync)>,
    miss_handler: Option<MissHandler>,
    max_file_size_bytes: Option<usize>,
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
            max_file_size_bytes: None,
            meta: None,
            parser: ResponseParse::new(),
            trace,
        }
    }

    /// Set the max cacheable size limit
    pub fn set_max_file_size_bytes(&mut self, max_file_size_bytes: usize) {
        self.max_file_size_bytes = Some(max_file_size_bytes);
    }

    async fn put_header(&mut self, meta: CacheMeta) -> Result<()> {
        let trace = self.trace.child("cache put header", |o| o.start()).handle();
        let miss_handler = self
            .storage
            .get_miss_handler(&self.key, &meta, &trace)
            .await?;
        self.miss_handler = Some(
            if let Some(max_file_size_bytes) = self.max_file_size_bytes {
                Box::new(MaxFileSizeMissHandler::new(
                    miss_handler,
                    max_file_size_bytes,
                ))
            } else {
                miss_handler
            },
        );
        self.meta = Some(meta);
        Ok(())
    }

    async fn put_body(&mut self, data: Bytes, eof: bool) -> Result<()> {
        let miss_handler = self.miss_handler.as_mut().unwrap();
        miss_handler.write_body(data, eof).await
    }

    async fn finish(&mut self) -> Result<()> {
        let Some(miss_handler) = self.miss_handler.take() else {
            // no miss_handler, uncacheable
            return Ok(());
        };
        let finish = miss_handler.finish().await?;
        if let Some(eviction) = self.eviction.as_ref() {
            let cache_key = self.key.to_compact();
            let meta = self.meta.as_ref().unwrap();
            let evicted = match finish {
                MissFinishType::Appended(delta) => eviction.increment_weight(cache_key, delta),
                MissFinishType::Created(size) => {
                    eviction.admit(cache_key, size, meta.0.internal.fresh_until)
                }
            };
            // actual eviction can be done async
            let trace = self
                .trace
                .child("cache put eviction", |o| o.start())
                .handle();
            let storage = self.storage;
            tokio::task::spawn(async move {
                for item in evicted {
                    if let Err(e) = storage.purge(&item, PurgeType::Eviction, &trace).await {
                        warn!("Failed to purge {item} during eviction for cache put: {e}");
                    }
                }
            });
        }

        Ok(())
    }

    async fn do_cache_put(&mut self, data: &[u8]) -> Result<Option<NoCacheReason>> {
        let tasks = self.parser.inject_data(data)?;
        for task in tasks {
            match task {
                HttpTask::Header(header, _eos) => match self.cache_put.cacheable(*header) {
                    RespCacheable::Cacheable(meta) => {
                        if let Some(max_file_size_bytes) = self.max_file_size_bytes {
                            let content_length_hdr = meta.headers().get(header::CONTENT_LENGTH);
                            if let Some(content_length) =
                                header_value_content_length(content_length_hdr)
                            {
                                if content_length > max_file_size_bytes {
                                    return Ok(Some(NoCacheReason::ResponseTooLarge));
                                }
                            }
                        }

                        self.put_header(meta).await?;
                    }
                    RespCacheable::Uncacheable(reason) => {
                        return Ok(Some(reason));
                    }
                },
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
    pub async fn cache_put(
        &mut self,
        session: &mut ServerSession,
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
        Ok(no_cache_reason)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use cf_rustracing::span::Span;
    use once_cell::sync::Lazy;

    struct TestCachePut();
    impl CachePut for TestCachePut {
        fn cache_defaults() -> &'static CacheMetaDefaults {
            const DEFAULT: CacheMetaDefaults = CacheMetaDefaults::new(|_| Some(1), 1, 1);
            &DEFAULT
        }
    }

    type TestCachePutCtx = CachePutCtx<TestCachePut>;
    static CACHE_BACKEND: Lazy<MemCache> = Lazy::new(MemCache::new);

    #[tokio::test]
    async fn test_cache_put() {
        let key = CacheKey::new("", "a", "1");
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
        let key = CacheKey::new("", "a", "1");
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
        let key = CacheKey::new("", "b", "1");
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
        let key = CacheKey::new("", "c", "1");
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
    use bytes::BytesMut;
    use httparse::Status;
    use pingora_error::{
        Error,
        ErrorType::{self, *},
    };

    pub const INCOMPLETE_BODY: ErrorType = ErrorType::new("IncompleteHttpBody");

    const MAX_HEADERS: usize = 256;
    const INIT_HEADER_BUF_SIZE: usize = 4096;

    #[derive(Debug, Clone, Copy, PartialEq)]
    enum ParseState {
        Init,
        PartialHeader,
        PartialBodyContentLength(usize, usize),
        PartialBody(usize),
        Done(usize),
        Invalid(httparse::Error),
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
    }

    impl ResponseParse {
        pub fn new() -> Self {
            ResponseParse {
                state: ParseState::Init,
                buf: BytesMut::with_capacity(INIT_HEADER_BUF_SIZE),
                header_bytes: Bytes::new(),
            }
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
            if matches!(self.state, Done(_) | Invalid(_)) {
                panic!("Wrong phase {:?}", self.state);
            }
            self.buf.extend_from_slice(data);
        }

        fn parse_header(&mut self) -> Result<Option<ResponseHeader>> {
            let mut headers = [httparse::EMPTY_HEADER; MAX_HEADERS];
            let mut resp = httparse::Response::new(&mut headers);
            let mut parser = httparse::ParserConfig::default();
            parser.allow_spaces_after_header_name_in_responses(true);
            parser.allow_obsolete_multiline_headers_in_responses(true);

            let res = parser.parse_response(&mut resp, &self.buf);
            let res = match res {
                Ok(res) => res,
                Err(e) => {
                    self.state = ParseState::Invalid(e);
                    return Error::e_because(
                        InvalidHTTPHeader,
                        format!("buf: {:?}", String::from_utf8_lossy(&self.buf)),
                        e,
                    );
                }
            };

            let split_to = match res {
                Status::Complete(s) => s,
                Status::Partial => {
                    self.state = ParseState::PartialHeader;
                    return Ok(None);
                }
            };
            // safe to unwrap, valid response always has code set.
            let mut response =
                ResponseHeader::build(resp.code.unwrap(), Some(resp.headers.len())).unwrap();
            for header in resp.headers {
                // TODO: consider hold a Bytes and all header values can be Bytes referencing the
                // original buffer without reallocation
                response.append_header(header.name.to_owned(), header.value.to_owned())?;
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
                Init | PartialHeader | Invalid(_) => {
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
                ParseState::Invalid(httparse::Error::Version) => {}
                _ => panic!("should have failed to parse"),
            }
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
