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

//! HTTP/1 parser compatibility layer for Pingora.
//!
//! This crate owns Pingora's dependency on a concrete HTTP/1 parser. Callers
//! use the normalized types below so parser backends can be selected without
//! changing protocol code.

use std::fmt;

mod backend;

pub use backend::{HeaderStorage, Headers, Request, Response};

/// Number of header fields parsed without heap-allocating parser storage.
pub const INLINE_HEADER_CAPACITY: usize = 256;

/// The default maximum number of HTTP/1 header fields.
pub const DEFAULT_HEADER_LIMIT: HeaderLimit = HeaderLimit::Limited(INLINE_HEADER_CAPACITY);

/// Compile-time capabilities of the selected parser backend.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Capabilities {
    /// Whether fields beyond [`INLINE_HEADER_CAPACITY`] are collected without
    /// reparsing the header block.
    pub single_pass_header_overflow: bool,
    /// Whether non-ASCII response header names are preserved.
    pub non_ascii_response_header_names: bool,
}

/// Return the capabilities of the selected parser backend.
pub const fn capabilities() -> Capabilities {
    backend::CAPABILITIES
}

/// Maximum number of HTTP/1 header fields accepted by a parser.
///
/// [`HeaderLimit::Unlimited`] removes only the independent field-count guard.
/// Callers should still impose a byte-size limit on the complete header block.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HeaderLimit {
    /// Reject a message with more than the given number of fields.
    Limited(usize),
    /// Apply no independent field-count limit.
    Unlimited,
}

impl HeaderLimit {
    /// Create a finite field-count limit.
    pub const fn limited(max: usize) -> Self {
        Self::Limited(max)
    }

    /// Remove the independent field-count limit.
    pub const fn unlimited() -> Self {
        Self::Unlimited
    }

    /// Return the finite limit, or [`None`] when it is disabled.
    pub const fn max(self) -> Option<usize> {
        match self {
            Self::Limited(max) => Some(max),
            Self::Unlimited => None,
        }
    }
}

impl Default for HeaderLimit {
    fn default() -> Self {
        DEFAULT_HEADER_LIMIT
    }
}

/// Configuration for parsing an HTTP/1 response head.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ResponseParserConfig {
    /// Maximum number of header fields accepted.
    pub header_limit: HeaderLimit,
    /// Allow whitespace between a header name and its colon. Defaults to `false`.
    pub allow_spaces_after_header_name_in_responses: bool,
    /// Allow obsolete line folding in header values. Defaults to `false`.
    pub allow_obsolete_multiline_headers_in_responses: bool,
}

impl ResponseParserConfig {
    /// Create strict response parser configuration with the given field-count limit.
    pub const fn new(header_limit: HeaderLimit) -> Self {
        Self {
            header_limit,
            allow_spaces_after_header_name_in_responses: false,
            allow_obsolete_multiline_headers_in_responses: false,
        }
    }

    /// Configure whether whitespace is allowed between a header name and its colon.
    pub const fn with_spaces_after_header_name_in_responses(mut self, allow: bool) -> Self {
        self.allow_spaces_after_header_name_in_responses = allow;
        self
    }

    /// Configure whether obsolete line folding is allowed in header values.
    pub const fn with_obsolete_multiline_headers_in_responses(mut self, allow: bool) -> Self {
        self.allow_obsolete_multiline_headers_in_responses = allow;
        self
    }
}

impl Default for ResponseParserConfig {
    fn default() -> Self {
        Self::new(HeaderLimit::default())
    }
}

/// A normalized HTTP/1 parser error.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    /// Invalid byte in a header name.
    HeaderName,
    /// Invalid byte in a header value.
    HeaderValue,
    /// Invalid newline.
    NewLine,
    /// Invalid response status.
    Status,
    /// Invalid byte where a token was required.
    Token,
    /// The configured header limit was exceeded.
    TooManyHeaders,
    /// Invalid HTTP version.
    Version,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::HeaderName => "invalid header name",
            Self::HeaderValue => "invalid header value",
            Self::NewLine => "invalid new line",
            Self::Status => "invalid response status",
            Self::Token => "invalid token",
            Self::TooManyHeaders => "too many headers",
            Self::Version => "invalid HTTP version",
        })
    }
}

impl std::error::Error for Error {}

/// Invalid HTTP/1 chunk-size line.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InvalidChunkSize;

impl fmt::Display for InvalidChunkSize {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("invalid chunk size")
    }
}

