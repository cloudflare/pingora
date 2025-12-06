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

//! HTTP header objects that preserve http header cases
//!
//! Although HTTP header names are supposed to be case-insensitive for compatibility, proxies
//! ideally shouldn't alter the HTTP traffic, especially the headers they don't need to read.
//!
//! This crate provide structs and methods to preserve the headers in order to build a transparent
//! proxy.

#![allow(clippy::new_without_default)]

use bytes::BufMut;
use http::header::{AsHeaderName, HeaderName, HeaderValue};
use http::request::Builder as ReqBuilder;
use http::request::Parts as ReqParts;
use http::response::Builder as RespBuilder;
use http::response::Parts as RespParts;
use http::uri::Uri;
use pingora_error::{ErrorType::*, OrErr, Result};
use std::ops::{Deref, DerefMut};

pub use http::method::Method;
pub use http::status::StatusCode;
pub use http::version::Version;
pub use http::HeaderMap as HMap;

mod case_header_name;
use case_header_name::CaseHeaderName;
pub use case_header_name::IntoCaseHeaderName;

pub mod prelude {
    pub use crate::RequestHeader;
    pub use crate::ResponseHeader;
}

/* an ordered header map to store the original case of each header name
HMap({
    "foo": ["Foo", "foO", "FoO"]
})
The order how HeaderMap iter over its items is "arbitrary, but consistent".
Hopefully this property makes sure this map of header names always iterates in the
same order of the map of header values.
This idea is inspaired by hyper @nox
*/
type CaseMap = HMap<CaseHeaderName>;

pub enum HeaderNameVariant<'a> {
    Case(&'a CaseHeaderName),
    Titled(&'a str),
}

/// The HTTP request header type.
///
/// This type is similar to [http::request::Parts] but preserves header name case.
/// It also preserves request path even if it is not UTF-8.
///
/// [RequestHeader] implements [Deref] for [http::request::Parts] so it can be used as it in most
/// places.
#[derive(Debug)]
pub struct RequestHeader {
    base: ReqParts,
    header_name_map: Option<CaseMap>,
    // store the raw path bytes only if it is invalid utf-8
    raw_path_fallback: Vec<u8>, // can also be Box<[u8]>
    // whether we send END_STREAM with HEADERS for h2 requests
    send_end_stream: bool,
}

impl AsRef<ReqParts> for RequestHeader {
    fn as_ref(&self) -> &ReqParts {
        &self.base
    }
}

impl Deref for RequestHeader {
    type Target = ReqParts;

    fn deref(&self) -> &Self::Target {
        &self.base
    }
}

impl DerefMut for RequestHeader {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.base
    }
}

impl RequestHeader {
    fn new_no_case(size_hint: Option<usize>) -> Self {
        let mut base = ReqBuilder::new().body(()).unwrap().into_parts().0;
        base.headers.reserve(http_header_map_upper_bound(size_hint));
        RequestHeader {
            base,
            header_name_map: None,
            raw_path_fallback: vec![],
            send_end_stream: true,
        }
    }

    /// Create a new [RequestHeader] with the given method and path.
    ///
    /// The `path` can be non UTF-8.
    pub fn build(
        method: impl TryInto<Method>,
        path: &[u8],
        size_hint: Option<usize>,
    ) -> Result<Self> {
        let mut req = Self::build_no_case(method, path, size_hint)?;
        req.header_name_map = Some(CaseMap::with_capacity(http_header_map_upper_bound(
            size_hint,
        )));
        Ok(req)
    }

    /// Create a new [RequestHeader] with the given method and path without preserving header case.
    ///
    /// A [RequestHeader] created from this type is more space efficient than those from [Self::build()].
    ///
    /// Use this method if reading from or writing to HTTP/2 sessions where header case doesn't matter anyway.
    pub fn build_no_case(
        method: impl TryInto<Method>,
        path: &[u8],
        size_hint: Option<usize>,
    ) -> Result<Self> {
        let mut req = Self::new_no_case(size_hint);
        req.base.method = method
            .try_into()
            .explain_err(InvalidHTTPHeader, |_| "invalid method")?;
        req.set_raw_path(path)?;
        Ok(req)
    }

    /// Append the header name and value to `self`.
    ///
    /// If there are already some headers under the same name, a new value will be added without
    /// any others being removed.
    pub fn append_header(
        &mut self,
        name: impl IntoCaseHeaderName,
        value: impl TryInto<HeaderValue>,
    ) -> Result<bool> {
        let header_value = value
            .try_into()
            .explain_err(InvalidHTTPHeader, |_| "invalid value while append")?;
        append_header_value(
            self.header_name_map.as_mut(),
            &mut self.base.headers,
            name,
            header_value,
        )
    }

