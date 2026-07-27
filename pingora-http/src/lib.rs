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
use std::ops::Deref;

pub use http::method::Method;
pub use http::status::StatusCode;
pub use http::version::Version;
pub use http::HeaderMap as HMap;

pub mod authority;
use authority::{raw_target_authority, RawTargetAuthority};

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
/// places. Mutable access to the underlying parts is intentionally not provided because header and
/// URI mutations must use methods on [RequestHeader] to preserve its internal state.
///
/// ```compile_fail
/// use pingora_http::RequestHeader;
///
/// let mut request = RequestHeader::build("GET", b"/", None).unwrap();
/// request.headers.remove("user-agent");
/// ```
#[derive(Debug)]
pub struct RequestHeader {
    base: ReqParts,
    header_name_map: Option<CaseMap>,
    // raw request-target bytes for wire serialization, set when the parsed Uri does not
    // round-trip the original target: non-UTF-8 paths, absolute-form, and the
    // authority-form CONNECT target
    raw_path_fallback: Vec<u8>, // can also be Box<[u8]>
    // whether the request-target was valid UTF-8. Tracked separately because
    // raw_path_fallback is also populated for valid-UTF-8 non-origin-form targets, so
    // its emptiness no longer implies UTF-8.
    raw_path_utf8: bool,
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

impl RequestHeader {
    fn new_no_case(size_hint: Option<usize>) -> Self {
        let mut base = ReqBuilder::new().body(()).unwrap().into_parts().0;
        base.headers.reserve(http_header_map_upper_bound(size_hint));
        RequestHeader {
            base,
            header_name_map: None,
            raw_path_fallback: vec![],
            raw_path_utf8: true,
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

    /// Return mutable access to the request extensions.
    pub fn extensions_mut(&mut self) -> &mut http::Extensions {
        &mut self.base.extensions
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
        // The Uri is now the sole source of the target, and it is valid UTF-8
        self.raw_path_utf8 = true;
    }

    /// Set the request target directly via raw bytes.
    ///
    /// Generally prefer [`Self::set_uri()`] to modify the header's URI if able.
    ///
    /// This API is to allow supporting non UTF-8 cases, and request-targets that are not
    /// in origin-form ([RFC 9112 section 3.2]).
    ///
    /// Origin-form and asterisk-form targets round-trip through the URI. Absolute-form and
    /// the authority-form CONNECT target do not, so they are additionally kept verbatim for
    /// [`Self::raw_path()`]. For those two forms the URI carries only the path component,
    /// which means [`http::Uri::path()`] returns a path rather than a whole URL and
    /// [`http::Uri::authority()`] is left unset.
    ///
    /// Any fragment is dropped: it is not part of the request-target and must not be sent
    /// upstream.
    ///
    /// [RFC 9112 section 3.2]: https://www.rfc-editor.org/rfc/rfc9112.html#section-3.2
    pub fn set_raw_path(&mut self, path: &[u8]) -> Result<()> {
        // Everything is computed before anything is stored: a rejected target must
        // leave the existing one intact.
        let parsed = parse_request_target(path)?;
        self.base.uri = parsed.uri;
        // Origin-form and asterisk-form replace the fallback with an empty vec, so a
        // reused header (e.g. a CONNECT mutated into a normal request) cannot
        // serialize the stale target.
        self.raw_path_fallback = parsed.raw_path_fallback;
        self.raw_path_utf8 = parsed.raw_path_utf8;
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

    /// Return the request target in its raw format, as it should appear on the wire.
    ///
    /// For origin-form and asterisk-form this is the path and query. For absolute-form and
    /// the authority-form CONNECT target it is the whole target as received, less any
    /// fragment.
    ///
    /// Non-UTF8 is supported; [`Self::raw_path_is_utf8()`] reports whether these bytes are
    /// valid UTF-8 or were replaced lossily in the URI.
    pub fn raw_path(&self) -> &[u8] {
        if !self.raw_path_fallback.is_empty() {
            &self.raw_path_fallback
        } else {
            self.base
                .uri
                .path_and_query()
                .map(|path| path.as_str().as_bytes())
                .or_else(|| {
                    self.base
                        .uri
                        .authority()
                        .map(|authority| authority.as_str().as_bytes())
                })
                .unwrap_or_default()
        }
    }

    /// Whether [`Self::raw_path`] is valid UTF-8 without lossy replacement.
    pub fn raw_path_is_utf8(&self) -> bool {
        self.raw_path_utf8
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
}

impl Clone for RequestHeader {
    fn clone(&self) -> Self {
        Self {
            base: self.as_owned_parts(),
            header_name_map: self.header_name_map.clone(),
            raw_path_fallback: self.raw_path_fallback.clone(),
            raw_path_utf8: self.raw_path_utf8,
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
            raw_path_utf8: true,
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
/// places. Mutable access to the underlying parts is intentionally not provided because header
/// mutations must use methods on [ResponseHeader] to preserve its internal state.
///
/// ```compile_fail
/// use pingora_http::ResponseHeader;
///
/// let mut response = ResponseHeader::build(200, None).unwrap();
/// response.headers.remove("server");
/// ```
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

    /// Return mutable access to the response extensions.
    pub fn extensions_mut(&mut self) -> &mut http::Extensions {
        &mut self.base.extensions
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

/// Build a [Uri] carrying only a path-and-query component. `target` is the original
/// request-target, used for error context.
fn path_and_query_uri(path_and_query: &str, target: &str) -> Result<Uri> {
    Uri::builder()
        .path_and_query(path_and_query)
        .build()
        .explain_err(InvalidHTTPHeader, |_| format!("invalid uri {target}"))
}

/// A request-target parsed into the pieces a [RequestHeader] stores.
struct ParsedRequestTarget {
    /// The [Uri] to store, carrying at most a path-and-query component.
    uri: Uri,
    /// Bytes for [RequestHeader::raw_path()] to re-serialize. Empty when `uri`
    /// round-trips the original target.
    raw_path_fallback: Vec<u8>,
    /// Whether the original target was valid UTF-8, i.e. `uri` is not lossy.
    raw_path_utf8: bool,
}

/// Parse a request-target (RFC 9112 §3.2) into the pieces to store on the header.
fn parse_request_target(path: &[u8]) -> Result<ParsedRequestTarget> {
    let Ok(p) = std::str::from_utf8(path) else {
        // put a valid utf-8 path into base for read only access
        let lossy_str = String::from_utf8_lossy(path);
        let uri = Uri::builder()
            .path_and_query(lossy_str.as_ref())
            .build()
            .explain_err(InvalidHTTPHeader, |_| format!("invalid uri {lossy_str}"))?;
        return Ok(ParsedRequestTarget {
            uri,
            raw_path_fallback: path.to_vec(),
            raw_path_utf8: false,
        });
    };

    // Origin-form (§3.2.1) and asterisk-form (§3.2.4) round-trip through the Uri's
    // path_and_query(), so they need no raw fallback.
    if p.starts_with('/') || p == "*" {
        return Ok(ParsedRequestTarget {
            uri: path_and_query_uri(p, p)?,
            raw_path_fallback: vec![],
            raw_path_utf8: true,
        });
    }

    // A fragment is not part of the request-target (§3.2) and is separated from the URI
    // before dereference (RFC 3986 §3.5), so it must not reach the upstream request-line.
    // Both authority validators already terminate the authority at `#`, so dropping it
    // cannot change the authority they reconcile against `Host`.
    let target = p
        .split_once('#')
        .map_or(p, |(before_fragment, _)| before_fragment);

    // Absolute-form (§3.2.2) and the authority-form CONNECT target (§3.2.3) are kept
    // verbatim for raw_path(), while the Uri carries only the path component so that
    // callers reading uri.path() see a path rather than a whole URL.
    //
    // Scheme and authority are deliberately left off the Uri. The proxy layer
    // reconciles the target's authority against `Host` by parsing these raw bytes;
    // populating uri.authority() here would send that reconciliation down its URI
    // branch instead, rewriting the target on egress.
    //
    // The authority boundary comes from the same classifier the proxy layer reconciles
    // with, so the path extracted here cannot disagree with the authority validated
    // there. A second parser with its own idea of where the authority ends would let
    // targets like `foo:bar://host/admin` yield a path that was never validated.
    let uri = match raw_target_authority(target.as_bytes()) {
        RawTargetAuthority::Absolute { path_and_query, .. } => {
            // `path_and_query` is a suffix of `target` starting at an ASCII delimiter, so
            // it is always valid UTF-8; an empty component leaves the Uri at its default.
            match std::str::from_utf8(path_and_query).unwrap_or_default() {
                "" => Uri::default(),
                // "http://host?q=1" has no path, but origin-form requires at least "/"
                // (§3.2.1), so anchor the component to the root.
                pq if !pq.starts_with('/') => path_and_query_uri(&format!("/{pq}"), p)?,
                pq => path_and_query_uri(pq, p)?,
            }
        }
        // No absolute-form authority means there is no path component to extract: the
        // authority-form CONNECT target (§3.2.3), opaque custom schemes, and targets
        // whose authority is ambiguous all land here. Leaving the Uri at its default
        // keeps uri.path() at "/" rather than inventing a path from bytes the authority
        // classifier read differently.
        RawTargetAuthority::None | RawTargetAuthority::AmbiguousAuthority => Uri::default(),
    };
    Ok(ParsedRequestTarget {
        uri,
        raw_path_fallback: target.as_bytes().to_vec(),
        raw_path_utf8: true,
    })
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
        // Use the non-panicking `try_append`: the infallible `append` calls
        // `.expect("size overflows MAX_SIZE")` internally, which would abort the
        // process if the case map ever exceeded `http`'s `MAX_SIZE` (1 << 15).
        name_map
            .try_append(header_name.clone(), case_header_name)
            .or_err(InvalidHTTPHeader, "header name map size overflows MAX_SIZE")?;
    }

    // Non-panicking `try_append` for the same reason as the case map above.
    value_map.try_append(header_name, value).or_err(
        InvalidHTTPHeader,
        "header value map size overflows MAX_SIZE",
    )
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

/// Build a [`HeaderValue`] from owned bytes, normalizing the value per RFC
/// 9110 section 5.5 and RFC 9112 section 5.2: obs-fold continuations
/// collapse to a single SP, and any stray CR / LF / NUL is replaced with SP.
/// Zero-copy when the input contains no CR/LF/NUL.
///
/// # Precondition
///
/// Input must come from a conformant HTTP/1.1 parser: bytes must satisfy
/// the `field-content` grammar (SP, HTAB, `%x21-7E`, `obs-text` `%x80-FF`),
/// with CR/LF/NUL only appearing as part of an obs-fold or as invalid bytes
/// that this function will replace with SP. All workspace callers satisfy
/// this via httparse.
///
/// # Panics
///
/// Debug builds panic on precondition violation via the sanity check in
/// [`HeaderValue::from_maybe_shared_unchecked`]. Release builds skip the
/// check; invalid input is undefined behavior per the `http` crate.
pub fn header_value_from_raw(raw: impl Into<bytes::Bytes>) -> HeaderValue {
    let normalized = normalize_field_value(raw.into());
    // SAFETY: `normalize_field_value` replaces all CR/LF/NUL with SP; by
    // precondition the remaining bytes pass the `http` crate's `is_valid`
    // byte-set check (other controls except HTAB are not expected here). The
    // crate's documented safety contract names "valid UTF-8", but its
    // internal use of the bytes never relies on UTF-8 in release. Matches
    // long-standing precedent.
    unsafe { HeaderValue::from_maybe_shared_unchecked(normalized) }
}

/// Build a [`HeaderValue`] from a borrowed slice, normalizing obs-fold per
/// RFC 9112 section 5.2. The slice is copied into an owned buffer.
///
/// Precondition and panic behavior are the same as [`header_value_from_raw`].
pub fn header_value_from_slice(raw: &[u8]) -> HeaderValue {
    header_value_from_raw(bytes::Bytes::copy_from_slice(raw))
}

/// Normalize a header field value per RFC 9110 section 5.5 and RFC 9112
/// section 5.2:
///
/// - Each CRLF + WSP obs-fold continuation collapses to a single SP
///   ([RFC 9112 section 5.2]).
/// - Any standalone CR, LF, or NUL (not part of an obs-fold being collapsed)
///   is replaced with a single SP ([RFC 9110 section 5.5]).
/// - Other CTL characters are retained, as RFC 9110 section 5.5 permits.
///
/// Zero-copy when the input contains no CR, LF, or NUL.
/// Example: `b"obs\r\n fold\r\n\t line"` becomes `b"obs fold line"`.
///
/// [RFC 9112 section 5.2]: https://datatracker.ietf.org/doc/html/rfc9112#section-5.2
/// [RFC 9110 section 5.5]: https://datatracker.ietf.org/doc/html/rfc9110#section-5.5
fn normalize_field_value(raw: bytes::Bytes) -> bytes::Bytes {
    // Fast path: no CR/LF/NUL, nothing to do.
    if !raw.iter().any(|b| matches!(b, b'\r' | b'\n' | b'\0')) {
        return raw;
    }

    // No LF means no obs-fold (which requires CRLF). Replace each stray CR
    // or NUL with SP per RFC 9110 section 5.5.
    let Some(first_nl) = raw.iter().position(|b| *b == b'\n') else {
        let replaced: Vec<u8> = raw
            .iter()
            .map(|&b| if matches!(b, b'\r' | b'\0') { b' ' } else { b })
            .collect();
        return bytes::Bytes::from(replaced);
    };

    // Mid-segment CRs (and NULs) — which boundary trimming can't reach because
    // they're not at a segment boundary — are replaced with SP at copy time
    // per RFC 9110 section 5.5. Empty continuations contribute no SP, so
    // leading/trailing newlines don't introduce spurious whitespace.
    fn push_with_replacement(dst: &mut Vec<u8>, src: &[u8]) {
        dst.extend(
            src.iter()
                .map(|&b| if matches!(b, b'\r' | b'\0') { b' ' } else { b }),
        );
    }

    // Trim ASCII whitespace at segment boundaries (we split on `\n`) so the
    // trailing `\r` of each CRLF is absorbed into the obs-fold collapse along
    // with the fold's SP/HTAB run.
    let head = raw[..first_nl].trim_ascii_end();
    let mut unfolded = Vec::with_capacity(raw.len());
    push_with_replacement(&mut unfolded, head);
    for line in raw[first_nl + 1..].split(|b| *b == b'\n') {
        let line = line.trim_ascii();
        if line.is_empty() {
            continue;
        }
        if !unfolded.is_empty() {
            unfolded.push(b' ');
        }
        push_with_replacement(&mut unfolded, line);
    }
    bytes::Bytes::from(unfolded)
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
        let mut req = RequestHeader::build("GET", b"/", None).unwrap();
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
        assert!(req.case_header_iter().next().is_none());

        let mut resp = ResponseHeader::new_no_case(None);
        resp.insert_header("foo", "bar").unwrap();
        resp.insert_header("FoO", "Bar").unwrap();
        let mut buf: Vec<u8> = vec![];
        resp.header_to_h1_wire(&mut buf);
        assert_eq!(buf, b"foo: Bar\r\n");
        assert!(resp.case_header_iter().next().is_none());
    }

    #[test]
    fn test_multiple_header() {
        let mut req = RequestHeader::build("GET", b"/", None).unwrap();
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
        assert!(!req.raw_path_is_utf8());
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
        assert!(req.raw_path_is_utf8());
    }

    #[test]
    fn test_absolute_form_http() {
        // The Uri exposes the path component, while raw_path() keeps the target
        // verbatim for the wire.
        let req = RequestHeader::build("GET", b"http://host/path?query=1", None).unwrap();
        assert_eq!("/path?query=1", req.uri.path_and_query().unwrap().as_str());
        assert_eq!("/path", req.uri.path());
        assert_eq!(b"http://host/path?query=1", req.raw_path());
    }

    #[test]
    fn test_absolute_form_https() {
        let req = RequestHeader::build("GET", b"https://example.com/a/b/c?d=e", None).unwrap();
        assert_eq!("/a/b/c?d=e", req.uri.path_and_query().unwrap().as_str());
        assert_eq!("/a/b/c", req.uri.path());
    }

    #[test]
    fn test_absolute_form_no_path() {
        // No path component, so the Uri is left at its default of "/".
        let req = RequestHeader::build("GET", b"http://host", None).unwrap();
        assert_eq!("/", req.uri.path());
        assert_eq!(Some("/"), req.uri.path_and_query().map(|pq| pq.as_str()));
        assert_eq!(b"http://host", req.raw_path());
    }

    #[test]
    fn test_absolute_form_root() {
        let req = RequestHeader::build("GET", b"http://host/", None).unwrap();
        assert_eq!("/", req.uri.path());
    }

    #[test]
    fn test_absolute_form_no_path_with_query() {
        // "http://host?query" has no path; origin-form requires at least "/"
        // (§3.2.1), so the query is anchored to the root on the stored Uri.
        let req = RequestHeader::build("GET", b"http://host?query=1", None).unwrap();
        assert_eq!("/", req.uri.path());
        assert_eq!(Some("query=1"), req.uri.query());
        assert_eq!("/?query=1", req.uri.path_and_query().unwrap().as_str());
        assert_eq!(b"http://host?query=1", req.raw_path());
    }

    #[test]
    fn test_absolute_form_uri_has_no_authority() {
        // Scheme and authority are deliberately kept off the Uri: the proxy layer
        // reconciles the target authority against `Host` from the raw bytes, and a
        // populated uri.authority() would divert it to rewriting the target instead.
        let req = RequestHeader::build("GET", b"http://host:8080/path?q=1", None).unwrap();
        assert_eq!(None, req.uri.scheme_str());
        assert_eq!(None, req.uri.authority());
        assert_eq!("/path", req.uri.path());
        assert_eq!(b"http://host:8080/path?q=1", req.raw_path());
    }

    #[test]
    fn test_fragment_is_not_forwarded() {
        // A fragment is not part of the request-target (§3.2) and is separated from the
        // URI before dereference (RFC 3986 §3.5), so it must not reach the upstream
        // request-line.
        for (target, raw, path) in [
            (&b"http://host/p#frag"[..], &b"http://host/p"[..], "/p"),
            (b"http://host#frag", b"http://host", "/"),
            (b"http://host?q=1#frag", b"http://host?q=1", "/"),
            // Both authority classifiers terminate at `#`, so a fragment cannot smuggle
            // userinfo or a second authority past `Host` reconciliation.
            (b"http://host#@evil.example/", b"http://host", "/"),
        ] {
            let req = RequestHeader::build("GET", target, None).unwrap();
            let target = String::from_utf8_lossy(target);
            assert_eq!(raw, req.raw_path(), "{target}");
            assert_eq!(path, req.uri.path(), "{target}");
        }

        // Origin-form fragments are already dropped by the Uri, so stripping here keeps
        // absolute-form consistent with origin-form rather than diverging from it.
        let req = RequestHeader::build("GET", b"/p#frag", None).unwrap();
        assert_eq!(b"/p", req.raw_path());

        // CONNECT reconciles `Host` against the `#`-truncated prefix, so the stored
        // bytes are exactly the ones that were validated.
        let req = RequestHeader::build("CONNECT", b"host:443#x", None).unwrap();
        assert_eq!(b"host:443", req.raw_path());
    }

    #[test]
    fn test_uri_path_cannot_disagree_with_validated_authority() {
        // The Uri path is extracted with the same classifier the proxy layer uses to
        // reconcile the authority. A separate parser anchoring on the first "://" would
        // read "/admin" out of these targets, a path no authority check ever saw,
        // because the classifier stops at the first scheme-terminating colon.
        for target in [
            &b"foo:bar://evil.example/admin"[..],
            b"myproto:x://evil.example/admin",
            b"myproto:opaque",
        ] {
            let req = RequestHeader::build("GET", target, None).unwrap();
            let label = String::from_utf8_lossy(target);
            assert_eq!(
                RawTargetAuthority::None,
                raw_target_authority(req.raw_path()),
                "{label}"
            );
            assert_eq!("/", req.uri.path(), "{label}");
            assert_eq!(target, req.raw_path(), "{label}");
        }

        // An ambiguous authority yields no path either: normalization could move the
        // authority boundary, so no extracted path would be trustworthy.
        let req = RequestHeader::build("GET", b"http:///path", None).unwrap();
        assert_eq!("/", req.uri.path());
        assert_eq!(b"http:///path", req.raw_path());
    }

    #[test]
    fn test_origin_form_raw_path_is_byte_identical() {
        // Origin-form is the hot path and must round-trip through the Uri untouched.
        for target in [
            &b"/"[..],
            b"/index.html",
            b"/a/b/c?d=e&f=g",
            b"/%2e%2e/x",
            b"/a+b/c%20d",
            b"*",
        ] {
            let req = RequestHeader::build("GET", target, None).unwrap();
            assert_eq!(
                target,
                req.raw_path(),
                "{}",
                String::from_utf8_lossy(target)
            );
            assert!(req.raw_path_is_utf8());
        }
    }

    #[test]
    fn test_non_origin_form_survives_clone_and_parts_round_trip() {
        let req = RequestHeader::build("GET", b"http://host:8080/p?q=1", None).unwrap();

        let cloned = req.clone();
        assert_eq!(req.raw_path(), cloned.raw_path());
        assert_eq!(req.uri.path(), cloned.uri.path());
        assert_eq!(req.raw_path_is_utf8(), cloned.raw_path_is_utf8());

        // ReqParts carries no raw target, so the round-trip falls back to the Uri and
        // yields the origin-form path rather than the original absolute-form target.
        let from_parts = RequestHeader::from(req.as_owned_parts());
        assert_eq!(b"/p?q=1", from_parts.raw_path());
        assert!(from_parts.raw_path_is_utf8());
    }

    #[test]
    fn test_connect_authority_form() {
        // §3.2.3: authority-form is only used for CONNECT and must reach the tunnel
        // destination verbatim. It has no path component, so the Uri is left at its
        // default and the authority stays available only through raw_path().
        let req = RequestHeader::build("CONNECT", b"example.com:443", None).unwrap();
        assert_eq!(b"example.com:443", req.raw_path());
        assert_eq!(None, req.uri.authority());
        assert_eq!("/", req.uri.path());
    }

    #[test]
    fn test_connect_authority_form_default_port() {
        // RFC 9112 §3.2.3 example: CONNECT www.example.com:80.
        let req = RequestHeader::build("CONNECT", b"www.example.com:80", None).unwrap();
        assert_eq!(b"www.example.com:80", req.raw_path());
    }

    #[test]
    fn test_connect_authority_form_shapes_pass_through() {
        // Parsing deliberately does not validate the authority grammar: that belongs to
        // the authority module, which applications running custom protocols can opt out
        // of. What this level guarantees is that whichever shape arrives reaches the
        // tunnel destination byte-identically and contributes no path, covering both the
        // IP-literal and reg-name forms of RFC 3986 §3.2.2.
        for target in [
            &b"[v7.x]:443"[..],
            b"[vF.a:b~!$&'()*+,;=]:8443",
            b"[::1]:443",
            b"[2001:db8::1]:8443",
            b"127.0.0.1:443",
            b"sub.example.com:8080",
            b"host-with-dash:1",
            b"a_b:443",
        ] {
            let req = RequestHeader::build("CONNECT", target, None).unwrap();
            let label = String::from_utf8_lossy(target);
            assert_eq!(target, req.raw_path(), "{label}");
            assert_eq!("/", req.uri.path(), "{label}");
        }
    }

    #[test]
    fn test_set_raw_path_replaces_all_target_state() {
        // set_raw_path computes every field before storing any of them, so a mutation
        // leaves nothing from the previous target behind.
        //
        // The rejection half of that contract is not covered here: the pinned http
        // fork accepts every request-target, including spaces and control bytes, so
        // the error path is unreachable in this configuration. Forbidden bytes are
        // caught when the request-line is serialized instead.
        let mut req = RequestHeader::build("GET", b"/path-\xff", None).unwrap();
        assert!(!req.raw_path_is_utf8());
        assert!(!req.raw_path_fallback.is_empty());

        req.set_raw_path(b"/plain").unwrap();
        assert_eq!(b"/plain", req.raw_path());
        assert_eq!("/plain", req.uri.path());
        assert!(req.raw_path_is_utf8());
        assert!(req.raw_path_fallback.is_empty());

        req.set_raw_path(b"http://host/abs?q=1").unwrap();
        assert_eq!(b"http://host/abs?q=1", req.raw_path());
        assert_eq!("/abs", req.uri.path());
        assert!(req.raw_path_is_utf8());
    }

    #[test]
    fn test_raw_path_is_utf8_tracks_encoding_not_fallback() {
        // The fallback is populated for valid-UTF-8 non-origin-form targets too, so
        // raw_path_is_utf8() cannot be inferred from it being non-empty.
        let req = RequestHeader::build("GET", b"http://host/path", None).unwrap();
        assert!(!req.raw_path_fallback.is_empty());
        assert!(req.raw_path_is_utf8());

        let req = RequestHeader::build("CONNECT", b"example.com:443", None).unwrap();
        assert!(!req.raw_path_fallback.is_empty());
        assert!(req.raw_path_is_utf8());

        let req = RequestHeader::build("GET", b"/path-\xff", None).unwrap();
        assert!(!req.raw_path_fallback.is_empty());
        assert!(!req.raw_path_is_utf8());

        let req = RequestHeader::build("GET", b"/path", None).unwrap();
        assert!(req.raw_path_fallback.is_empty());
        assert!(req.raw_path_is_utf8());
    }

    #[test]
    fn test_set_raw_path_clears_stale_connect_fallback() {
        // Reusing a header: the CONNECT authority-form target must not survive into
        // a subsequent origin-form request via a stale raw_path_fallback.
        let mut req = RequestHeader::build("CONNECT", b"example.com:443", None).unwrap();
        assert_eq!(b"example.com:443", req.raw_path());
        req.set_method(Method::GET);
        req.set_raw_path(b"/ok").unwrap();
        assert_eq!(b"/ok", req.raw_path());
        assert_eq!("/ok", req.uri.path());
    }

    #[test]
    fn test_absolute_form_set_raw_path_mutation() {
        // The mutation path, not just construction via build().
        let mut req = RequestHeader::build("GET", b"/original", None).unwrap();
        assert_eq!("/original", req.uri.path());
        req.set_raw_path(b"http://host/mutated?q=1").unwrap();
        assert_eq!("/mutated?q=1", req.uri.path_and_query().unwrap().as_str());
        assert_eq!("/mutated", req.uri.path());
    }

    #[test]
    fn test_absolute_form_with_port() {
        let req = RequestHeader::build("GET", b"http://host:8080/path", None).unwrap();
        assert_eq!("/path", req.uri.path());
    }

    #[test]
    fn test_absolute_form_uppercase_scheme() {
        // RFC 3986 §3.1: scheme is case-insensitive.
        let req = RequestHeader::build("GET", b"HTTP://HOST/path", None).unwrap();
        assert_eq!("/path", req.uri.path());
    }

    #[test]
    fn test_absolute_form_non_http_scheme() {
        // scheme().is_some() admits any valid scheme, not just http/https.
        let req = RequestHeader::build("GET", b"ftp://host/path", None).unwrap();
        assert_eq!("/path", req.uri.path());
    }

    #[test]
    fn test_origin_form_unchanged() {
        let req = RequestHeader::build("GET", b"/path?q=1", None).unwrap();
        assert_eq!("/path?q=1", req.uri.path_and_query().unwrap().as_str());
    }

    #[test]
    fn test_origin_form_with_scheme_in_query() {
        // An origin-form path whose query contains "://" must not be mistaken
        // for absolute-form (guarded by the starts_with('/') fast path).
        let req = RequestHeader::build("GET", b"/redir?url=http://other", None).unwrap();
        assert_eq!(
            "/redir?url=http://other",
            req.uri.path_and_query().unwrap().as_str()
        );
    }

    #[test]
    fn test_asterisk_form_unchanged() {
        let req = RequestHeader::build("OPTIONS", b"*", None).unwrap();
        assert_eq!("*", req.uri.path_and_query().unwrap().as_str());
    }

    #[test]
    fn test_authority_form_raw_path() {
        let mut req = RequestHeader::new_no_case(None);
        req.set_method(Method::CONNECT);
        req.set_uri(Uri::builder().authority("pingora.org:443").build().unwrap());

        assert!(req.uri.path_and_query().is_none());
        assert_eq!(b"pingora.org:443", req.raw_path());
        assert!(req.raw_path_is_utf8());
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
    fn normalize_field_value_no_fold_is_zero_copy() {
        // The value has no newline at all -> the input `Bytes` must be
        // returned untouched (same allocation). We assert pointer/length
        // identity via `Bytes::ptr_eq` semantics: cloning a `Bytes` shares
        // the same underlying buffer, so comparing byte content + length
        // is sufficient to confirm no allocation happened.
        let input = bytes::Bytes::from_static(b"text/html; charset=utf-8");
        let out = normalize_field_value(input.clone());
        assert_eq!(out, input);
        assert_eq!(out.as_ptr(), input.as_ptr());
    }

    #[test]
    fn normalize_field_value_single_fold() {
        // CRLF + SP continuation collapses to a single SP.
        let input = bytes::Bytes::from_static(b"obs\r\n fold");
        assert_eq!(&normalize_field_value(input)[..], b"obs fold");
    }

    #[test]
    fn normalize_field_value_multiple_folds_mixed_ws() {
        // "obs\r\n fold\r\n\t line" -> "obs fold line". Each fold becomes
        // exactly one SP regardless of how many SP/HTAB chars the
        // continuation indented with.
        let input = bytes::Bytes::from_static(b"obs\r\n fold\r\n\t line");
        assert_eq!(&normalize_field_value(input)[..], b"obs fold line");
    }

    #[test]
    fn normalize_field_value_collapses_long_indent() {
        // Real-world CSP-style values often indent continuations with many
        // spaces. All of that indent collapses to a single SP.
        let input =
            bytes::Bytes::from_static(b"default-src 'self';\r\n        script-src 'self' blob:");
        assert_eq!(
            &normalize_field_value(input)[..],
            b"default-src 'self'; script-src 'self' blob:"
        );
    }

    #[test]
    fn normalize_field_value_removes_all_cr_and_lf() {
        // After normalization no CR or LF byte may survive in the value
        // (each obs-fold collapses to a single SP).
        let input = bytes::Bytes::from_static(b"a\r\n b\r\n c\r\n d");
        let out = normalize_field_value(input);
        assert!(!out.contains(&b'\r'));
        assert!(!out.contains(&b'\n'));
        assert_eq!(&out[..], b"a b c d");
    }

    #[test]
    fn normalize_field_value_bare_lf() {
        // Defensive: bare LF (no preceding CR) is also treated as a fold,
        // since the implementation splits on `\n` alone.
        let input = bytes::Bytes::from_static(b"obs\n fold");
        assert_eq!(&normalize_field_value(input)[..], b"obs fold");
    }

    #[test]
    fn normalize_field_value_empty() {
        let input = bytes::Bytes::new();
        let out = normalize_field_value(input.clone());
        assert_eq!(out, input);
    }

    #[test]
    fn header_value_from_raw_round_trip() {
        // End-to-end: a folded value parses into a HeaderValue whose bytes
        // contain no CR/LF and equal the normalized form.
        let hv = header_value_from_raw(bytes::Bytes::from_static(
            b"default-src 'self';\r\n script-src 'self'",
        ));
        assert_eq!(hv.as_bytes(), b"default-src 'self'; script-src 'self'");
    }

    #[test]
    fn header_value_from_raw_passthrough() {
        // No fold -> bytes preserved exactly.
        let hv = header_value_from_raw(bytes::Bytes::from_static(b"application/json"));
        assert_eq!(hv.as_bytes(), b"application/json");
    }

    // The remaining tests pin down the contract on edge-case inputs.
    // `header_value_from_raw` is `pub`, so any caller (not just our own
    // httparse-driven paths) can pass arbitrary bytes; these cases
    // document what they will get back.

    #[test]
    fn normalize_field_value_leading_newline_no_spurious_space() {
        // A value starting with a fold collapses to just the continuation,
        // with no spurious leading SP.
        let input = bytes::Bytes::from_static(b"\r\n fold");
        assert_eq!(&normalize_field_value(input)[..], b"fold");
    }

    #[test]
    fn normalize_field_value_trailing_newline_no_spurious_space() {
        // A trailing CRLF (or CRLF + WSP that ends the value) is dropped
        // without leaving a trailing SP.
        let input = bytes::Bytes::from_static(b"foo\r\n");
        assert_eq!(&normalize_field_value(input)[..], b"foo");

        let input = bytes::Bytes::from_static(b"a\r\n b\r\n");
        assert_eq!(&normalize_field_value(input)[..], b"a b");
    }

    #[test]
    fn normalize_field_value_only_newlines() {
        // Pathological input made entirely of newlines collapses to empty.
        assert_eq!(
            &normalize_field_value(bytes::Bytes::from_static(b"\r\n"))[..],
            b""
        );
        assert_eq!(
            &normalize_field_value(bytes::Bytes::from_static(b"\n"))[..],
            b""
        );
        assert_eq!(
            &normalize_field_value(bytes::Bytes::from_static(b"\r\n\r\n"))[..],
            b""
        );
    }

    #[test]
    fn normalize_field_value_replaces_bare_cr_with_sp() {
        // Per RFC 9110 section 5.5, stray CR / LF / NUL within a field
        // value MUST be replaced with SP (not stripped, not left in place).
        let cases: &[(&[u8], &[u8])] = &[
            (b"foo\rbar", b"foo bar"),
            (b"foo\r", b"foo "),
            (b"\rfoo", b" foo"),
            (b"\r", b" "),
            (b"\r\r\r", b"   "),
        ];
        for (input, expected) in cases {
            let out = normalize_field_value(bytes::Bytes::copy_from_slice(input));
            assert_eq!(&out[..], *expected, "input = {input:?}");
            assert!(!out.contains(&b'\r'));
            assert!(!out.contains(&b'\n'));
            assert!(!out.contains(&b'\0'));
        }
    }

    #[test]
    fn normalize_field_value_replaces_cr_mid_segment_with_sp() {
        // Mid-segment CR (sits inside a segment, not at a boundary the
        // trim helpers reach) is replaced with SP per RFC 9110 section 5.5.
        let input = bytes::Bytes::from_static(b"foo\rbar\r\n baz");
        assert_eq!(&normalize_field_value(input)[..], b"foo bar baz");
    }

    #[test]
    fn normalize_field_value_replaces_nul_with_sp() {
        // NUL within a field value MUST be replaced with SP per RFC 9110
        // section 5.5. Covers NUL standalone, between CR and LF, and after
        // CRLF.
        let cases: &[(&[u8], &[u8])] = &[
            (b"foo\0bar", b"foo bar"),
            (b"\0\0\0", b"   "),
            (b"foo\0", b"foo "),
            // NUL between CR and LF: each is a stray byte -> three SPs.
            (b"foo\r\0\nbar", b"foo   bar"),
            // NUL after CRLF: CRLF treated as a fold boundary (one SP),
            // NUL replaced with one SP -> two SPs total.
            (b"foo\r\n\0bar", b"foo  bar"),
        ];
        for (input, expected) in cases {
            let out = normalize_field_value(bytes::Bytes::copy_from_slice(input));
            assert_eq!(&out[..], *expected, "input = {input:?}");
            assert!(!out.contains(&b'\r'));
            assert!(!out.contains(&b'\n'));
            assert!(!out.contains(&b'\0'));
        }
    }

    #[test]
    fn header_value_from_raw_handles_invalid_bytes() {
        // Bare CR and NUL aren't valid `field-content`, but
        // `normalize_field_value` replaces them with SP before the
        // unchecked constructor sees them, so no invalid byte ever reaches
        // `HeaderValue`.
        let hv = header_value_from_raw(bytes::Bytes::from_static(b"foo\rbar\0baz"));
        assert_eq!(hv.as_bytes(), b"foo bar baz");
    }
}
