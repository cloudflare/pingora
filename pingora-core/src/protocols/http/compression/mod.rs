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

//! HTTP response (de)compression libraries
//!
//! Brotli and Gzip and partially supported.

use super::HttpTask;

use bytes::Bytes;
use log::{debug, warn};
use pingora_error::{ErrorType, Result};
use pingora_http::{RequestHeader, ResponseHeader};
use std::time::Duration;

use strum::EnumCount;
use strum_macros::EnumCount as EnumCountMacro;

mod brotli;
mod gzip;
mod zstd;

/// The type of error to return when (de)compression fails
pub const COMPRESSION_ERROR: ErrorType = ErrorType::new("CompressionError");

/// The trait for both compress and decompress because the interface and syntax are the same:
/// encode some bytes to other bytes
pub trait Encode {
    /// Encode the input bytes. The `end` flag signals the end of the entire input. The `end` flag
    /// helps the encoder to flush out the remaining buffered encoded data because certain compression
    /// algorithms prefer to collect large enough data to compress all together.
    fn encode(&mut self, input: &[u8], end: bool) -> Result<Bytes>;
    /// Return the Encoder's name, the total input bytes, the total output bytes and the total
    /// duration spent on encoding the data.
    fn stat(&self) -> (&'static str, usize, usize, Duration);
}

/// The response compression object. Currently support gzip compression and brotli decompression.
///
/// To use it, the caller should create a [`ResponseCompressionCtx`] per HTTP session.
/// The caller should call the corresponding filters for the request header, response header and
/// response body. If the algorithms are supported, the output response body will be encoded.
/// The response header will be adjusted accordingly as well. If the algorithm is not supported
/// or no encoding is needed, the response is untouched.
///
/// If configured and if the request's `accept-encoding` header contains the algorithm supported and the
/// incoming response doesn't have that encoding, the filter will compress the response.
/// If configured and supported, and if the incoming response's `content-encoding` isn't one of the
/// request's `accept-encoding` supported algorithm, the ctx will decompress the response.
///
/// # Currently supported algorithms and actions
/// - Brotli decompression: if the response is br compressed, this ctx can decompress it
/// - Gzip compression: if the response is uncompressed, this ctx can compress it with gzip
pub struct ResponseCompressionCtx(CtxInner);

enum CtxInner {
    HeaderPhase {
        // Store the preferred list to compare with content-encoding
        accept_encoding: Vec<Algorithm>,
        encoding_levels: [u32; Algorithm::COUNT],
        decompress_enable: [bool; Algorithm::COUNT],
        preserve_etag: [bool; Algorithm::COUNT],
    },
    BodyPhase(Option<Box<dyn Encode + Send + Sync>>),
}

impl ResponseCompressionCtx {
    /// Create a new [`ResponseCompressionCtx`] with the expected compression level. `0` will disable
    /// the compression. The compression level is applied across all algorithms.
    /// The `decompress_enable` flag will tell the ctx to decompress if needed.
    /// The `preserve_etag` flag indicates whether the ctx should avoid modifying the etag,
    /// which will otherwise be weakened if the flag is false and (de)compression is applied.
    pub fn new(compression_level: u32, decompress_enable: bool, preserve_etag: bool) -> Self {
        Self(CtxInner::HeaderPhase {
            accept_encoding: Vec::new(),
            encoding_levels: [compression_level; Algorithm::COUNT],
            decompress_enable: [decompress_enable; Algorithm::COUNT],
            preserve_etag: [preserve_etag; Algorithm::COUNT],
        })
    }

    /// Whether the encoder is enabled.
    /// The enablement will change according to the request and response filter by this ctx.
    pub fn is_enabled(&self) -> bool {
        match &self.0 {
            CtxInner::HeaderPhase {
                decompress_enable,
                encoding_levels: levels,
                ..
            } => levels.iter().any(|l| *l != 0) || decompress_enable.iter().any(|d| *d),
            CtxInner::BodyPhase(c) => c.is_some(),
        }
    }