impl std::error::Error for InvalidChunkSize {}

/// The result of a successful parse pass.
#[derive(Debug)]
pub enum Status<T> {
    /// The input contains a complete value and the number of consumed bytes.
    Complete(usize, T),
    /// More input is required.
    Partial,
}

/// A borrowed HTTP/1 header field.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Header<'buf> {
    /// Original field name.
    pub name: &'buf str,
    /// Original field value.
    pub value: &'buf [u8],
}

/// Parse an HTTP/1 request head from `buf`.
///
/// The caller owns `storage`, which provides the [`HeaderStorage`] used for
/// parsed header fields. Keep both `storage` and `buf` alive for as long as
/// the returned [`Request`] is used. The parser borrows header names and
/// values from `buf` and the parsed field slice from `storage`.
///
/// On success, [`Status::Complete`] contains the number of bytes consumed
/// through the end of the request head and the parsed [`Request`]. The
/// [`Status::Partial`] result means that more bytes are needed; it is not a
/// successfully parsed request and the caller should provide more input.
///
/// [`HeaderLimit`] limits the number of header fields independently of any
/// byte-size limit the caller applies to `buf`.
pub fn parse_request<'headers, 'buf>(
    storage: &'headers mut HeaderStorage<'buf>,
    buf: &'buf [u8],
    limit: HeaderLimit,
) -> Result<Status<Request<'headers, 'buf>>, Error> {
    backend::parse_request(storage, buf, limit)
}

/// Parse an HTTP/1 response head from `buf`.
///
/// The caller owns `storage`, which provides the [`HeaderStorage`] used for
/// parsed header fields. Keep both `storage` and `buf` alive for as long as
/// the returned [`Response`] is used. The parser borrows header names and
/// values from `buf` and the parsed field slice from `storage`.
///
/// On success, [`Status::Complete`] contains the number of bytes consumed
/// through the end of the response head and the parsed [`Response`]. The
/// [`Status::Partial`] result means that more bytes are needed; it is not a
/// successfully parsed response and the caller should provide more input.
///
/// [`ResponseParserConfig::default`] applies the default [`HeaderLimit`] and
/// strictly rejects whitespace between a header name and its colon and
/// obsolete line folding (obs-fold). Set the corresponding
/// [`ResponseParserConfig`] options explicitly to accept those legacy forms.
pub fn parse_response<'headers, 'buf>(
    storage: &'headers mut HeaderStorage<'buf>,
    buf: &'buf [u8],
    config: ResponseParserConfig,
) -> Result<Status<Response<'headers, 'buf>>, Error> {
    backend::parse_response(storage, buf, config)
}