    /// Insert the header name and value to `self`.
    ///
    /// Different from [Self::append_header()], this method will replace all other existing headers
    /// under the same name (case-insensitive).
    pub fn insert_header(
        &mut self,
        name: impl IntoCaseHeaderName,
        value: impl TryInto<HeaderValue>,
    ) -> Result<()> {
        let header_value = value
            .try_into()
            .explain_err(InvalidHTTPHeader, |_| "invalid value while insert")?;
        insert_header_value(
            self.header_name_map.as_mut(),
            &mut self.base.headers,
            name,
            header_value,
        )
    }

    /// Remove all headers under the name
    pub fn remove_header<'a, N: ?Sized>(&mut self, name: &'a N) -> Option<HeaderValue>
    where
        &'a N: 'a + AsHeaderName,
    {
        remove_header(self.header_name_map.as_mut(), &mut self.base.headers, name)
    }

    /// Write the header to the `buf` in HTTP/1.1 wire format.
    ///
    /// The header case will be preserved.
    pub fn header_to_h1_wire(&self, buf: &mut impl BufMut) {
        header_to_h1_wire(self.header_name_map.as_ref(), &self.base.headers, buf)
    }

    /// If case sensitivity is enabled, returns an iterator to iterate over case-sensitive header names and values.
    /// Otherwise returns an empty iterator.
    ///
    /// Headers of the same name are visited in insertion order.
    pub fn case_header_iter(&self) -> impl Iterator<Item = (&CaseHeaderName, &HeaderValue)> + '_ {
        case_header_iter(self.header_name_map.as_ref(), &self.base.headers)
    }

    /// Returns true if the request has case-sensitive headers.
    pub fn has_case(&self) -> bool {
        self.header_name_map.is_some()
    }

    pub fn map<F: FnMut(HeaderNameVariant, &HeaderValue) -> Result<()>>(
        &self,
        mut f: F,
    ) -> Result<()> {
        let key_map = self.header_name_map.as_ref();
        let value_map = &self.base.headers;

        if let Some(key_map) = key_map {
            let iter = key_map.iter().zip(value_map.iter());
            for ((header, case_header), (header2, val)) in iter {
                if header != header2 {
                    // in case the header iteration order changes in future versions of HMap
                    panic!("header iter mismatch {}, {}", header, header2)
                }
                f(HeaderNameVariant::Case(case_header), val)?;
            }
        } else {
            for (header, value) in value_map {
                let titled_header =
                    case_header_name::titled_header_name_str(header).unwrap_or(header.as_str());
                f(HeaderNameVariant::Titled(titled_header), value)?;
            }
        }

        Ok(())
    }

    /// Set the request method
    pub fn set_method(&mut self, method: Method) {
        self.base.method = method;
    }

    /// Set the request URI
    pub fn set_uri(&mut self, uri: http::Uri) {
        self.base.uri = uri;
        // Clear out raw_path_fallback, or it will be used when serializing
        self.raw_path_fallback = vec![];
    }

    /// Set the request URI directly via raw bytes.
    ///
    /// Generally prefer [Self::set_uri()] to modify the header's URI if able.
    ///
    /// This API is to allow supporting non UTF-8 cases.
    pub fn set_raw_path(&mut self, path: &[u8]) -> Result<()> {
        if let Ok(p) = std::str::from_utf8(path) {
            let uri = Uri::builder()
                .path_and_query(p)
                .build()
                .explain_err(InvalidHTTPHeader, |_| format!("invalid uri {}", p))?;
            self.base.uri = uri;
            // keep raw_path empty, no need to store twice
        } else {
            // put a valid utf-8 path into base for read only access
            let lossy_str = String::from_utf8_lossy(path);
            let uri = Uri::builder()
                .path_and_query(lossy_str.as_ref())
                .build()
                .explain_err(InvalidHTTPHeader, |_| format!("invalid uri {}", lossy_str))?;
            self.base.uri = uri;
            self.raw_path_fallback = path.to_vec();
        }
        Ok(())
    }

    /// Set whether we send an END_STREAM on H2 request HEADERS if body is empty.
    pub fn set_send_end_stream(&mut self, send_end_stream: bool) {
        self.send_end_stream = send_end_stream;
    }

    /// Returns if we support sending an END_STREAM on H2 request HEADERS if body is empty,
    /// returns None if not H2.
    pub fn send_end_stream(&self) -> Option<bool> {
        if self.base.version != Version::HTTP_2 {
            return None;
        }
        Some(self.send_end_stream)
    }

    /// Return the request path in its raw format
    ///
    /// Non-UTF8 is supported.
    pub fn raw_path(&self) -> &[u8] {
        if !self.raw_path_fallback.is_empty() {
            &self.raw_path_fallback
        } else {
            // Url should always be set
            self.base
                .uri
                .path_and_query()
                .as_ref()
                .unwrap()
                .as_str()
                .as_bytes()
        }
    }

    /// Return the file extension of the path
    pub fn uri_file_extension(&self) -> Option<&str> {
        // get everything after the last '.' in path
        let (_, ext) = self
            .uri
            .path_and_query()
            .and_then(|pq| pq.path().rsplit_once('.'))?;
        Some(ext)
    }

    /// Set http version
    pub fn set_version(&mut self, version: Version) {
        self.base.version = version;
    }

    /// Clone `self` into [http::request::Parts].
    pub fn as_owned_parts(&self) -> ReqParts {
        clone_req_parts(&self.base)
    }

    /// Removes all hop-by-hop headers as defined in RFC 7230 Section 6.1
    /// and RFC 7540 Section 8.1.2
    ///
    /// Hop-by-hop headers are only meaningful for a single connection and
    /// should not be forwarded by proxies. This method removes:
    /// - Connection
    /// - Keep-Alive
    /// - Proxy-Authenticate
    /// - Proxy-Authorization
    /// - TE
    /// - Trailer
    /// - Transfer-Encoding
    /// - Upgrade
    ///
    /// Additionally, RFC 7230 allows the Connection header to declare
    /// other headers as hop-by-hop (e.g., "Connection: close, Custom-Header").
    /// This method also removes any headers declared in the Connection header.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let mut req = RequestHeader::build("GET", "/", None)?;
    /// req.insert_header("Connection", "close")?;
    /// req.insert_header("Host", "example.com")?;
    ///
    /// req.remove_hop_by_hop_headers();
    ///
    /// assert!(req.get_header("Connection").is_none());
    /// assert_eq!(req.get_header("Host").unwrap(), "example.com");
    /// ```
    pub fn remove_hop_by_hop_headers(&mut self) {
        // RFC 7230 also allows the Connection header to declare
        // additional headers as hop-by-hop. We need to handle this first, before we remove Connection itself.
        // e.g., "Connection: X-Custom-Header, close" means X-Custom-Header is also hop-by-hop
        let mut tokens_to_remove = Vec::new();
        if let Some(conn_value) = self.headers.get("connection") {
            if let Ok(conn_str) = std::str::from_utf8(conn_value.as_bytes()) {
                for token in conn_str.split(',').map(|s| s.trim()).filter(|s| !s.is_empty()) {
                    // Skip known Connection values, only collect other headers to remove
                    if !token.eq_ignore_ascii_case("close")
                        && !token.eq_ignore_ascii_case("keep-alive")
                        && !token.eq_ignore_ascii_case("upgrade")
                    {
                        tokens_to_remove.push(token.to_string());
                    }
                }
            }
        }

        // Now remove the standard hop-by-hop headers
        // See: https://tools.ietf.org/html/rfc7230#section-6.1
        const HOP_BY_HOP: &[&str] = &[
            "connection",
            "keep-alive",
            "proxy-authenticate",
            "proxy-authorization",
            "te",
            "trailer",
            "transfer-encoding",
            "upgrade",
        ];

        for header_name in HOP_BY_HOP {
            self.remove_header(*header_name);
        }

        // Finally, remove the headers declared in Connection
        for token in tokens_to_remove {
            self.remove_header(&token);
        }
    }
}