    /// Return the stat of this ctx:
    /// algorithm name, in bytes, out bytes, time took for the compression
    pub fn get_info(&self) -> Option<(&'static str, usize, usize, Duration)> {
        match &self.0 {
            CtxInner::HeaderPhase { .. } => None,
            CtxInner::BodyPhase(c) => c.as_ref().map(|c| c.stat()),
        }
    }

    /// Adjust the compression level for all compression algorithms.
    /// # Panic
    /// This function will panic if it has already started encoding the response body.
    pub fn adjust_level(&mut self, new_level: u32) {
        match &mut self.0 {
            CtxInner::HeaderPhase {
                encoding_levels: levels,
                ..
            } => {
                *levels = [new_level; Algorithm::COUNT];
            }
            CtxInner::BodyPhase(_) => panic!("Wrong phase: BodyPhase"),
        }
    }

    /// Adjust the compression level for a specific algorithm.
    /// # Panic
    /// This function will panic if it has already started encoding the response body.
    pub fn adjust_algorithm_level(&mut self, algorithm: Algorithm, new_level: u32) {
        match &mut self.0 {
            CtxInner::HeaderPhase {
                encoding_levels: levels,
                ..
            } => {
                levels[algorithm.index()] = new_level;
            }
            CtxInner::BodyPhase(_) => panic!("Wrong phase: BodyPhase"),
        }
    }

    /// Adjust the decompression flag for all compression algorithms.
    /// # Panic
    /// This function will panic if it has already started encoding the response body.
    pub fn adjust_decompression(&mut self, enabled: bool) {
        match &mut self.0 {
            CtxInner::HeaderPhase {
                decompress_enable, ..
            } => {
                *decompress_enable = [enabled; Algorithm::COUNT];
            }
            CtxInner::BodyPhase(_) => panic!("Wrong phase: BodyPhase"),
        }
    }

    /// Adjust the decompression flag for a specific algorithm.
    /// # Panic
    /// This function will panic if it has already started encoding the response body.
    pub fn adjust_algorithm_decompression(&mut self, algorithm: Algorithm, enabled: bool) {
        match &mut self.0 {
            CtxInner::HeaderPhase {
                decompress_enable, ..
            } => {
                decompress_enable[algorithm.index()] = enabled;
            }
            CtxInner::BodyPhase(_) => panic!("Wrong phase: BodyPhase"),
        }
    }

    /// Adjust preserve etag setting.
    /// # Panic
    /// This function will panic if it has already started encoding the response body.
    pub fn adjust_preserve_etag(&mut self, enabled: bool) {
        match &mut self.0 {
            CtxInner::HeaderPhase { preserve_etag, .. } => {
                *preserve_etag = [enabled; Algorithm::COUNT];
            }
            CtxInner::BodyPhase(_) => panic!("Wrong phase: BodyPhase"),
        }
    }

    /// Adjust preserve etag setting for a specific algorithm.
    /// # Panic
    /// This function will panic if it has already started encoding the response body.
    pub fn adjust_algorithm_preserve_etag(&mut self, algorithm: Algorithm, enabled: bool) {
        match &mut self.0 {
            CtxInner::HeaderPhase { preserve_etag, .. } => {
                preserve_etag[algorithm.index()] = enabled;
            }
            CtxInner::BodyPhase(_) => panic!("Wrong phase: BodyPhase"),
        }
    }

    /// Feed the request header into this ctx.
    pub fn request_filter(&mut self, req: &RequestHeader) {
        if !self.is_enabled() {
            return;
        }
        match &mut self.0 {
            CtxInner::HeaderPhase {
                accept_encoding, ..
            } => parse_accept_encoding(
                req.headers.get(http::header::ACCEPT_ENCODING),
                accept_encoding,
            ),
            CtxInner::BodyPhase(_) => panic!("Wrong phase: BodyPhase"),
        }
    }