/// Parse an HTTP/1 chunk-size line from `buf`.
///
/// On success, [`Status::Complete`] contains the number of bytes consumed by
/// the chunk-size line and the parsed size. [`Status::Partial`] means that
/// more input is needed to determine the size. A malformed or invalid line
/// returns [`InvalidChunkSize`].
pub fn parse_chunk_size(buf: &[u8]) -> Result<Status<u64>, InvalidChunkSize> {
    backend::parse_chunk_size(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request_with_headers(count: usize) -> Vec<u8> {
        let mut request = b"GET / HTTP/1.1\r\n".to_vec();
        for i in 0..count {
            request.extend_from_slice(format!("x-{i}: value\r\n").as_bytes());
        }
        request.extend_from_slice(b"\r\n");
        request
    }

    fn response_with_headers(count: usize) -> Vec<u8> {
        let mut response = b"HTTP/1.1 200 OK\r\n".to_vec();
        for i in 0..count {
            response.extend_from_slice(format!("x-{i}: value\r\n").as_bytes());
        }
        response.extend_from_slice(b"\r\n");
        response
    }

    #[test]
    fn default_limit_is_preserved() {
        let input = request_with_headers(INLINE_HEADER_CAPACITY + 1);
        let mut storage = HeaderStorage::new();
        assert_eq!(
            parse_request(&mut storage, &input, DEFAULT_HEADER_LIMIT).unwrap_err(),
            Error::TooManyHeaders
        );
    }

    #[test]
    fn raised_request_limit_uses_expanded_storage() {
        let input = request_with_headers(4096);
        let mut storage = HeaderStorage::new();
        let Status::Complete(consumed, request) =
            parse_request(&mut storage, &input, HeaderLimit::Limited(4096)).unwrap()
        else {
            panic!("request should be complete");
        };
        assert_eq!(consumed, input.len());
        assert_eq!(request.header_count(), 4096);
        assert_eq!(request.headers().nth(256).unwrap().name, "x-256");
    }

    #[test]
    fn configured_limit_is_exact() {
        let input = response_with_headers(257);
        let mut storage = HeaderStorage::new();
        assert!(matches!(
            parse_response(
                &mut storage,
                &input,
                ResponseParserConfig::new(HeaderLimit::Limited(257))
            ),
            Ok(Status::Complete(_, _))
        ));

        let input = response_with_headers(258);
        let mut storage = HeaderStorage::new();
        assert_eq!(
            parse_response(
                &mut storage,
                &input,
                ResponseParserConfig::new(HeaderLimit::Limited(257))
            )
            .unwrap_err(),
            Error::TooManyHeaders
        );
    }

    #[test]
    fn overflow_can_still_be_partial() {
        let mut input = request_with_headers(300);
        input.truncate(input.len() - 2);
        let mut storage = HeaderStorage::new();
        assert!(matches!(
            parse_request(&mut storage, &input, HeaderLimit::Limited(512)),
            Ok(Status::Partial)
        ));
    }

    #[test]
    fn duplicate_order_is_preserved_across_inline_boundary() {
        let mut input = b"GET / HTTP/1.1\r\n".to_vec();
        for i in 0..258 {
            input.extend_from_slice(format!("x-duplicate: {i}\r\n").as_bytes());
        }
        input.extend_from_slice(b"\r\n");

        let mut storage = HeaderStorage::new();
        let Status::Complete(_, request) =
            parse_request(&mut storage, &input, HeaderLimit::Limited(258)).unwrap()
        else {
            panic!("request should be complete");
        };
        let values: Vec<_> = request
            .headers()
            .skip(255)
            .map(|header| header.value)
            .collect();
        assert_eq!(
            values,
            [b"255".as_slice(), b"256".as_slice(), b"257".as_slice()]
        );
    }

    #[test]
    fn unlimited_response_uses_expanded_storage() {
        let input = response_with_headers(4096);
        let mut storage = HeaderStorage::new();
        let Status::Complete(consumed, response) = parse_response(
            &mut storage,
            &input,
            ResponseParserConfig::new(HeaderLimit::Unlimited),
        )
        .unwrap() else {
            panic!("response should be complete");
        };
        assert_eq!(consumed, input.len());
        assert_eq!(response.header_count(), 4096);
        assert_eq!(response.headers().nth(256).unwrap().name, "x-256");
    }

    #[test]
    fn configured_limit_does_not_preallocate_beyond_the_input() {
        let input = request_with_headers(INLINE_HEADER_CAPACITY + 1);
        let mut storage = HeaderStorage::new();
        let Status::Complete(_, request) =
            parse_request(&mut storage, &input, HeaderLimit::Limited(usize::MAX)).unwrap()
        else {
            panic!("request should be complete");
        };
        assert_eq!(request.header_count(), INLINE_HEADER_CAPACITY + 1);
    }

    #[test]
    fn spaces_after_header_name_are_opt_in() {
        let input = b"HTTP/1.1 200 OK\r\nname : value\r\n\r\n";
        let mut storage = HeaderStorage::new();
        assert!(parse_response(&mut storage, input, ResponseParserConfig::default()).is_err());

        let config =
            ResponseParserConfig::default().with_spaces_after_header_name_in_responses(true);
        let Status::Complete(_, response) = parse_response(&mut storage, input, config).unwrap()
        else {
            panic!("response should be complete");
        };
        assert_eq!(response.header_count(), 1);
        assert_eq!(response.headers().next().unwrap().value, b"value");
    }

    #[test]
    fn obsolete_multiline_headers_are_opt_in() {
        let input = b"HTTP/1.1 200 OK\r\nfolded: one\r\n two\r\n\r\n";
        let mut storage = HeaderStorage::new();
        assert!(parse_response(&mut storage, input, ResponseParserConfig::default()).is_err());

        let config =
            ResponseParserConfig::default().with_obsolete_multiline_headers_in_responses(true);
        let Status::Complete(_, response) = parse_response(&mut storage, input, config).unwrap()
        else {
            panic!("response should be complete");
        };
        assert_eq!(response.header_count(), 1);
        assert_eq!(response.headers().next().unwrap().value, b"one\r\n two");
    }
}
