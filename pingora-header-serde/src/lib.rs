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

//! HTTP Response header serialization with compression
//!
//! This crate is able to serialize http response header to about 1/3 of its original size (HTTP/1.1 wire format)
//! with trained dictionary.

#![warn(clippy::all)]
#![allow(clippy::new_without_default)]
#![allow(clippy::type_complexity)]

pub mod dict;
mod thread_zstd;

use bytes::BufMut;
use http::Version;
use pingora_error::{Error, ErrorType, ImmutStr, Result};
pub use pingora_http::h1::{HeaderLimit, ResponseParserConfig};
use pingora_http::ResponseHeader;
use pingora_http_parser::{
    parse_response, HeaderStorage, Response as ParsedResponse, Status as ParseStatus,
};
use std::cell::RefCell;
use std::ops::DerefMut;
use thread_local::ThreadLocal;

/// HTTP Response header serialization
///
/// This struct provides the APIs to convert HTTP response header into compressed wired format for
/// storage.
pub struct HeaderSerde {
    compression: ZstdCompression,
    parser_config: ResponseParserConfig,
    // internal buffer for uncompressed data to be compressed and vice versa
    buf: ThreadLocal<RefCell<Vec<u8>>>,
}

const MAX_HEADER_BUF_SIZE: usize = 128 * 1024; // 128KB

const COMPRESS_LEVEL: i32 = 3;

impl HeaderSerde {
    /// Create a new [HeaderSerde]
    ///
    /// An optional zstd compression dictionary can be provided to improve the compression ratio
    /// and speed. See [dict] for more details. Deserialization uses the default
    /// [`HeaderLimit`]; use [`Self::new_with_header_limit`] or
    /// [`Self::new_with_parser_config`] to accept a different field count or
    /// legacy response syntax.
    pub fn new(dict: Option<Vec<u8>>) -> Self {
        Self::new_with_header_limit(dict, HeaderLimit::default())
    }

    /// Create a new [`HeaderSerde`] with a field-count limit for deserialization.
    pub fn new_with_header_limit(dict: Option<Vec<u8>>, header_limit: HeaderLimit) -> Self {
        Self::new_with_parser_config(dict, ResponseParserConfig::new(header_limit))
    }

    /// Create a new [`HeaderSerde`] with response parser configuration for deserialization.
    pub fn new_with_parser_config(
        dict: Option<Vec<u8>>,
        parser_config: ResponseParserConfig,
    ) -> Self {
        if let Some(dict) = dict {
            HeaderSerde {
                compression: ZstdCompression::WithDict(thread_zstd::CompressionWithDict::new(
                    &dict,
                    COMPRESS_LEVEL,
                )),
                parser_config,
                buf: ThreadLocal::new(),
            }
        } else {
            HeaderSerde {
                compression: ZstdCompression::Default(
                    thread_zstd::Compression::new(),
                    COMPRESS_LEVEL,
                ),
                parser_config,
                buf: ThreadLocal::new(),
            }
        }
    }

    /// Serialize the given response header
    pub fn serialize(&self, header: &ResponseHeader) -> Result<Vec<u8>> {
        // for now we use HTTP 1.1 wire format for that
        // TODO: should convert to h1 if the incoming header is for h2
        let mut buf = self
            .buf
            .get_or(|| RefCell::new(Vec::with_capacity(MAX_HEADER_BUF_SIZE)))
            .borrow_mut();
        buf.clear(); // reset the buf
        resp_header_to_buf(header, &mut buf);
        self.compression.compress(&buf)
    }

    /// Deserialize the given response header
    pub fn deserialize(&self, data: &[u8]) -> Result<ResponseHeader> {
        let mut buf = self
            .buf
            .get_or(|| RefCell::new(Vec::with_capacity(MAX_HEADER_BUF_SIZE)))
            .borrow_mut();
        buf.clear(); // reset the buf
        self.compression
            .decompress_to_buffer(data, buf.deref_mut())?;
        buf_to_http_header_with_parser_config(&buf, self.parser_config)
    }
}