    /// Feed the response header into this ctx
    pub fn response_header_filter(&mut self, resp: &mut ResponseHeader, end: bool) {
        if !self.is_enabled() {
            return;
        }
        match &self.0 {
            CtxInner::HeaderPhase {
                decompress_enable,
                preserve_etag,
                accept_encoding,
                encoding_levels: levels,
            } => {
                if resp.status.is_informational() {
                    if resp.status == http::status::StatusCode::SWITCHING_PROTOCOLS {
                        // no transformation for websocket (TODO: cite RFC)
                        self.0 = CtxInner::BodyPhase(None);
                    }
                    // else, wait for the final response header for decision
                    return;
                }
                // do nothing if no body
                if end {
                    self.0 = CtxInner::BodyPhase(None);
                    return;
                }

                if depends_on_accept_encoding(
                    resp,
                    levels.iter().any(|level| *level != 0),
                    decompress_enable,
                ) {
                    // The response depends on the Accept-Encoding header, make sure to indicate it
                    // in the Vary response header.
                    // https://www.rfc-editor.org/rfc/rfc9110#name-vary
                    add_vary_header(resp, &http::header::ACCEPT_ENCODING);
                }

                let action = decide_action(resp, accept_encoding);
                debug!("compression action: {action:?}");
                let (encoder, preserve_etag) = match action {
                    Action::Noop => (None, false),
                    Action::Compress(algorithm) => {
                        let idx = algorithm.index();
                        (algorithm.compressor(levels[idx]), preserve_etag[idx])
                    }
                    Action::Decompress(algorithm) => {
                        let idx = algorithm.index();
                        (
                            algorithm.decompressor(decompress_enable[idx]),
                            preserve_etag[idx],
                        )
                    }
                };
                if encoder.is_some() {
                    adjust_response_header(resp, &action, preserve_etag);
                }
                self.0 = CtxInner::BodyPhase(encoder);
            }
            CtxInner::BodyPhase(_) => panic!("Wrong phase: BodyPhase"),
        }
    }

    /// Stream the response body chunks into this ctx. The return value will be the compressed
    /// data.
    ///
    /// Return None if compression is not enabled.
    pub fn response_body_filter(&mut self, data: Option<&Bytes>, end: bool) -> Option<Bytes> {
        match &mut self.0 {
            CtxInner::HeaderPhase { .. } => panic!("Wrong phase: HeaderPhase"),
            CtxInner::BodyPhase(compressor) => {
                let result = compressor
                    .as_mut()
                    .map(|c| {
                        // Feed even empty slice to compressor because it might yield data
                        // when `end` is true
                        let data = if let Some(b) = data { b.as_ref() } else { &[] };
                        c.encode(data, end)
                    })
                    .transpose();
                result.unwrap_or_else(|e| {
                    warn!("Failed to compress, compression disabled, {}", e);
                    // no point to transcode further data because bad data is already seen
                    self.0 = CtxInner::BodyPhase(None);
                    None
                })
            }
        }
    }

    // TODO: retire this function, replace it with the two functions above
    /// Feed the response into this ctx.
    /// This filter will mutate the response accordingly if encoding is needed.
    pub fn response_filter(&mut self, t: &mut HttpTask) {
        if !self.is_enabled() {
            return;
        }
        match t {
            HttpTask::Header(resp, end) => self.response_header_filter(resp, *end),
            HttpTask::Body(data, end) => {
                let compressed = self.response_body_filter(data.as_ref(), *end);
                if compressed.is_some() {
                    *t = HttpTask::Body(compressed, *end);
                }
            }
            HttpTask::Done => {
                // try to finish/flush compression
                let compressed = self.response_body_filter(None, true);
                if compressed.is_some() {
                    // compressor has more data to flush
                    *t = HttpTask::Body(compressed, true);
                }
            }
            _ => { /* Trailer, Failed: do nothing? */ }
        }
    }
}

#[derive(Debug, PartialEq, Eq, Clone, Copy, EnumCountMacro)]
pub enum Algorithm {
    Any, // the "*"
    Gzip,
    Brotli,
    Zstd,
    // TODO: Identity,
    // TODO: Deflate
    Other, // anything unknown
}

impl Algorithm {
    pub fn as_str(&self) -> &'static str {
        match self {
            Algorithm::Gzip => "gzip",
            Algorithm::Brotli => "br",
            Algorithm::Zstd => "zstd",
            Algorithm::Any => "*",
            Algorithm::Other => "other",
        }
    }