impl Clone for RequestHeader {
    fn clone(&self) -> Self {
        Self {
            base: self.as_owned_parts(),
            header_name_map: self.header_name_map.clone(),
            raw_path_fallback: self.raw_path_fallback.clone(),
            send_end_stream: self.send_end_stream,
        }
    }
}

// The `RequestHeader` will be the no case variant, because `ReqParts` keeps no header case
impl From<ReqParts> for RequestHeader {
    fn from(parts: ReqParts) -> RequestHeader {
        Self {
            base: parts,
            header_name_map: None,
            // no illegal path
            raw_path_fallback: vec![],
            send_end_stream: true,
        }
    }
}

impl From<RequestHeader> for ReqParts {
    fn from(resp: RequestHeader) -> ReqParts {
        resp.base
    }
}

/// The HTTP response header type.
///
/// This type is similar to [http::response::Parts] but preserves header name case.
/// [ResponseHeader] implements [Deref] for [http::response::Parts] so it can be used as it in most
/// places.
#[derive(Debug)]
pub struct ResponseHeader {
    base: RespParts,
    // an ordered header map to store the original case of each header name
    header_name_map: Option<CaseMap>,
    // the reason phrase of the response, if unset, a default one will be used
    reason_phrase: Option<String>,
}

impl AsRef<RespParts> for ResponseHeader {
    fn as_ref(&self) -> &RespParts {
        &self.base
    }
}

impl Deref for ResponseHeader {
    type Target = RespParts;

    fn deref(&self) -> &Self::Target {
        &self.base
    }
}

impl DerefMut for ResponseHeader {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.base
    }
}

impl Clone for ResponseHeader {
    fn clone(&self) -> Self {
        Self {
            base: self.as_owned_parts(),
            header_name_map: self.header_name_map.clone(),
            reason_phrase: self.reason_phrase.clone(),
        }
    }
}

