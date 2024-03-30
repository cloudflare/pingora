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

//! Functions and utilities to help parse Cache-Control headers

use super::*;

use http::header::HeaderName;
use http::HeaderValue;
use indexmap::IndexMap;
use once_cell::sync::Lazy;
use pingora_error::{Error, ErrorType};
use regex::bytes::Regex;
use std::num::IntErrorKind;
use std::slice;
use std::str;

/// The max delta-second per [RFC 9111](https://datatracker.ietf.org/doc/html/rfc9111#section-1.2.2)
// "If a cache receives a delta-seconds
// value greater than the greatest integer it can represent, or if any
// of its subsequent calculations overflows, the cache MUST consider the
// value to be either 2147483648 (2^31) or the greatest positive integer
// it can conveniently represent."
pub const DELTA_SECONDS_OVERFLOW_VALUE: u32 = 2147483648;

/// Cache control directive key type
pub type DirectiveKey = String;

/// Cache control directive value type
#[derive(Debug)]
pub struct DirectiveValue(pub Vec<u8>);

impl AsRef<[u8]> for DirectiveValue {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

impl DirectiveValue {
    /// A [DirectiveValue] without quotes (`"`).
    pub fn parse_as_bytes(&self) -> &[u8] {
        self.0
            .strip_prefix(&[b'"'])
            .and_then(|bytes| bytes.strip_suffix(&[b'"']))
            .unwrap_or(&self.0[..])
    }

    /// A [DirectiveValue] without quotes (`"`) as `str`.
    pub fn parse_as_str(&self) -> Result<&str> {
        str::from_utf8(self.parse_as_bytes()).or_else(|e| {
            Error::e_because(ErrorType::InternalError, "could not parse value as utf8", e)
        })
    }

    /// Parse the [DirectiveValue] as delta seconds
    ///
    /// `"`s are ignored. The value is capped to [DELTA_SECONDS_OVERFLOW_VALUE].
    pub fn parse_as_delta_seconds(&self) -> Result<u32> {
        match self.parse_as_str()?.parse::<u32>() {
            Ok(value) => Ok(value),
            Err(e) => {
                // delta-seconds expect to handle positive overflow gracefully
                if e.kind() == &IntErrorKind::PosOverflow {
                    Ok(DELTA_SECONDS_OVERFLOW_VALUE)
                } else {
                    Error::e_because(ErrorType::InternalError, "could not parse value as u32", e)
                }
            }
        }
    }
}

/// An ordered map to store cache control key value pairs.
pub type DirectiveMap = IndexMap<DirectiveKey, Option<DirectiveValue>>;

/// Parsed Cache-Control directives
#[derive(Debug)]
pub struct CacheControl {
    /// The parsed directives
    pub directives: DirectiveMap,
}

/// Cacheability calculated from cache control.
#[derive(Debug, PartialEq, Eq)]
pub enum Cacheable {
    /// Cacheable
    Yes,
    /// Not cacheable
    No,
    /// No directive found for explicit cacheability
    Default,
}

/// An iter over all the cache control directives
pub struct ListValueIter<'a>(slice::Split<'a, u8, fn(&u8) -> bool>);

impl<'a> ListValueIter<'a> {
    pub fn from(value: &'a DirectiveValue) -> Self {
        ListValueIter(value.parse_as_bytes().split(|byte| byte == &b','))
    }
}

// https://datatracker.ietf.org/doc/html/rfc9110#name-whitespace
// optional whitespace OWS = *(SP / HTAB); SP = 0x20, HTAB = 0x09
fn trim_ows(bytes: &[u8]) -> &[u8] {
    fn not_ows(b: &u8) -> bool {
        b != &b'\x20' && b != &b'\x09'
    }
    // find first non-OWS char from front (head) and from end (tail)
    let head = bytes.iter().position(not_ows).unwrap_or(0);
    let tail = bytes
        .iter()
        .rposition(not_ows)
        .map(|rpos| rpos + 1)
        .unwrap_or(head);
    &bytes[head..tail]
}

impl<'a> Iterator for ListValueIter<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<Self::Item> {
        Some(trim_ows(self.0.next()?))
    }
}