    pub fn compressor(&self, level: u32) -> Option<Box<dyn Encode + Send + Sync>> {
        if level == 0 {
            None
        } else {
            match self {
                Self::Gzip => Some(Box::new(gzip::Compressor::new(level))),
                Self::Brotli => Some(Box::new(brotli::Compressor::new(level))),
                Self::Zstd => Some(Box::new(zstd::Compressor::new(level))),
                _ => None, // not implemented
            }
        }
    }

    pub fn decompressor(&self, enabled: bool) -> Option<Box<dyn Encode + Send + Sync>> {
        if !enabled {
            None
        } else {
            match self {
                Self::Gzip => Some(Box::new(gzip::Decompressor::new())),
                Self::Brotli => Some(Box::new(brotli::Decompressor::new())),
                _ => None, // not implemented
            }
        }
    }

    pub fn index(&self) -> usize {
        *self as usize
    }
}

impl From<&str> for Algorithm {
    fn from(s: &str) -> Self {
        use unicase::UniCase;

        let coding = UniCase::new(s);
        if coding == UniCase::ascii("gzip") {
            Algorithm::Gzip
        } else if coding == UniCase::ascii("br") {
            Algorithm::Brotli
        } else if coding == UniCase::ascii("zstd") {
            Algorithm::Zstd
        } else if s.is_empty() {
            Algorithm::Any
        } else {
            Algorithm::Other
        }
    }
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum Action {
    Noop, // do nothing, e.g. when the input is already gzip
    Compress(Algorithm),
    Decompress(Algorithm),
}

// parse Accept-Encoding header and put it to the list
fn parse_accept_encoding(accept_encoding: Option<&http::HeaderValue>, list: &mut Vec<Algorithm>) {
    // https://www.rfc-editor.org/rfc/rfc9110#name-accept-encoding
    if let Some(ac) = accept_encoding {
        // fast path
        if ac.as_bytes() == b"gzip" {
            list.push(Algorithm::Gzip);
            return;
        }
        // properly parse AC header
        match sfv::Parser::parse_list(ac.as_bytes()) {
            Ok(parsed) => {
                for item in parsed {
                    if let sfv::ListEntry::Item(i) = item {
                        if let Some(s) = i.bare_item.as_token() {
                            // TODO: support q value
                            let algorithm = Algorithm::from(s);
                            // ignore algorithms that we don't understand ignore
                            if algorithm != Algorithm::Other {
                                list.push(Algorithm::from(s));
                            }
                        }
                    }
                }
            }
            Err(e) => {
                warn!("Failed to parse accept-encoding {ac:?}, {e}")
            }
        }
    } else {
        // "If no Accept-Encoding header, any content coding is acceptable"
        // keep the list empty
    }
}

#[test]
fn test_accept_encoding_req_header() {
    let mut header = RequestHeader::build("GET", b"/", None).unwrap();
    let mut ac_list = Vec::new();
    parse_accept_encoding(
        header.headers.get(http::header::ACCEPT_ENCODING),
        &mut ac_list,
    );
    assert!(ac_list.is_empty());

    let mut ac_list = Vec::new();
    header.insert_header("accept-encoding", "gzip").unwrap();
    parse_accept_encoding(
        header.headers.get(http::header::ACCEPT_ENCODING),
        &mut ac_list,
    );
    assert_eq!(ac_list[0], Algorithm::Gzip);

    let mut ac_list = Vec::new();
    header
        .insert_header("accept-encoding", "what, br, gzip")
        .unwrap();
    parse_accept_encoding(
        header.headers.get(http::header::ACCEPT_ENCODING),
        &mut ac_list,
    );
    assert_eq!(ac_list[0], Algorithm::Brotli);
    assert_eq!(ac_list[1], Algorithm::Gzip);
}

// test whether the response depends on Accept-Encoding header
fn depends_on_accept_encoding(
    resp: &ResponseHeader,
    compress_enabled: bool,
    decompress_enabled: &[bool],
) -> bool {
    use http::header::CONTENT_ENCODING;

    (decompress_enabled.iter().any(|enabled| *enabled)
        && resp.headers.get(CONTENT_ENCODING).is_some())
        || (compress_enabled && compressible(resp))
}

#[test]
fn test_decide_on_accept_encoding() {
    let mut resp = ResponseHeader::build(200, None).unwrap();
    resp.insert_header("content-length", "50").unwrap();
    resp.insert_header("content-type", "text/html").unwrap();
    resp.insert_header("content-encoding", "gzip").unwrap();

    // enabled
    assert!(depends_on_accept_encoding(&resp, false, &[true]));

    // decompress disabled => disabled
    assert!(!depends_on_accept_encoding(&resp, false, &[false]));

    // no content-encoding => disabled
    resp.remove_header("content-encoding");
    assert!(!depends_on_accept_encoding(&resp, false, &[true]));

    // compress enabled and compressible response => enabled
    assert!(depends_on_accept_encoding(&resp, true, &[false]));

    // compress disabled and compressible response => disabled
    assert!(!depends_on_accept_encoding(&resp, false, &[false]));

    // compress enabled and not compressible response => disabled
    resp.insert_header("content-type", "text/html+zip").unwrap();
    assert!(!depends_on_accept_encoding(&resp, true, &[false]));
}

// filter response header to see if (de)compression is needed
fn decide_action(resp: &ResponseHeader, accept_encoding: &[Algorithm]) -> Action {
    use http::header::CONTENT_ENCODING;

    let content_encoding = if let Some(ce) = resp.headers.get(CONTENT_ENCODING) {
        // https://www.rfc-editor.org/rfc/rfc9110#name-content-encoding
        if let Ok(ce_str) = std::str::from_utf8(ce.as_bytes()) {
            Some(Algorithm::from(ce_str))
        } else {
            // not utf-8, treat it as unknown encoding to leave it untouched
            Some(Algorithm::Other)
        }
    } else {
        // no Accept-encoding
        None
    };

    if let Some(ce) = content_encoding {
        if accept_encoding.contains(&ce) {
            // downstream can accept this encoding, nothing to do
            Action::Noop
        } else {
            // always decompress because uncompressed is always acceptable
            // https://www.rfc-editor.org/rfc/rfc9110#field.accept-encoding
            // "If the representation has no content coding, then it is acceptable by default
            // unless specifically excluded..." TODO: check the exclude case
            // TODO: we could also transcode it to a preferred encoding, e.g. br->gzip
            Action::Decompress(ce)
        }
    } else if accept_encoding.is_empty() // both CE and AE are empty
        || !compressible(resp) // the type is not compressible
        || accept_encoding[0] == Algorithm::Any
    {
        Action::Noop
    } else {
        // try to compress with the first AC
        // TODO: support to configure preferred encoding
        Action::Compress(accept_encoding[0])
    }
}

#[test]
fn test_decide_action() {
    use Action::*;
    use Algorithm::*;

    let header = ResponseHeader::build(200, None).unwrap();
    // no compression asked, no compression needed
    assert_eq!(decide_action(&header, &[]), Noop);

    // already gzip, no compression needed
    let mut header = ResponseHeader::build(200, None).unwrap();
    header.insert_header("content-type", "text/html").unwrap();
    header.insert_header("content-encoding", "gzip").unwrap();
    assert_eq!(decide_action(&header, &[Gzip]), Noop);

    // already gzip, no compression needed, upper case
    let mut header = ResponseHeader::build(200, None).unwrap();
    header.insert_header("content-encoding", "GzIp").unwrap();
    header.insert_header("content-type", "text/html").unwrap();
    assert_eq!(decide_action(&header, &[Gzip]), Noop);

    // no encoding, compression needed, accepted content-type, large enough
    // Will compress
    let mut header = ResponseHeader::build(200, None).unwrap();
    header.insert_header("content-length", "20").unwrap();
    header.insert_header("content-type", "text/html").unwrap();
    assert_eq!(decide_action(&header, &[Gzip]), Compress(Gzip));

    // too small
    let mut header = ResponseHeader::build(200, None).unwrap();
    header.insert_header("content-length", "19").unwrap();
    header.insert_header("content-type", "text/html").unwrap();
    assert_eq!(decide_action(&header, &[Gzip]), Noop);

    // already compressed MIME
    let mut header = ResponseHeader::build(200, None).unwrap();
    header.insert_header("content-length", "20").unwrap();
    header
        .insert_header("content-type", "text/html+zip")
        .unwrap();
    assert_eq!(decide_action(&header, &[Gzip]), Noop);

    // unsupported MIME
    let mut header = ResponseHeader::build(200, None).unwrap();
    header.insert_header("content-length", "20").unwrap();
    header.insert_header("content-type", "image/jpg").unwrap();
    assert_eq!(decide_action(&header, &[Gzip]), Noop);

    // compressed, need decompress
    let mut header = ResponseHeader::build(200, None).unwrap();
    header.insert_header("content-encoding", "gzip").unwrap();
    assert_eq!(decide_action(&header, &[]), Decompress(Gzip));

    // accept-encoding different, need decompress
    let mut header = ResponseHeader::build(200, None).unwrap();
    header.insert_header("content-encoding", "gzip").unwrap();
    assert_eq!(decide_action(&header, &[Brotli]), Decompress(Gzip));

    // less preferred but no need to decompress
    let mut header = ResponseHeader::build(200, None).unwrap();
    header.insert_header("content-encoding", "gzip").unwrap();
    assert_eq!(decide_action(&header, &[Brotli, Gzip]), Noop);
}

use once_cell::sync::Lazy;
use regex::Regex;

// Allow text, application, font, a few image/ MIME types and binary/octet-stream
// TODO: fine tune this list
static MIME_CHECK: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"^(?:text/|application/|font/|image/(?:x-icon|svg\+xml|nd\.microsoft\.icon)|binary/octet-stream)")
        .unwrap()
});