// The `ResponseHeader` will be the no case variant, because `RespParts` keeps no header case
impl From<RespParts> for ResponseHeader {
    fn from(parts: RespParts) -> ResponseHeader {
        Self {
            base: parts,
            header_name_map: None,
            reason_phrase: None,
        }
    }
}

impl From<ResponseHeader> for RespParts {
    fn from(resp: ResponseHeader) -> RespParts {
        resp.base
    }
}

impl From<Box<ResponseHeader>> for Box<RespParts> {
    fn from(resp: Box<ResponseHeader>) -> Box<RespParts> {
        Box::new(resp.base)
    }
}

impl ResponseHeader {
    fn new(size_hint: Option<usize>) -> Self {
        let mut resp_header = Self::new_no_case(size_hint);
        resp_header.header_name_map = Some(CaseMap::with_capacity(http_header_map_upper_bound(
            size_hint,
        )));
        resp_header
    }

    fn new_no_case(size_hint: Option<usize>) -> Self {
        let mut base = RespBuilder::new().body(()).unwrap().into_parts().0;
        base.headers.reserve(http_header_map_upper_bound(size_hint));
        ResponseHeader {
            base,
            header_name_map: None,
            reason_phrase: None,
        }
    }

    /// Create a new [ResponseHeader] with the given status code.
    pub fn build(code: impl TryInto<StatusCode>, size_hint: Option<usize>) -> Result<Self> {
        let mut resp = Self::new(size_hint);
        resp.base.status = code
            .try_into()
            .explain_err(InvalidHTTPHeader, |_| "invalid status")?;
        Ok(resp)
    }

    /// Create a new [ResponseHeader] with the given status code without preserving header case.
    ///
    /// A [ResponseHeader] created from this type is more space efficient than those from [Self::build()].
    ///
    /// Use this method if reading from or writing to HTTP/2 sessions where header case doesn't matter anyway.
    pub fn build_no_case(code: impl TryInto<StatusCode>, size_hint: Option<usize>) -> Result<Self> {
        let mut resp = Self::new_no_case(size_hint);
        resp.base.status = code
            .try_into()
            .explain_err(InvalidHTTPHeader, |_| "invalid status")?;
        Ok(resp)
    }

    /// Append the header name and value to `self`.
    ///
    /// If there are already some headers under the same name, a new value will be added without
    /// any others being removed.
    pub fn append_header(
        &mut self,
        name: impl IntoCaseHeaderName,
        value: impl TryInto<HeaderValue>,
    ) -> Result<bool> {
        let header_value = value
            .try_into()
            .explain_err(InvalidHTTPHeader, |_| "invalid value while append")?;
        append_header_value(
            self.header_name_map.as_mut(),
            &mut self.base.headers,
            name,
            header_value,
        )
    }

    /// Insert the header name and value to `self`.
    ///
    /// Different from [Self::append_header()], this method will replace all other existing headers
    /// under the same name (case insensitive).
    pub fn insert_header(
        &mut self,
        name: impl IntoCaseHeaderName,
        value: impl TryInto<HeaderValue>,
    ) -> Result<()> {
        let header_value = value
            .try_into()
            .explain_err(InvalidHTTPHeader, |_| "invalid value while insert")?;
        insert_header_value(
            self.header_name_map.as_mut(),
            &mut self.base.headers,
            name,
            header_value,
        )
    }