// Wrapper type to unify compressing with and withuot a dictionary,
// since the two structs have different inputs for their APIs.
enum ZstdCompression {
    Default(thread_zstd::Compression, i32),
    WithDict(thread_zstd::CompressionWithDict),
}

#[inline]
fn into_error<S: Into<ImmutStr>>(e: &'static str, context: S) -> Box<Error> {
    Error::because(ErrorType::InternalError, context, e)
}

impl ZstdCompression {
    fn compress(&self, data: &[u8]) -> Result<Vec<u8>> {
        match &self {
            ZstdCompression::Default(c, level) => c
                .compress(data, *level)
                .map_err(|e| into_error(e, "compress header")),
            ZstdCompression::WithDict(c) => c
                .compress(data)
                .map_err(|e| into_error(e, "compress header")),
        }
    }

    fn decompress_to_buffer(&self, source: &[u8], destination: &mut Vec<u8>) -> Result<usize> {
        match &self {
            ZstdCompression::Default(c, _) => {
                c.decompress_to_buffer(source, destination).map_err(|e| {
                    into_error(
                        e,
                        format!(
                            "decompress header, frame_content_size: {}",
                            get_frame_content_size(source)
                        ),
                    )
                })
            }
            ZstdCompression::WithDict(c) => {
                c.decompress_to_buffer(source, destination).map_err(|e| {
                    into_error(
                        e,
                        format!(
                            "decompress header, frame_content_size: {}",
                            get_frame_content_size(source)
                        ),
                    )
                })
            }
        }
    }
}

#[inline]
fn get_frame_content_size(source: &[u8]) -> ImmutStr {
    match zstd_safe::get_frame_content_size(source) {
        Ok(Some(size)) => match size {
            zstd_safe::CONTENTSIZE_ERROR => ImmutStr::from("invalid"),
            zstd_safe::CONTENTSIZE_UNKNOWN => ImmutStr::from("unknown"),
            _ => ImmutStr::from(size.to_string()),
        },
        Ok(None) => ImmutStr::from("none"),
        Err(_e) => ImmutStr::from("failed"),
    }
}

const CRLF: &[u8; 2] = b"\r\n";

// Borrowed from pingora http1
#[inline]
fn resp_header_to_buf(resp: &ResponseHeader, buf: &mut Vec<u8>) -> usize {
    // Status-Line
    let version = match resp.version {
        Version::HTTP_10 => "HTTP/1.0 ",
        Version::HTTP_11 => "HTTP/1.1 ",
        _ => "HTTP/1.1 ", // store everything else (including h2) in http 1.1 format
    };
    buf.put_slice(version.as_bytes());
    let status = resp.status;
    buf.put_slice(status.as_str().as_bytes());
    buf.put_u8(b' ');
    let reason = status.canonical_reason();
    if let Some(reason_buf) = reason {
        buf.put_slice(reason_buf.as_bytes());
    }
    buf.put_slice(CRLF);

    // headers
    resp.header_to_h1_wire(buf);

    buf.put_slice(CRLF);

    buf.len()
}

#[inline]
#[cfg(test)]
fn buf_to_http_header(buf: &[u8]) -> Result<ResponseHeader> {
    buf_to_http_header_with_parser_config(buf, ResponseParserConfig::new(HeaderLimit::Unlimited))
}

#[inline]
fn buf_to_http_header_with_parser_config(
    buf: &[u8],
    parser_config: ResponseParserConfig,
) -> Result<ResponseHeader> {
    let mut storage = HeaderStorage::new();
    match parse_response(&mut storage, buf, parser_config) {
        Ok(ParseStatus::Complete(_size, response)) => parsed_to_header(&response),
        Ok(ParseStatus::Partial) => {
            // We always feed the buffer that contains the entire header to parse.
            Error::e_explain(ErrorType::InternalError, "incomplete uncompressed header")
        }
        Err(e) => Error::e_because(
            ErrorType::InternalError,
            format!(
                "parsing failed on uncompressed header, len={}, content={:?}",
                buf.len(),
                String::from_utf8_lossy(buf)
            ),
            e,
        ),
    }
}