// check if the response mime type is compressible
fn compressible(resp: &ResponseHeader) -> bool {
    // arbitrary size limit, things to consider
    // 1. too short body may have little redundancy to compress
    // 2. gzip header and footer overhead
    // 3. latency is the same as long as data fits in a TCP congestion window regardless of size
    const MIN_COMPRESS_LEN: usize = 20;

    // check if response is too small to compress
    if let Some(cl) = resp.headers.get(http::header::CONTENT_LENGTH) {
        if let Some(cl_num) = std::str::from_utf8(cl.as_bytes())
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
        {
            if cl_num < MIN_COMPRESS_LEN {
                return false;
            }
        }
    }
    // no Content-Length or large enough, check content-type next
    if let Some(ct) = resp.headers.get(http::header::CONTENT_TYPE) {
        if let Ok(ct_str) = std::str::from_utf8(ct.as_bytes()) {
            if ct_str.contains("zip") {
                // heuristic: don't compress mime type that has zip in it
                false
            } else {
                // check if mime type in allow list
                MIME_CHECK.find(ct_str).is_some()
            }
        } else {
            false // invalid CT header, don't compress
        }
    } else {
        false // don't compress empty content-type
    }
}

// add Vary header with the specified value or extend an existing Vary header value
fn add_vary_header(resp: &mut ResponseHeader, value: &http::header::HeaderName) {
    use http::header::{HeaderValue, VARY};

    let already_present = resp.headers.get_all(VARY).iter().any(|existing| {
        existing
            .as_bytes()
            .split(|b| *b == b',')
            .map(|mut v| {
                // This is equivalent to slice.trim_ascii() which is unstable
                while let [first, rest @ ..] = v {
                    if first.is_ascii_whitespace() {
                        v = rest;
                    } else {
                        break;
                    }
                }
                while let [rest @ .., last] = v {
                    if last.is_ascii_whitespace() {
                        v = rest;
                    } else {
                        break;
                    }
                }
                v
            })
            .any(|v| v == b"*" || v.eq_ignore_ascii_case(value.as_ref()))
    });

    if !already_present {
        resp.append_header(&VARY, HeaderValue::from_name(value.clone()))
            .unwrap();
    }
}