// Originally from https://github.com/hapijs/wreck which has the following comments:
// Cache-Control   = 1#cache-directive
// cache-directive = token [ "=" ( token / quoted-string ) ]
// token           = [^\x00-\x20\(\)<>@\,;\:\\"\/\[\]\?\=\{\}\x7F]+
// quoted-string   = "(?:[^"\\]|\\.)*"
//
// note the `token` implementation excludes disallowed ASCII ranges
// and disallowed delimiters: https://datatracker.ietf.org/doc/html/rfc9110#section-5.6.2
// though it does not forbid `obs-text`: %x80-FF
static RE_CACHE_DIRECTIVE: Lazy<Regex> =
    // to break our version down further:
    // `(?-u)`: unicode support disabled, which puts the regex into "ASCII compatible mode" for specifying literal bytes like \x7F: https://docs.rs/regex/1.10.4/regex/bytes/index.html#syntax
    // `(?:^|(?:\s*[,;]\s*)`: allow either , or ; as a delimiter
    // `([^\x00-\x20\(\)<>@,;:\\"/\[\]\?=\{\}\x7F]+)`: token (directive name capture group)
    // `(?:=((?:[^\x00-\x20\(\)<>@,;:\\"/\[\]\?=\{\}\x7F]+|(?:"(?:[^"\\]|\\.)*"))))`: token OR quoted-string (directive value capture-group)
    Lazy::new(|| {
        Regex::new(r#"(?-u)(?:^|(?:\s*[,;]\s*))([^\x00-\x20\(\)<>@,;:\\"/\[\]\?=\{\}\x7F]+)(?:=((?:[^\x00-\x20\(\)<>@,;:\\"/\[\]\?=\{\}\x7F]+|(?:"(?:[^"\\]|\\.)*"))))?"#).unwrap()
    });

impl CacheControl {
    // Our parsing strategy is more permissive than the RFC in a few ways:
    // - Allows semicolons as delimiters (in addition to commas). See the regex above.
    // - Allows octets outside of visible ASCII in `token`s, and in later RFCs, octets outside of
    //   the `quoted-string` range: https://datatracker.ietf.org/doc/html/rfc9110#section-5.6.2
    //   See the regex above.
    // - Doesn't require no-value for "boolean directives," such as must-revalidate
    // - Allows quoted-string format for numeric values.
    fn from_headers(headers: http::header::GetAll<HeaderValue>) -> Option<Self> {
        let mut directives = IndexMap::new();
        // should iterate in header line insertion order
        for line in headers {
            for captures in RE_CACHE_DIRECTIVE.captures_iter(line.as_bytes()) {
                // directive key
                // header values don't have to be utf-8, but we store keys as strings for case-insensitive hashing
                let key = captures.get(1).and_then(|cap| {
                    str::from_utf8(cap.as_bytes())
                        .ok()
                        .map(|token| token.to_lowercase())
                });
                if key.is_none() {
                    continue;
                }
                // directive value
                // match token or quoted-string
                let value = captures
                    .get(2)
                    .map(|cap| DirectiveValue(cap.as_bytes().to_vec()));
                directives.insert(key.unwrap(), value);
            }
        }
        Some(CacheControl { directives })
    }

    /// Parse from the given header name in `headers`
    pub fn from_headers_named(header_name: &str, headers: &http::HeaderMap) -> Option<Self> {
        if !headers.contains_key(header_name) {
            return None;
        }

        Self::from_headers(headers.get_all(header_name))
    }

    /// Parse from the given header name in the [ReqHeader]
    pub fn from_req_headers_named(header_name: &str, req_header: &ReqHeader) -> Option<Self> {
        Self::from_headers_named(header_name, &req_header.headers)
    }

    /// Parse `Cache-Control` header name from the [ReqHeader]
    pub fn from_req_headers(req_header: &ReqHeader) -> Option<Self> {
        Self::from_req_headers_named("cache-control", req_header)
    }

    /// Parse from the given header name in the [RespHeader]
    pub fn from_resp_headers_named(header_name: &str, resp_header: &RespHeader) -> Option<Self> {
        Self::from_headers_named(header_name, &resp_header.headers)
    }

    /// Parse `Cache-Control` header name from the [RespHeader]
    pub fn from_resp_headers(resp_header: &RespHeader) -> Option<Self> {
        Self::from_resp_headers_named("cache-control", resp_header)
    }

    /// Whether the given directive is in the cache control.
    pub fn has_key(&self, key: &str) -> bool {
        self.directives.contains_key(key)
    }

    /// Whether the `public` directive is in the cache control.
    pub fn public(&self) -> bool {
        self.has_key("public")
    }

    /// Whether the given directive exists, and it has no value.
    fn has_key_without_value(&self, key: &str) -> bool {
        matches!(self.directives.get(key), Some(None))
    }

    /// Whether the standalone `private` exists in the cache control
    // RFC 7234: using the #field-name versions of `private`
    // means a shared cache "MUST NOT store the specified field-name(s),
    // whereas it MAY store the remainder of the response."
    // It must be a boolean form (no value) to apply to the whole response.
    // https://datatracker.ietf.org/doc/html/rfc7234#section-5.2.2.6
    pub fn private(&self) -> bool {
        self.has_key_without_value("private")
    }

    fn get_field_names(&self, key: &str) -> Option<ListValueIter> {
        if let Some(Some(value)) = self.directives.get(key) {
            Some(ListValueIter::from(value))
        } else {
            None
        }
    }

    /// Get the values of `private=`
    pub fn private_field_names(&self) -> Option<ListValueIter> {
        self.get_field_names("private")
    }

    /// Whether the standalone `no-cache` exists in the cache control
    pub fn no_cache(&self) -> bool {
        self.has_key_without_value("no-cache")
    }

    /// Get the values of `no-cache=`
    pub fn no_cache_field_names(&self) -> Option<ListValueIter> {
        self.get_field_names("no-cache")
    }

    /// Whether `no-store` exists.
    pub fn no_store(&self) -> bool {
        self.has_key("no-store")
    }

    fn parse_delta_seconds(&self, key: &str) -> Result<Option<u32>> {
        if let Some(Some(dir_value)) = self.directives.get(key) {
            Ok(Some(dir_value.parse_as_delta_seconds()?))
        } else {
            Ok(None)
        }
    }

    /// Return the `max-age` seconds
    pub fn max_age(&self) -> Result<Option<u32>> {
        self.parse_delta_seconds("max-age")
    }

    /// Return the `s-maxage` seconds
    pub fn s_maxage(&self) -> Result<Option<u32>> {
        self.parse_delta_seconds("s-maxage")
    }

    /// Return the `stale-while-revalidate` seconds
    pub fn stale_while_revalidate(&self) -> Result<Option<u32>> {
        self.parse_delta_seconds("stale-while-revalidate")
    }

    /// Return the `stale-if-error` seconds
    pub fn stale_if_error(&self) -> Result<Option<u32>> {
        self.parse_delta_seconds("stale-if-error")
    }

    /// Whether `must-revalidate` exists.
    pub fn must_revalidate(&self) -> bool {
        self.has_key("must-revalidate")
    }

    /// Whether `proxy-revalidate` exists.
    pub fn proxy_revalidate(&self) -> bool {
        self.has_key("proxy-revalidate")
    }

    /// Whether `only-if-cached` exists.
    pub fn only_if_cached(&self) -> bool {
        self.has_key("only-if-cached")
    }
}

impl InterpretCacheControl for CacheControl {
    fn is_cacheable(&self) -> Cacheable {
        if self.no_store() || self.private() {
            return Cacheable::No;
        }
        if self.has_key("s-maxage") || self.has_key("max-age") || self.public() {
            return Cacheable::Yes;
        }
        Cacheable::Default
    }

    fn allow_caching_authorized_req(&self) -> bool {
        // RFC 7234 https://datatracker.ietf.org/doc/html/rfc7234#section-3
        // "MUST NOT" store requests with Authorization header
        // unless response contains one of these directives
        self.must_revalidate() || self.public() || self.has_key("s-maxage")
    }

    fn fresh_sec(&self) -> Option<u32> {
        if self.no_cache() {
            // always treated as stale
            return Some(0);
        }
        match self.s_maxage() {
            Ok(Some(seconds)) => Some(seconds),
            // s-maxage not present
            Ok(None) => match self.max_age() {
                Ok(Some(seconds)) => Some(seconds),
                _ => None,
            },
            _ => None,
        }
    }

    fn serve_stale_while_revalidate_sec(&self) -> Option<u32> {
        // RFC 7234: these directives forbid serving stale.
        // https://datatracker.ietf.org/doc/html/rfc7234#section-4.2.4
        if self.must_revalidate() || self.proxy_revalidate() || self.has_key("s-maxage") {
            return Some(0);
        }
        self.stale_while_revalidate().unwrap_or(None)
    }

    fn serve_stale_if_error_sec(&self) -> Option<u32> {
        if self.must_revalidate() || self.proxy_revalidate() || self.has_key("s-maxage") {
            return Some(0);
        }
        self.stale_if_error().unwrap_or(None)
    }

    // Strip header names listed in `private` or `no-cache` directives from a response.
    fn strip_private_headers(&self, resp_header: &mut ResponseHeader) {
        fn strip_listed_headers(resp: &mut ResponseHeader, field_names: ListValueIter) {
            for name in field_names {
                if let Ok(header) = HeaderName::from_bytes(name) {
                    resp.remove_header(&header);
                }
            }
        }

        if let Some(headers) = self.private_field_names() {
            strip_listed_headers(resp_header, headers);
        }
        // We interpret `no-cache` the same way as `private`,
        // though technically it has a less restrictive requirement
        // ("MUST NOT be sent in the response to a subsequent request
        // without successful revalidation with the origin server").
        // https://datatracker.ietf.org/doc/html/rfc7234#section-5.2.2.2
        if let Some(headers) = self.no_cache_field_names() {
            strip_listed_headers(resp_header, headers);
        }
    }
}

/// `InterpretCacheControl` provides a meaningful interface to the parsed `CacheControl`.
/// These functions actually interpret the parsed cache-control directives to return
/// the freshness or other cache meta values that cache-control is signaling.
///
/// By default `CacheControl` implements an RFC-7234 compliant reading that assumes it is being
/// used with a shared (proxy) cache.
pub trait InterpretCacheControl {
    /// Does cache-control specify this response is cacheable?
    ///
    /// Note that an RFC-7234 compliant cacheability check must also
    /// check if the request contained the Authorization header and
    /// `allow_caching_authorized_req`.
    fn is_cacheable(&self) -> Cacheable;

    /// Does this cache-control allow caching a response to
    /// a request with the Authorization header?
    fn allow_caching_authorized_req(&self) -> bool;

    /// Returns freshness ttl specified in cache-control
    ///
    /// - `Some(_)` indicates cache-control specifies a valid ttl. Some(0) = always stale.
    /// - `None` means cache-control did not specify a valid ttl.
    fn fresh_sec(&self) -> Option<u32>;

    /// Returns stale-while-revalidate ttl,
    ///
    /// The result should consider all the relevant cache directives, not just SWR header itself.
    ///
    /// Some(0) means serving such stale is disallowed by directive like `must-revalidate`
    /// or `stale-while-revalidater=0`.
    ///
    /// `None` indicates no SWR ttl was specified.
    fn serve_stale_while_revalidate_sec(&self) -> Option<u32>;

    /// Returns stale-if-error ttl,
    ///
    /// The result should consider all the relevant cache directives, not just SIE header itself.
    ///
    /// Some(0) means serving such stale is disallowed by directive like `must-revalidate`
    /// or `stale-if-error=0`.
    ///
    /// `None` indicates no SIE ttl was specified.
    fn serve_stale_if_error_sec(&self) -> Option<u32>;

    /// Strip header names listed in `private` or `no-cache` directives from a response,
    /// usually prior to storing that response in cache.
    fn strip_private_headers(&self, resp_header: &mut ResponseHeader);
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::header::CACHE_CONTROL;
    use http::{request, response};

    fn build_response(cc_key: HeaderName, cc_value: &str) -> response::Parts {
        let (parts, _) = response::Builder::new()
            .header(cc_key, cc_value)
            .body(())
            .unwrap()
            .into_parts();
        parts
    }

    #[test]
    fn test_simple_cache_control() {
        let resp = build_response(CACHE_CONTROL, "public, max-age=10000");
        let cc = CacheControl::from_resp_headers(&resp).unwrap();
        assert!(cc.public());
        assert_eq!(cc.max_age().unwrap().unwrap(), 10000);
    }

    #[test]
    fn test_private_cache_control() {
        let resp = build_response(CACHE_CONTROL, "private");
        let cc = CacheControl::from_resp_headers(&resp).unwrap();

        assert!(cc.private());
        assert!(cc.max_age().unwrap().is_none());
    }

    #[test]
    fn test_directives_across_header_lines() {
        let (parts, _) = response::Builder::new()
            .header(CACHE_CONTROL, "public,")
            .header("cache-Control", "max-age=10000")
            .body(())
            .unwrap()
            .into_parts();
        let cc = CacheControl::from_resp_headers(&parts).unwrap();

        assert!(cc.public());
        assert_eq!(cc.max_age().unwrap().unwrap(), 10000);
    }

    #[test]
    fn test_recognizes_semicolons_as_delimiters() {
        let resp = build_response(CACHE_CONTROL, "public; max-age=0");
        let cc = CacheControl::from_resp_headers(&resp).unwrap();

        assert!(cc.public());
        assert_eq!(cc.max_age().unwrap().unwrap(), 0);
    }

    #[test]
    fn test_unknown_directives() {
        let resp = build_response(CACHE_CONTROL, "public,random1=random2, rand3=\"\"");
        let cc = CacheControl::from_resp_headers(&resp).unwrap();
        let mut directive_iter = cc.directives.iter();

        let first = directive_iter.next().unwrap();
        assert_eq!(first.0, &"public");
        assert!(first.1.is_none());

        let second = directive_iter.next().unwrap();
        assert_eq!(second.0, &"random1");
        assert_eq!(second.1.as_ref().unwrap().0, "random2".as_bytes());

        let third = directive_iter.next().unwrap();
        assert_eq!(third.0, &"rand3");
        assert_eq!(third.1.as_ref().unwrap().0, "\"\"".as_bytes());

        assert!(directive_iter.next().is_none());
    }

    #[test]
    fn test_case_insensitive_directive_keys() {
        let resp = build_response(
            CACHE_CONTROL,
            "Public=\"something\", mAx-AGe=\"10000\", foo=cRaZyCaSe, bAr=\"inQuotes\"",
        );
        let cc = CacheControl::from_resp_headers(&resp).unwrap();

        assert!(cc.public());
        assert_eq!(cc.max_age().unwrap().unwrap(), 10000);

        let mut directive_iter = cc.directives.iter();
        let first = directive_iter.next().unwrap();
        assert_eq!(first.0, &"public");
        assert_eq!(first.1.as_ref().unwrap().0, "\"something\"".as_bytes());

        let second = directive_iter.next().unwrap();
        assert_eq!(second.0, &"max-age");
        assert_eq!(second.1.as_ref().unwrap().0, "\"10000\"".as_bytes());

        // values are still stored with casing
        let third = directive_iter.next().unwrap();
        assert_eq!(third.0, &"foo");
        assert_eq!(third.1.as_ref().unwrap().0, "cRaZyCaSe".as_bytes());

        let fourth = directive_iter.next().unwrap();
        assert_eq!(fourth.0, &"bar");
        assert_eq!(fourth.1.as_ref().unwrap().0, "\"inQuotes\"".as_bytes());

        assert!(directive_iter.next().is_none());
    }

    #[test]
    fn test_non_ascii() {
        let resp = build_response(CACHE_CONTROL, "püblic=💖, max-age=\"💯\"");
        let cc = CacheControl::from_resp_headers(&resp).unwrap();

        // Not considered valid registered directive keys / values
        assert!(!cc.public());
        assert_eq!(
            cc.max_age().unwrap_err().context.unwrap().to_string(),
            "could not parse value as u32"
        );

        let mut directive_iter = cc.directives.iter();
        let first = directive_iter.next().unwrap();
        assert_eq!(first.0, &"püblic");
        assert_eq!(first.1.as_ref().unwrap().0, "💖".as_bytes());

        let second = directive_iter.next().unwrap();
        assert_eq!(second.0, &"max-age");
        assert_eq!(second.1.as_ref().unwrap().0, "\"💯\"".as_bytes());

        assert!(directive_iter.next().is_none());
    }

    #[test]
    fn test_non_utf8_key() {
        let mut resp = response::Builder::new().body(()).unwrap();
        resp.headers_mut().insert(
            CACHE_CONTROL,
            HeaderValue::from_bytes(b"bar\xFF=\"baz\", a=b").unwrap(),
        );
        let (parts, _) = resp.into_parts();
        let cc = CacheControl::from_resp_headers(&parts).unwrap();

        // invalid bytes for key
        let mut directive_iter = cc.directives.iter();
        let first = directive_iter.next().unwrap();
        assert_eq!(first.0, &"a");
        assert_eq!(first.1.as_ref().unwrap().0, "b".as_bytes());

        assert!(directive_iter.next().is_none());
    }

    #[test]
    fn test_non_utf8_value() {
        // RFC 7230: 0xFF is part of obs-text and is officially considered a valid octet in quoted-strings
        let mut resp = response::Builder::new().body(()).unwrap();
        resp.headers_mut().insert(
            CACHE_CONTROL,
            HeaderValue::from_bytes(b"max-age=ba\xFFr, bar=\"baz\xFF\", a=b").unwrap(),
        );
        let (parts, _) = resp.into_parts();
        let cc = CacheControl::from_resp_headers(&parts).unwrap();

        assert_eq!(
            cc.max_age().unwrap_err().context.unwrap().to_string(),
            "could not parse value as utf8"
        );

        let mut directive_iter = cc.directives.iter();

        let first = directive_iter.next().unwrap();
        assert_eq!(first.0, &"max-age");
        assert_eq!(first.1.as_ref().unwrap().0, b"ba\xFFr");

        let second = directive_iter.next().unwrap();
        assert_eq!(second.0, &"bar");
        assert_eq!(second.1.as_ref().unwrap().0, b"\"baz\xFF\"");

        let third = directive_iter.next().unwrap();
        assert_eq!(third.0, &"a");
        assert_eq!(third.1.as_ref().unwrap().0, "b".as_bytes());

        assert!(directive_iter.next().is_none());
    }

    #[test]
    fn test_age_overflow() {
        let resp = build_response(
            CACHE_CONTROL,
            "max-age=-99999999999999999999999999, s-maxage=99999999999999999999999999",
        );
        let cc = CacheControl::from_resp_headers(&resp).unwrap();

        assert_eq!(
            cc.s_maxage().unwrap().unwrap(),
            DELTA_SECONDS_OVERFLOW_VALUE
        );
        // negative ages still result in errors even with overflow handling
        assert_eq!(
            cc.max_age().unwrap_err().context.unwrap().to_string(),
            "could not parse value as u32"
        );
    }

    #[test]
    fn test_fresh_sec() {
        let resp = build_response(CACHE_CONTROL, "");
        let cc = CacheControl::from_resp_headers(&resp).unwrap();
        assert!(cc.fresh_sec().is_none());

        let resp = build_response(CACHE_CONTROL, "max-age=12345");
        let cc = CacheControl::from_resp_headers(&resp).unwrap();
        assert_eq!(cc.fresh_sec().unwrap(), 12345);

        let resp = build_response(CACHE_CONTROL, "max-age=99999,s-maxage=123");
        let cc = CacheControl::from_resp_headers(&resp).unwrap();
        // prefer s-maxage over max-age
        assert_eq!(cc.fresh_sec().unwrap(), 123);
    }

    #[test]
    fn test_cacheability() {
        let resp = build_response(CACHE_CONTROL, "");
        let cc = CacheControl::from_resp_headers(&resp).unwrap();
        assert_eq!(cc.is_cacheable(), Cacheable::Default);

        // uncacheable
        let resp = build_response(CACHE_CONTROL, "private, max-age=12345");
        let cc = CacheControl::from_resp_headers(&resp).unwrap();
        assert_eq!(cc.is_cacheable(), Cacheable::No);

        let resp = build_response(CACHE_CONTROL, "no-store, max-age=12345");
        let cc = CacheControl::from_resp_headers(&resp).unwrap();
        assert_eq!(cc.is_cacheable(), Cacheable::No);

        // cacheable
        let resp = build_response(CACHE_CONTROL, "public");
        let cc = CacheControl::from_resp_headers(&resp).unwrap();
        assert_eq!(cc.is_cacheable(), Cacheable::Yes);

        let resp = build_response(CACHE_CONTROL, "max-age=0");
        let cc = CacheControl::from_resp_headers(&resp).unwrap();
        assert_eq!(cc.is_cacheable(), Cacheable::Yes);
    }

    #[test]
    fn test_no_cache() {
        let resp = build_response(CACHE_CONTROL, "no-cache, max-age=12345");
        let cc = CacheControl::from_resp_headers(&resp).unwrap();
        assert_eq!(cc.is_cacheable(), Cacheable::Yes);
        assert_eq!(cc.fresh_sec().unwrap(), 0);
    }

    #[test]
    fn test_no_cache_field_names() {
        let resp = build_response(CACHE_CONTROL, "no-cache=\"set-cookie\", max-age=12345");
        let cc = CacheControl::from_resp_headers(&resp).unwrap();
        assert!(!cc.private());
        assert_eq!(cc.is_cacheable(), Cacheable::Yes);
        assert_eq!(cc.fresh_sec().unwrap(), 12345);
        let mut field_names = cc.no_cache_field_names().unwrap();
        assert_eq!(
            str::from_utf8(field_names.next().unwrap()).unwrap(),
            "set-cookie"
        );
        assert!(field_names.next().is_none());

        let mut resp = response::Builder::new().body(()).unwrap();
        resp.headers_mut().insert(
            CACHE_CONTROL,
            HeaderValue::from_bytes(
                b"private=\"\", no-cache=\"a\xFF, set-cookie, Baz\x09 , c,d  ,, \"",
            )
            .unwrap(),
        );
        let (parts, _) = resp.into_parts();
        let cc = CacheControl::from_resp_headers(&parts).unwrap();
        let mut field_names = cc.private_field_names().unwrap();
        assert_eq!(str::from_utf8(field_names.next().unwrap()).unwrap(), "");
        assert!(field_names.next().is_none());
        let mut field_names = cc.no_cache_field_names().unwrap();
        assert!(str::from_utf8(field_names.next().unwrap()).is_err());
        assert_eq!(
            str::from_utf8(field_names.next().unwrap()).unwrap(),
            "set-cookie"
        );
        assert_eq!(str::from_utf8(field_names.next().unwrap()).unwrap(), "Baz");
        assert_eq!(str::from_utf8(field_names.next().unwrap()).unwrap(), "c");
        assert_eq!(str::from_utf8(field_names.next().unwrap()).unwrap(), "d");
        assert_eq!(str::from_utf8(field_names.next().unwrap()).unwrap(), "");
        assert_eq!(str::from_utf8(field_names.next().unwrap()).unwrap(), "");
        assert!(field_names.next().is_none());
    }

    #[test]
    fn test_strip_private_headers() {
        let mut resp = ResponseHeader::build(200, None).unwrap();
        resp.append_header(
            CACHE_CONTROL,
            "no-cache=\"x-private-header\", max-age=12345",
        )
        .unwrap();
        resp.append_header("X-Private-Header", "dropped").unwrap();

        let cc = CacheControl::from_resp_headers(&resp).unwrap();
        cc.strip_private_headers(&mut resp);
        assert!(!resp.headers.contains_key("X-Private-Header"));
    }

    #[test]
    fn test_stale_while_revalidate() {
        let resp = build_response(CACHE_CONTROL, "max-age=12345, stale-while-revalidate=5");
        let cc = CacheControl::from_resp_headers(&resp).unwrap();
        assert_eq!(cc.stale_while_revalidate().unwrap().unwrap(), 5);
        assert_eq!(cc.serve_stale_while_revalidate_sec().unwrap(), 5);
        assert!(cc.serve_stale_if_error_sec().is_none());
    }

    #[test]
    fn test_stale_if_error() {
        let resp = build_response(CACHE_CONTROL, "max-age=12345, stale-if-error=3600");
        let cc = CacheControl::from_resp_headers(&resp).unwrap();
        assert_eq!(cc.stale_if_error().unwrap().unwrap(), 3600);
        assert_eq!(cc.serve_stale_if_error_sec().unwrap(), 3600);
        assert!(cc.serve_stale_while_revalidate_sec().is_none());
    }

    #[test]
    fn test_must_revalidate() {
        let resp = build_response(
            CACHE_CONTROL,
            "max-age=12345, stale-while-revalidate=60, stale-if-error=30, must-revalidate",
        );
        let cc = CacheControl::from_resp_headers(&resp).unwrap();
        assert!(cc.must_revalidate());
        assert_eq!(cc.stale_while_revalidate().unwrap().unwrap(), 60);
        assert_eq!(cc.stale_if_error().unwrap().unwrap(), 30);
        assert_eq!(cc.serve_stale_while_revalidate_sec().unwrap(), 0);
        assert_eq!(cc.serve_stale_if_error_sec().unwrap(), 0);
    }

    #[test]
    fn test_proxy_revalidate() {
        let resp = build_response(
            CACHE_CONTROL,
            "max-age=12345, stale-while-revalidate=60, stale-if-error=30, proxy-revalidate",
        );
        let cc = CacheControl::from_resp_headers(&resp).unwrap();
        assert!(cc.proxy_revalidate());
        assert_eq!(cc.stale_while_revalidate().unwrap().unwrap(), 60);
        assert_eq!(cc.stale_if_error().unwrap().unwrap(), 30);
        assert_eq!(cc.serve_stale_while_revalidate_sec().unwrap(), 0);
        assert_eq!(cc.serve_stale_if_error_sec().unwrap(), 0);
    }

    #[test]
    fn test_s_maxage_stale() {
        let resp = build_response(
            CACHE_CONTROL,
            "s-maxage=0, stale-while-revalidate=60, stale-if-error=30",
        );
        let cc = CacheControl::from_resp_headers(&resp).unwrap();
        assert_eq!(cc.stale_while_revalidate().unwrap().unwrap(), 60);
        assert_eq!(cc.stale_if_error().unwrap().unwrap(), 30);
        assert_eq!(cc.serve_stale_while_revalidate_sec().unwrap(), 0);
        assert_eq!(cc.serve_stale_if_error_sec().unwrap(), 0);
    }

    #[test]
    fn test_authorized_request() {
        let resp = build_response(CACHE_CONTROL, "max-age=10");
        let cc = CacheControl::from_resp_headers(&resp).unwrap();
        assert!(!cc.allow_caching_authorized_req());

        let resp = build_response(CACHE_CONTROL, "s-maxage=10");
        let cc = CacheControl::from_resp_headers(&resp).unwrap();
        assert!(cc.allow_caching_authorized_req());

        let resp = build_response(CACHE_CONTROL, "public");
        let cc = CacheControl::from_resp_headers(&resp).unwrap();
        assert!(cc.allow_caching_authorized_req());

        let resp = build_response(CACHE_CONTROL, "must-revalidate, max-age=0");
        let cc = CacheControl::from_resp_headers(&resp).unwrap();
        assert!(cc.allow_caching_authorized_req());

        let resp = build_response(CACHE_CONTROL, "");
        let cc = CacheControl::from_resp_headers(&resp).unwrap();
        assert!(!cc.allow_caching_authorized_req());
    }

    fn build_request(cc_key: HeaderName, cc_value: &str) -> request::Parts {
        let (parts, _) = request::Builder::new()
            .header(cc_key, cc_value)
            .body(())
            .unwrap()
            .into_parts();
        parts
    }

    #[test]
    fn test_request_only_if_cached() {
        let req = build_request(CACHE_CONTROL, "only-if-cached=1");
        let cc = CacheControl::from_req_headers(&req).unwrap();
        assert!(cc.only_if_cached())
    }
}