    /// Remove all headers under the name
    pub fn remove_header<'a, N: ?Sized>(&mut self, name: &'a N) -> Option<HeaderValue>
    where
        &'a N: 'a + AsHeaderName,
    {
        remove_header(self.header_name_map.as_mut(), &mut self.base.headers, name)
    }

    /// Write the header to the `buf` in HTTP/1.1 wire format.
    ///
    /// The header case will be preserved.
    pub fn header_to_h1_wire(&self, buf: &mut impl BufMut) {
        header_to_h1_wire(self.header_name_map.as_ref(), &self.base.headers, buf)
    }

    /// If case sensitivity is enabled, returns an iterator to iterate over case-sensitive header names and values.
    /// Otherwise returns an empty iterator.
    ///
    /// Headers of the same name are visited in insertion order.
    pub fn case_header_iter(&self) -> impl Iterator<Item = (&CaseHeaderName, &HeaderValue)> + '_ {
        case_header_iter(self.header_name_map.as_ref(), &self.base.headers)
    }

    /// Returns true if the response has case-sensitive headers.
    pub fn has_case(&self) -> bool {
        self.header_name_map.is_some()
    }

    pub fn map<F: FnMut(HeaderNameVariant, &HeaderValue) -> Result<()>>(
        &self,
        mut f: F,
    ) -> Result<()> {
        let key_map = self.header_name_map.as_ref();
        let value_map = &self.base.headers;

        if let Some(key_map) = key_map {
            let iter = key_map.iter().zip(value_map.iter());
            for ((header, case_header), (header2, val)) in iter {
                if header != header2 {
                    // in case the header iteration order changes in future versions of HMap
                    panic!("header iter mismatch {}, {}", header, header2)
                }
                f(HeaderNameVariant::Case(case_header), val)?;
            }
        } else {
            for (header, value) in value_map {
                let titled_header =
                    case_header_name::titled_header_name_str(header).unwrap_or(header.as_str());
                f(HeaderNameVariant::Titled(titled_header), value)?;
            }
        }

        Ok(())
    }

    /// Set the status code
    pub fn set_status(&mut self, status: impl TryInto<StatusCode>) -> Result<()> {
        self.base.status = status
            .try_into()
            .explain_err(InvalidHTTPHeader, |_| "invalid status")?;
        Ok(())
    }

    /// Set the HTTP version
    pub fn set_version(&mut self, version: Version) {
        self.base.version = version
    }

    /// Set the HTTP reason phase. If `None`, a default reason phase will be used
    pub fn set_reason_phrase(&mut self, reason_phrase: Option<&str>) -> Result<()> {
        // No need to allocate memory to store the phrase if it is the default one.
        if reason_phrase == self.base.status.canonical_reason() {
            self.reason_phrase = None;
            return Ok(());
        }

        // TODO: validate it "*( HTAB / SP / VCHAR / obs-text )"
        self.reason_phrase = reason_phrase.map(str::to_string);
        Ok(())
    }

    /// Get the HTTP reason phase. If [Self::set_reason_phrase()] is never called
    /// or set to `None`, a default reason phase will be used
    pub fn get_reason_phrase(&self) -> Option<&str> {
        self.reason_phrase
            .as_deref()
            .or_else(|| self.base.status.canonical_reason())
    }

    /// Clone `self` into [http::response::Parts].
    pub fn as_owned_parts(&self) -> RespParts {
        clone_resp_parts(&self.base)
    }

    /// Helper function to set the HTTP content length on the response header.
    pub fn set_content_length(&mut self, len: usize) -> Result<()> {
        self.insert_header(http::header::CONTENT_LENGTH, len)
    }
}

fn clone_req_parts(me: &ReqParts) -> ReqParts {
    let mut parts = ReqBuilder::new()
        .method(me.method.clone())
        .uri(me.uri.clone())
        .version(me.version)
        .body(())
        .unwrap()
        .into_parts()
        .0;
    parts.headers = me.headers.clone();
    parts.extensions = me.extensions.clone();
    parts
}

fn clone_resp_parts(me: &RespParts) -> RespParts {
    let mut parts = RespBuilder::new()
        .status(me.status)
        .version(me.version)
        .body(())
        .unwrap()
        .into_parts()
        .0;
    parts.headers = me.headers.clone();
    parts.extensions = me.extensions.clone();
    parts
}

// This function returns an upper bound on the size of the header map used inside the http crate.
// As of version 0.2, there is a limit of 1 << 15 (32,768) items inside the map. There is an
// assertion against this size inside the crate, so we want to avoid panicking by not exceeding this
// upper bound.
fn http_header_map_upper_bound(size_hint: Option<usize>) -> usize {
    // Even though the crate has 1 << 15 as the max size, calls to `with_capacity` invoke a
    // function that returns the size + size / 3.
    //
    // See https://github.com/hyperium/http/blob/34a9d6bdab027948d6dea3b36d994f9cbaf96f75/src/header/map.rs#L3220
    //
    // Therefore we set our max size to be even lower, so we guarantee ourselves we won't hit that
    // upper bound in the crate. Any way you cut it, 4,096 headers is insane.
    const PINGORA_MAX_HEADER_COUNT: usize = 4096;
    const INIT_HEADER_SIZE: usize = 8;

    // We select the size hint or the max size here, ensuring that we pick a value substantially lower
    // than 1 << 15 with room to grow the header map.
    std::cmp::min(
        size_hint.unwrap_or(INIT_HEADER_SIZE),
        PINGORA_MAX_HEADER_COUNT,
    )
}

#[inline]
fn append_header_value<T>(
    name_map: Option<&mut CaseMap>,
    value_map: &mut HMap<T>,
    name: impl IntoCaseHeaderName,
    value: T,
) -> Result<bool> {
    let case_header_name = name.into_case_header_name();
    let header_name: HeaderName = case_header_name
        .as_slice()
        .try_into()
        .or_err(InvalidHTTPHeader, "invalid header name")?;
    // store the original case in the map
    if let Some(name_map) = name_map {
        name_map.append(header_name.clone(), case_header_name);
    }

    Ok(value_map.append(header_name, value))
}

#[inline]
fn insert_header_value<T>(
    name_map: Option<&mut CaseMap>,
    value_map: &mut HMap<T>,
    name: impl IntoCaseHeaderName,
    value: T,
) -> Result<()> {
    let case_header_name = name.into_case_header_name();
    let header_name: HeaderName = case_header_name
        .as_slice()
        .try_into()
        .or_err(InvalidHTTPHeader, "invalid header name")?;
    if let Some(name_map) = name_map {
        // store the original case in the map
        name_map.insert(header_name.clone(), case_header_name);
    }
    value_map.insert(header_name, value);
    Ok(())
}