#[test]
fn test_add_vary_header() {
    let mut header = ResponseHeader::build(200, None).unwrap();
    add_vary_header(&mut header, &http::header::ACCEPT_ENCODING);
    assert_eq!(
        header
            .headers
            .get_all("Vary")
            .into_iter()
            .collect::<Vec<_>>(),
        vec!["accept-encoding"]
    );

    let mut header = ResponseHeader::build(200, None).unwrap();
    header.insert_header("Vary", "Accept-Language").unwrap();
    add_vary_header(&mut header, &http::header::ACCEPT_ENCODING);
    assert_eq!(
        header
            .headers
            .get_all("Vary")
            .into_iter()
            .collect::<Vec<_>>(),
        vec!["Accept-Language", "accept-encoding"]
    );

    let mut header = ResponseHeader::build(200, None).unwrap();
    header
        .insert_header("Vary", "Accept-Language, Accept-Encoding")
        .unwrap();
    add_vary_header(&mut header, &http::header::ACCEPT_ENCODING);
    assert_eq!(
        header
            .headers
            .get_all("Vary")
            .into_iter()
            .collect::<Vec<_>>(),
        vec!["Accept-Language, Accept-Encoding"]
    );

    let mut header = ResponseHeader::build(200, None).unwrap();
    header.insert_header("Vary", "*").unwrap();
    add_vary_header(&mut header, &http::header::ACCEPT_ENCODING);
    assert_eq!(
        header
            .headers
            .get_all("Vary")
            .into_iter()
            .collect::<Vec<_>>(),
        vec!["*"]
    );
}