#[inline]
fn parsed_to_header(parsed: &ParsedResponse<'_, '_>) -> Result<ResponseHeader> {
    // code should always be there
    // TODO: allow reading the parsed http version?
    let mut resp = ResponseHeader::build(parsed.code().unwrap(), Some(parsed.header_count()))?;

    for header in parsed.headers() {
        let header_value = pingora_http::header_value_from_slice(header.value);
        resp.append_header(header.name.to_string(), header_value)?;
    }

    Ok(resp)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ser_wo_dict() {
        let serde = HeaderSerde::new(None);
        let mut header = ResponseHeader::build(200, None).unwrap();
        header.append_header("foo", "bar").unwrap();
        header.append_header("foo", "barbar").unwrap();
        header.append_header("foo", "barbarbar").unwrap();
        header.append_header("Server", "Pingora").unwrap();

        let compressed = serde.serialize(&header).unwrap();
        let mut buf = vec![];
        let uncompressed = resp_header_to_buf(&header, &mut buf);
        assert!(compressed.len() < uncompressed);
    }

    #[test]
    fn test_ser_de_no_dict() {
        let serde = HeaderSerde::new(None);
        let mut header = ResponseHeader::build(200, None).unwrap();
        header.append_header("foo1", "bar1").unwrap();
        header.append_header("foo2", "barbar2").unwrap();
        header.append_header("foo3", "barbarbar3").unwrap();
        header.append_header("Server", "Pingora").unwrap();

        let compressed = serde.serialize(&header).unwrap();
        let header2 = serde.deserialize(&compressed).unwrap();
        assert_eq!(header.status, header2.status);
        assert_eq!(header.headers, header2.headers);
    }

    #[test]
    fn test_serialize_deserialize_many_headers() {
        let serde = HeaderSerde::new_with_header_limit(None, HeaderLimit::Unlimited);
        let mut header = ResponseHeader::build(200, Some(4096)).unwrap();
        for i in 0..4096 {
            header
                .append_header(format!("x-{i}"), format!("value-{i}"))
                .unwrap();
        }

        let compressed = serde.serialize(&header).unwrap();
        let header2 = serde.deserialize(&compressed).unwrap();
        assert_eq!(header.headers, header2.headers);
    }

    #[test]
    fn test_default_header_limit() {
        let serde = HeaderSerde::new(None);
        let mut header = ResponseHeader::build(200, Some(257)).unwrap();
        for i in 0..257 {
            header
                .append_header(format!("x-{i}"), format!("value-{i}"))
                .unwrap();
        }

        let compressed = serde.serialize(&header).unwrap();
        assert!(serde.deserialize(&compressed).is_err());
    }

    #[test]
    fn test_ser_de_keeps_repeated_headers() {
        // repeated fields such as Set-Cookie survive a round trip instead of collapsing to one
        let serde = HeaderSerde::new(None);
        let mut header = ResponseHeader::build(200, None).unwrap();
        header.append_header("Set-Cookie", "a=1").unwrap();
        header.append_header("Set-Cookie", "b=2").unwrap();

        let compressed = serde.serialize(&header).unwrap();
        let header2 = serde.deserialize(&compressed).unwrap();
        let cookies: Vec<_> = header2.headers.get_all("set-cookie").iter().collect();
        assert_eq!(cookies, ["a=1", "b=2"]);
    }

    #[test]
    fn test_no_headers() {
        let serde = HeaderSerde::new(None);
        let header = ResponseHeader::build(200, None).unwrap(); // No headers added

        // Serialize and deserialize
        let compressed = serde.serialize(&header).unwrap();
        let header2 = serde.deserialize(&compressed).unwrap();

        assert_eq!(header.status, header2.status);
        assert_eq!(header.headers.len(), 0);
        assert_eq!(header2.headers.len(), 0);
    }

    #[test]
    fn test_empty_header_wire_format() {
        let header = ResponseHeader::build(200, None).unwrap();
        let mut buf = vec![];
        resp_header_to_buf(&header, &mut buf);

        // Should be: "HTTP/1.1 200 OK\r\n\r\n", total 19 bytes
        assert_eq!(buf.len(), 19);
        assert_eq!(buf, b"HTTP/1.1 200 OK\r\n\r\n");

        // Test that the configured HTTP/1 parser can handle this.
        let parsed = buf_to_http_header(&buf).unwrap();
        assert_eq!(parsed.status.as_u16(), 200);
    }

    #[test]
    fn test_serialize_deserialize_multiline_header() {
        let serde = HeaderSerde::new(None);
        let parser_config = ResponseParserConfig::new(HeaderLimit::Unlimited)
            .with_obsolete_multiline_headers_in_responses(true);

        let multiline_response = b"HTTP/1.1 200 OK\r\n\
Content-Security-Policy: default-src 'self';\r\n \
       script-src 'self' 'unsafe-inline' 'unsafe-eval' blob:;\r\n \
       connect-src 'self' wss:;\r\n \
       img-src 'self' data: blob:\r\n\
Content-Type: text/html\r\n\
\r\n";

        let header1 =
            buf_to_http_header_with_parser_config(multiline_response, parser_config).unwrap();
        let compressed = serde.serialize(&header1).unwrap();
        let header2 = serde.deserialize(&compressed).unwrap();

        assert_eq!(header1.status, header2.status);
        // After obs-fold normalization the parsed and the round-tripped
        // values must agree byte-for-byte.
        assert_eq!(header1.headers, header2.headers);

        let csp = header1.headers.get("content-security-policy").unwrap();
        let csp_bytes = csp.as_bytes();
        // No CR or LF must survive normalization — that's the whole point.
        assert!(!csp_bytes.contains(&b'\r'));
        assert!(!csp_bytes.contains(&b'\n'));
        // Each fold collapses to a single SP between the joined segments.
        let csp_str = std::str::from_utf8(csp_bytes).unwrap();
        assert_eq!(
            csp_str,
            "default-src 'self'; \
             script-src 'self' 'unsafe-inline' 'unsafe-eval' blob:; \
             connect-src 'self' wss:; \
             img-src 'self' data: blob:"
        );
    }

    #[test]
    fn test_serialize_normalizes_space_before_header_colon() {
        let parser_config =
            ResponseParserConfig::default().with_spaces_after_header_name_in_responses(true);
        let response = buf_to_http_header_with_parser_config(
            b"HTTP/1.1 200 OK\r\nName : value\r\n\r\n",
            parser_config,
        )
        .unwrap();

        let mut canonical = Vec::new();
        resp_header_to_buf(&response, &mut canonical);
        assert!(canonical
            .windows(b"Name: value\r\n".len())
            .any(|window| window == b"Name: value\r\n"));

        let serde = HeaderSerde::new(None);
        let compressed = serde.serialize(&response).unwrap();
        let round_tripped = serde.deserialize(&compressed).unwrap();
        assert_eq!(response.headers, round_tripped.headers);
    }

    #[test]
    fn test_parsing_multiline_header() {
        let parser_config = ResponseParserConfig::new(HeaderLimit::Unlimited)
            .with_obsolete_multiline_headers_in_responses(true);
        let multiline_response = b"HTTP/1.1 200 OK\r\n\
X-Custom: foo\r\n \
       bar\r\n \
       baz\r\n\
\r\n";

        let header =
            buf_to_http_header_with_parser_config(multiline_response, parser_config).unwrap();

        assert!(header.headers.contains_key("x-custom"));
        assert_eq!(header.headers.len(), 1);

        let custom = header.headers.get("x-custom").unwrap();
        let custom_bytes = custom.as_bytes();
        assert!(!custom_bytes.contains(&b'\r'));
        assert!(!custom_bytes.contains(&b'\n'));
        // "foo\r\n <ws>bar\r\n <ws>baz" -> "foo bar baz" (each fold a single SP).
        assert_eq!(custom_bytes, b"foo bar baz");
    }

    #[test]
    fn test_response_compatibility_syntax_is_rejected_by_default() {
        assert!(buf_to_http_header(b"HTTP/1.1 200 OK\r\nName : value\r\n\r\n").is_err());
        assert!(buf_to_http_header(b"HTTP/1.1 200 OK\r\nName: value\r\n folded\r\n\r\n").is_err());
    }
}