// the &N here is to avoid clone(). None Copy type like String can impl AsHeaderName
#[inline]
fn remove_header<'a, T, N: ?Sized>(
    name_map: Option<&mut CaseMap>,
    value_map: &mut HMap<T>,
    name: &'a N,
) -> Option<T>
where
    &'a N: 'a + AsHeaderName,
{
    let removed = value_map.remove(name);
    if removed.is_some() {
        if let Some(name_map) = name_map {
            name_map.remove(name);
        }
    }
    removed
}

#[inline]
fn header_to_h1_wire(key_map: Option<&CaseMap>, value_map: &HMap, buf: &mut impl BufMut) {
    const CRLF: &[u8; 2] = b"\r\n";
    const HEADER_KV_DELIMITER: &[u8; 2] = b": ";

    if let Some(key_map) = key_map {
        case_header_iter(key_map.into(), value_map).for_each(|(case_header, val)| {
            buf.put_slice(case_header.as_slice());
            buf.put_slice(HEADER_KV_DELIMITER);
            buf.put_slice(val.as_ref());
            buf.put_slice(CRLF);
        });
    } else {
        for (header, value) in value_map {
            let titled_header =
                case_header_name::titled_header_name_str(header).unwrap_or(header.as_str());
            buf.put_slice(titled_header.as_bytes());
            buf.put_slice(HEADER_KV_DELIMITER);
            buf.put_slice(value.as_ref());
            buf.put_slice(CRLF);
        }
    }
}