fn adjust_response_header(resp: &mut ResponseHeader, action: &Action, preserve_etag: bool) {
    use http::header::{
        HeaderValue, ACCEPT_RANGES, CONTENT_ENCODING, CONTENT_LENGTH, ETAG, TRANSFER_ENCODING,
    };

    fn set_stream_headers(resp: &mut ResponseHeader) {
        // because the transcoding is streamed, content length is not known ahead
        resp.remove_header(&CONTENT_LENGTH);
        // remove Accept-Ranges header because range requests will no longer work
        resp.remove_header(&ACCEPT_RANGES);

        // we stream body now TODO: chunked is for h1 only
        resp.insert_header(&TRANSFER_ENCODING, HeaderValue::from_static("chunked"))
            .unwrap();
    }

    fn weaken_or_clear_etag(resp: &mut ResponseHeader) {
        // RFC9110: https://datatracker.ietf.org/doc/html/rfc9110#section-8.8.1
        // "a validator is weak if it is shared by two or more representations
        // of a given resource at the same time, unless those representations
        // have identical representation data"
        // Follow nginx gzip filter's example when changing content encoding:
        // - if the ETag is not a valid strong ETag, clear it (i.e. does not start with `"`)
        // - else, weaken it
        if let Some(etag) = resp.headers.get(&ETAG) {
            let etag_bytes = etag.as_bytes();
            if etag_bytes.starts_with(b"W/") {
                // this is already a weak ETag, noop
            } else if etag_bytes.starts_with(b"\"") {
                // strong ETag, weaken since we are changing the byte representation
                let weakened_etag = HeaderValue::from_bytes(&[b"W/", etag_bytes].concat())
                    .expect("valid header value prefixed with \"W/\" should remain valid");
                resp.insert_header(&ETAG, weakened_etag)
                    .expect("can insert weakened etag when etag was already valid");
            } else {
                // invalid strong ETag, just clear it
                // https://datatracker.ietf.org/doc/html/rfc9110#section-8.8.3
                // says the opaque-tag section needs to be a quoted string
                resp.remove_header(&ETAG);
            }
        }
    }

    match action {
        Action::Noop => { /* do nothing */ }
        Action::Decompress(_) => {
            resp.remove_header(&CONTENT_ENCODING);
            set_stream_headers(resp);
            if !preserve_etag {
                weaken_or_clear_etag(resp);
            }
        }
        Action::Compress(a) => {
            resp.insert_header(&CONTENT_ENCODING, HeaderValue::from_static(a.as_str()))
                .unwrap();
            set_stream_headers(resp);
            if !preserve_etag {
                weaken_or_clear_etag(resp);
            }
        }
    }
}

#[test]
fn test_adjust_response_header() {
    use Action::*;
    use Algorithm::*;

    // noop
    let mut header = ResponseHeader::build(200, None).unwrap();
    header.insert_header("content-length", "20").unwrap();
    header.insert_header("content-encoding", "gzip").unwrap();
    header.insert_header("accept-ranges", "bytes").unwrap();
    header.insert_header("etag", "\"abc123\"").unwrap();
    adjust_response_header(&mut header, &Noop, false);
    assert_eq!(
        header.headers.get("content-encoding").unwrap().as_bytes(),
        b"gzip"
    );
    assert_eq!(
        header.headers.get("content-length").unwrap().as_bytes(),
        b"20"
    );
    assert_eq!(
        header.headers.get("etag").unwrap().as_bytes(),
        b"\"abc123\""
    );
    assert!(header.headers.get("transfer-encoding").is_none());

    // decompress gzip
    let mut header = ResponseHeader::build(200, None).unwrap();
    header.insert_header("content-length", "20").unwrap();
    header.insert_header("content-encoding", "gzip").unwrap();
    header.insert_header("accept-ranges", "bytes").unwrap();
    header.insert_header("etag", "\"abc123\"").unwrap();
    adjust_response_header(&mut header, &Decompress(Gzip), false);
    assert!(header.headers.get("content-encoding").is_none());
    assert!(header.headers.get("content-length").is_none());
    assert_eq!(
        header.headers.get("transfer-encoding").unwrap().as_bytes(),
        b"chunked"
    );
    assert!(header.headers.get("accept-ranges").is_none());
    assert_eq!(
        header.headers.get("etag").unwrap().as_bytes(),
        b"W/\"abc123\""
    );
    // when preserve_etag on, strong etag is kept
    header.insert_header("etag", "\"abc123\"").unwrap();
    adjust_response_header(&mut header, &Decompress(Gzip), true);
    assert_eq!(
        header.headers.get("etag").unwrap().as_bytes(),
        b"\"abc123\""
    );

    // compress
    let mut header = ResponseHeader::build(200, None).unwrap();
    header.insert_header("content-length", "20").unwrap();
    header.insert_header("accept-ranges", "bytes").unwrap();
    // try invalid etag, should be cleared
    header.insert_header("etag", "abc123").unwrap();
    adjust_response_header(&mut header, &Compress(Gzip), false);
    assert_eq!(
        header.headers.get("content-encoding").unwrap().as_bytes(),
        b"gzip"
    );
    assert!(header.headers.get("content-length").is_none());
    assert!(header.headers.get("accept-ranges").is_none());
    assert_eq!(
        header.headers.get("transfer-encoding").unwrap().as_bytes(),
        b"chunked"
    );
    assert!(header.headers.get("etag").is_none());
    // when preserve_etag on, etag is kept
    header.insert_header("etag", "abc123").unwrap();
    adjust_response_header(&mut header, &Compress(Gzip), true);
    assert_eq!(header.headers.get("etag").unwrap().as_bytes(), b"abc123");
}