#[inline]
fn case_header_iter<'a>(
    name_map: Option<&'a CaseMap>,
    value_map: &'a HMap,
) -> impl Iterator<Item = (&'a CaseHeaderName, &'a HeaderValue)> + 'a {
    name_map.into_iter().flat_map(|name_map| {
        name_map
            .iter()
            .zip(value_map.iter())
            .map(|((h1, name), (h2, value))| {
                // in case the header iteration order changes in future versions of HMap
                assert_eq!(h1, h2, "header iter mismatch {}, {}", h1, h2);
                (name, value)
            })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_map_upper_bound() {
        assert_eq!(8, http_header_map_upper_bound(None));
        assert_eq!(16, http_header_map_upper_bound(Some(16)));
        assert_eq!(4096, http_header_map_upper_bound(Some(7777)));
    }

    #[test]
    fn test_single_header() {
        let mut req = RequestHeader::build("GET", b"\\", None).unwrap();
        req.insert_header("foo", "bar").unwrap();
        req.insert_header("FoO", "Bar").unwrap();
        let mut buf: Vec<u8> = vec![];
        req.header_to_h1_wire(&mut buf);
        assert_eq!(buf, b"FoO: Bar\r\n");
        req.case_header_iter().enumerate().for_each(|(i, (k, v))| {
            let name = String::from_utf8_lossy(k.as_slice()).into_owned();
            let value = String::from_utf8_lossy(v.as_ref()).into_owned();
            match i + 1 {
                1 => {
                    assert_eq!(name, "FoO");
                    assert_eq!(value, "Bar");
                }
                _ => panic!("too many headers"),
            }
        });

        let mut resp = ResponseHeader::new(None);
        resp.insert_header("foo", "bar").unwrap();
        resp.insert_header("FoO", "Bar").unwrap();
        let mut buf: Vec<u8> = vec![];
        resp.header_to_h1_wire(&mut buf);
        assert_eq!(buf, b"FoO: Bar\r\n");
        resp.case_header_iter().enumerate().for_each(|(i, (k, v))| {
            let name = String::from_utf8_lossy(k.as_slice()).into_owned();
            let value = String::from_utf8_lossy(v.as_ref()).into_owned();
            match i + 1 {
                1 => {
                    assert_eq!(name, "FoO");
                    assert_eq!(value, "Bar");
                }
                _ => panic!("too many headers"),
            }
        });
    }

    #[test]
    fn test_single_header_no_case() {
        let mut req = RequestHeader::new_no_case(None);
        req.insert_header("foo", "bar").unwrap();
        req.insert_header("FoO", "Bar").unwrap();
        let mut buf: Vec<u8> = vec![];
        req.header_to_h1_wire(&mut buf);
        assert_eq!(buf, b"foo: Bar\r\n");
        req.case_header_iter().for_each(|(_, _)| {
            unreachable!("request has no case");
        });

        let mut resp = ResponseHeader::new_no_case(None);
        resp.insert_header("foo", "bar").unwrap();
        resp.insert_header("FoO", "Bar").unwrap();
        let mut buf: Vec<u8> = vec![];
        resp.header_to_h1_wire(&mut buf);
        assert_eq!(buf, b"foo: Bar\r\n");
        resp.case_header_iter().for_each(|(_, _)| {
            unreachable!("response has no case");
        });
    }

    #[test]
    fn test_multiple_header() {
        let mut req = RequestHeader::build("GET", b"\\", None).unwrap();
        req.append_header("FoO", "Bar").unwrap();
        req.append_header("fOO", "bar").unwrap();
        req.append_header("BAZ", "baR").unwrap();
        req.append_header(http::header::CONTENT_LENGTH, "0")
            .unwrap();
        req.append_header("a", "b").unwrap();
        req.remove_header("a");
        let mut buf: Vec<u8> = vec![];
        req.header_to_h1_wire(&mut buf);
        assert_eq!(
            buf,
            b"FoO: Bar\r\nfOO: bar\r\nBAZ: baR\r\nContent-Length: 0\r\n"
        );
        req.case_header_iter().enumerate().for_each(|(i, (k, v))| {
            let name = String::from_utf8_lossy(k.as_slice()).into_owned();
            let value = String::from_utf8_lossy(v.as_ref()).into_owned();
            match i + 1 {
                1 => {
                    assert_eq!(name, "FoO");
                    assert_eq!(value, "Bar");
                }
                2 => {
                    assert_eq!(name, "fOO");
                    assert_eq!(value, "bar");
                }
                3 => {
                    assert_eq!(name, "BAZ");
                    assert_eq!(value, "baR");
                }
                4 => {
                    assert_eq!(name, "Content-Length");
                    assert_eq!(value, "0");
                }
                _ => panic!("too many headers"),
            }
        });

        let mut resp = ResponseHeader::new(None);
        resp.append_header("FoO", "Bar").unwrap();
        resp.append_header("fOO", "bar").unwrap();
        resp.append_header("BAZ", "baR").unwrap();
        resp.append_header(http::header::CONTENT_LENGTH, "0")
            .unwrap();
        resp.append_header("a", "b").unwrap();
        resp.remove_header("a");
        let mut buf: Vec<u8> = vec![];
        resp.header_to_h1_wire(&mut buf);
        assert_eq!(
            buf,
            b"FoO: Bar\r\nfOO: bar\r\nBAZ: baR\r\nContent-Length: 0\r\n"
        );
        resp.case_header_iter().enumerate().for_each(|(i, (k, v))| {
            let name = String::from_utf8_lossy(k.as_slice()).into_owned();
            let value = String::from_utf8_lossy(v.as_ref()).into_owned();
            match i + 1 {
                1 => {
                    assert_eq!(name, "FoO");
                    assert_eq!(value, "Bar");
                }
                2 => {
                    assert_eq!(name, "fOO");
                    assert_eq!(value, "bar");
                }
                3 => {
                    assert_eq!(name, "BAZ");
                    assert_eq!(value, "baR");
                }
                4 => {
                    assert_eq!(name, "Content-Length");
                    assert_eq!(value, "0");
                }
                _ => panic!("too many headers"),
            }
        });
    }

    #[cfg(feature = "patched_http1")]
    #[test]
    fn test_invalid_path() {
        let raw_path = b"Hello\xF0\x90\x80World";
        let req = RequestHeader::build("GET", &raw_path[..], None).unwrap();
        assert_eq!("Hello�World", req.uri.path_and_query().unwrap());
        assert_eq!(raw_path, req.raw_path());
    }

    #[cfg(feature = "patched_http1")]
    #[test]
    fn test_override_invalid_path() {
        let raw_path = b"Hello\xF0\x90\x80World";
        let mut req = RequestHeader::build("GET", &raw_path[..], None).unwrap();
        assert_eq!("Hello�World", req.uri.path_and_query().unwrap());
        assert_eq!(raw_path, req.raw_path());

        let new_path = "/HelloWorld";
        req.set_uri(Uri::builder().path_and_query(new_path).build().unwrap());
        assert_eq!(new_path, req.uri.path_and_query().unwrap());
        assert_eq!(new_path.as_bytes(), req.raw_path());
    }

    #[test]
    fn test_reason_phrase() {
        let mut resp = ResponseHeader::new(None);
        let reason = resp.get_reason_phrase().unwrap();
        assert_eq!(reason, "OK");

        resp.set_reason_phrase(Some("FooBar")).unwrap();
        let reason = resp.get_reason_phrase().unwrap();
        assert_eq!(reason, "FooBar");

        resp.set_reason_phrase(Some("OK")).unwrap();
        let reason = resp.get_reason_phrase().unwrap();
        assert_eq!(reason, "OK");

        resp.set_reason_phrase(None).unwrap();
        let reason = resp.get_reason_phrase().unwrap();
        assert_eq!(reason, "OK");
    }

    #[test]
    fn set_test_send_end_stream() {
        let mut req = RequestHeader::build("GET", b"/", None).unwrap();
        req.set_send_end_stream(true);

        // None for requests that are not h2
        assert!(req.send_end_stream().is_none());

        let mut req = RequestHeader::build("GET", b"/", None).unwrap();
        req.set_version(Version::HTTP_2);

        // Some(true) by default for h2
        assert!(req.send_end_stream().unwrap());

        req.set_send_end_stream(false);
        // Some(false)
        assert!(!req.send_end_stream().unwrap());
    }

    #[test]
    fn set_test_set_content_length() {
        let mut resp = ResponseHeader::new(None);
        resp.set_content_length(10).unwrap();

        assert_eq!(
            b"10",
            resp.headers
                .get(http::header::CONTENT_LENGTH)
                .map(|d| d.as_bytes())
                .unwrap()
        );
    }

    #[test]
    fn test_remove_hop_by_hop_headers() {
        let mut req = RequestHeader::build("GET", b"/", None).unwrap();

        // Add standard hop-by-hop headers
        req.insert_header("Connection", "close").unwrap();
        req.insert_header("Keep-Alive", "timeout=5").unwrap();
        req.insert_header("Transfer-Encoding", "chunked").unwrap();
        req.insert_header("Upgrade", "websocket").unwrap();
        req.insert_header("Proxy-Authorization", "Basic xyz").unwrap();
        req.insert_header("TE", "trailers").unwrap();
        req.insert_header("Trailer", "X-Trailer").unwrap();

        // Add regular headers that should stay
        req.insert_header("Host", "example.com").unwrap();
        req.insert_header("User-Agent", "test").unwrap();
        req.insert_header("Accept", "*/*").unwrap();

        req.remove_hop_by_hop_headers();

        // Verify hop-by-hop headers are removed
        assert!(req.headers.get("Connection").is_none());
        assert!(req.headers.get("Keep-Alive").is_none());
        assert!(req.headers.get("Transfer-Encoding").is_none());
        assert!(req.headers.get("Upgrade").is_none());
        assert!(req.headers.get("Proxy-Authorization").is_none());
        assert!(req.headers.get("TE").is_none());
        assert!(req.headers.get("Trailer").is_none());

        // Verify regular headers remain
        assert_eq!(
            req.headers.get("Host").map(|v| v.as_bytes()),
            Some(b"example.com" as &[u8])
        );
        assert_eq!(
            req.headers.get("User-Agent").map(|v| v.as_bytes()),
            Some(b"test" as &[u8])
        );
        assert_eq!(
            req.headers.get("Accept").map(|v| v.as_bytes()),
            Some(b"*/*" as &[u8])
        );
    }

    #[test]
    fn test_connection_declared_headers() {
        let mut req = RequestHeader::build("GET", b"/", None).unwrap();

        // Connection header can declare other headers as hop-by-hop
        // Per RFC 7230: "Connection: X-Custom-Hop, close" means X-Custom-Hop is also hop-by-hop
        req.insert_header("Connection", "X-Custom-Hop, close").unwrap();
        req.insert_header("X-Custom-Hop", "custom-value").unwrap();
        req.insert_header("X-Regular-Header", "keep-me").unwrap();
        req.insert_header("Host", "example.com").unwrap();

        req.remove_hop_by_hop_headers();

        // Both Connection and declared headers should be removed
        assert!(req.headers.get("Connection").is_none());
        assert!(
            req.headers.get("X-Custom-Hop").is_none(),
            "X-Custom-Hop should be removed as it was declared in Connection header"
        );

        // Regular headers stay
        assert_eq!(
            req.headers.get("X-Regular-Header").map(|v| v.as_bytes()),
            Some(b"keep-me" as &[u8])
        );
        assert_eq!(
            req.headers.get("Host").map(|v| v.as_bytes()),
            Some(b"example.com" as &[u8])
        );
    }

    #[test]
    fn test_remove_hop_by_hop_headers_case_insensitive() {
        let mut req = RequestHeader::build("GET", b"/", None).unwrap();

        // Test case insensitivity
        req.insert_header("CONNECTION", "CLOSE").unwrap();
        req.insert_header("keep-alive", "timeout=5").unwrap();
        req.insert_header("TRANSFER-ENCODING", "chunked").unwrap();

        req.insert_header("Host", "example.com").unwrap();

        req.remove_hop_by_hop_headers();

        // All hop-by-hop headers should be removed regardless of case
        assert!(req.headers.get("CONNECTION").is_none());
        assert!(req.headers.get("connection").is_none());
        assert!(req.headers.get("keep-alive").is_none());
        assert!(req.headers.get("KEEP-ALIVE").is_none());
        assert!(req.headers.get("TRANSFER-ENCODING").is_none());
        assert!(req.headers.get("transfer-encoding").is_none());

        // Regular headers should stay
        assert_eq!(
            req.headers.get("Host").map(|v| v.as_bytes()),
            Some(b"example.com" as &[u8])
        );
    }
}
