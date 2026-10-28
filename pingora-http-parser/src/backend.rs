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

use super::{
    Capabilities, Error, Header, HeaderLimit, InvalidChunkSize, ResponseParserConfig, Status,
    INLINE_HEADER_CAPACITY,
};

pub const CAPABILITIES: Capabilities = Capabilities {
    single_pass_header_overflow: false,
    non_ascii_response_header_names: cfg!(feature = "patched_http1"),
};

/// Caller-owned storage used while parsing one HTTP/1 head.
///
/// The inline storage preserves Pingora's allocation-free common path. The
/// expanded storage is used only when the configured limit permits more fields
/// and the inline parse reports [`Error::TooManyHeaders`].
pub struct HeaderStorage<'buf> {
    inline: [httparse::Header<'buf>; INLINE_HEADER_CAPACITY],
    expanded: Vec<httparse::Header<'buf>>,
}

impl HeaderStorage<'_> {
    /// Create empty parser storage.
    pub fn new() -> Self {
        Self {
            inline: [httparse::EMPTY_HEADER; INLINE_HEADER_CAPACITY],
            expanded: Vec::new(),
        }
    }
}

impl Default for HeaderStorage<'_> {
    fn default() -> Self {
        Self::new()
    }
}

/// Iterator over parsed header fields in wire order.
pub struct Headers<'headers, 'buf> {
    inner: std::slice::Iter<'headers, httparse::Header<'buf>>,
}

impl<'buf> Iterator for Headers<'_, 'buf> {
    type Item = Header<'buf>;

    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next().map(|header| Header {
            name: header.name,
            value: header.value,
        })
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.inner.size_hint()
    }
}

impl ExactSizeIterator for Headers<'_, '_> {}

/// A parsed HTTP/1 request head.
#[derive(Debug)]
pub struct Request<'headers, 'buf> {
    inner: httparse::Request<'headers, 'buf>,
}

impl<'buf> Request<'_, 'buf> {
    /// Request method.
    pub fn method(&self) -> Option<&'buf str> {
        self.inner.method
    }

    /// Request target.
    pub fn path(&self) -> Option<&'buf str> {
        self.inner.path
    }

    /// Request target as raw bytes.
    pub fn path_bytes(&self) -> Option<&'buf [u8]> {
        self.inner.path.map(str::as_bytes)
    }

    /// HTTP minor version.
    pub fn version(&self) -> Option<u8> {
        self.inner.version
    }

    /// Number of parsed header fields.
    pub fn header_count(&self) -> usize {
        self.inner.headers.len()
    }

    /// Parsed header fields in wire order.
    pub fn headers(&self) -> Headers<'_, 'buf> {
        Headers {
            inner: self.inner.headers.iter(),
        }
    }
}

/// A parsed HTTP/1 response head.
#[derive(Debug)]
pub struct Response<'headers, 'buf> {
    inner: httparse::Response<'headers, 'buf>,
}

impl<'buf> Response<'_, 'buf> {
    /// HTTP minor version.
    pub fn version(&self) -> Option<u8> {
        self.inner.version
    }

    /// Response status code.
    pub fn code(&self) -> Option<u16> {
        self.inner.code
    }

    /// Response reason phrase.
    pub fn reason(&self) -> Option<&'buf str> {
        self.inner.reason
    }

    /// Number of parsed header fields.
    pub fn header_count(&self) -> usize {
        self.inner.headers.len()
    }

    /// Parsed header fields in wire order.
    pub fn headers(&self) -> Headers<'_, 'buf> {
        Headers {
            inner: self.inner.headers.iter(),
        }
    }
}

impl From<httparse::Error> for Error {
    fn from(value: httparse::Error) -> Self {
        match value {
            httparse::Error::HeaderName => Self::HeaderName,
            httparse::Error::HeaderValue => Self::HeaderValue,
            httparse::Error::NewLine => Self::NewLine,
            httparse::Error::Status => Self::Status,
            httparse::Error::Token => Self::Token,
            httparse::Error::TooManyHeaders => Self::TooManyHeaders,
            httparse::Error::Version => Self::Version,
        }
    }
}

fn initial_capacity(limit: HeaderLimit) -> usize {
    match limit {
        HeaderLimit::Limited(limit) => limit.min(INLINE_HEADER_CAPACITY),
        HeaderLimit::Unlimited => INLINE_HEADER_CAPACITY,
    }
}

fn expanded_capacity(limit: HeaderLimit, current: usize, buf: &[u8]) -> Option<usize> {
    let mut present_fields_upper_bound = 0;
    for (index, &byte) in buf.iter().enumerate() {
        if byte != b'\n' {
            continue;
        }

        present_fields_upper_bound += 1;
        let remainder = buf.get(index + 1..);
        let header_complete =
            matches!(remainder, Some([b'\n', ..])) || matches!(remainder, Some([b'\r', b'\n', ..]));
        if header_complete {
            present_fields_upper_bound += 1;
            break;
        }
    }
    let present_fields_upper_bound = present_fields_upper_bound.max(current.saturating_add(1));

    match limit {
        HeaderLimit::Limited(limit) if limit > current => {
            Some(limit.min(present_fields_upper_bound))
        }
        HeaderLimit::Unlimited => Some(present_fields_upper_bound),
        HeaderLimit::Limited(_) => None,
    }
}

fn prepare_expanded_headers<'buf>(
    headers: &mut Vec<httparse::Header<'buf>>,
    capacity: usize,
) -> Result<(), Error> {
    headers.clear();
    headers
        .try_reserve_exact(capacity)
        .map_err(|_| Error::TooManyHeaders)?;
    headers.resize(capacity, httparse::EMPTY_HEADER);
    Ok(())
}

fn parse_request_once<'headers, 'buf>(
    headers: &'headers mut [httparse::Header<'buf>],
    buf: &'buf [u8],
) -> Result<Status<Request<'headers, 'buf>>, Error> {
    let mut request = httparse::Request::new(headers);
    #[cfg(feature = "patched_http1")]
    let result = request.parse_unchecked(buf);
    #[cfg(not(feature = "patched_http1"))]
    let result = request.parse(buf);

    match result {
        Ok(httparse::Status::Complete(consumed)) => {
            Ok(Status::Complete(consumed, Request { inner: request }))
        }
        Ok(httparse::Status::Partial) => Ok(Status::Partial),
        Err(error) => Err(error.into()),
    }
}

pub fn parse_request<'headers, 'buf>(
    storage: &'headers mut HeaderStorage<'buf>,
    buf: &'buf [u8],
    limit: HeaderLimit,
) -> Result<Status<Request<'headers, 'buf>>, Error> {
    let inline_capacity = initial_capacity(limit);
    match parse_request_once(&mut storage.inline[..inline_capacity], buf) {
        Err(Error::TooManyHeaders) => {}
        result => return result,
    }

    let capacity = match expanded_capacity(limit, inline_capacity, buf) {
        Some(capacity) => capacity,
        None => return Err(Error::TooManyHeaders),
    };

    prepare_expanded_headers(&mut storage.expanded, capacity)?;
    parse_request_once(&mut storage.expanded, buf)
}

fn response_parser(config: ResponseParserConfig) -> httparse::ParserConfig {
    let mut parser = httparse::ParserConfig::default();
    parser.allow_spaces_after_header_name_in_responses(
        config.allow_spaces_after_header_name_in_responses,
    );
    parser.allow_obsolete_multiline_headers_in_responses(
        config.allow_obsolete_multiline_headers_in_responses,
    );
    parser
}

fn parse_response_once<'headers, 'buf>(
    headers: &'headers mut [httparse::Header<'buf>],
    buf: &'buf [u8],
    config: ResponseParserConfig,
) -> Result<Status<Response<'headers, 'buf>>, Error> {
    let mut response = httparse::Response::new(headers);
    match response_parser(config).parse_response(&mut response, buf) {
        Ok(httparse::Status::Complete(consumed)) => {
            Ok(Status::Complete(consumed, Response { inner: response }))
        }
        Ok(httparse::Status::Partial) => Ok(Status::Partial),
        Err(error) => Err(error.into()),
    }
}

pub fn parse_response<'headers, 'buf>(
    storage: &'headers mut HeaderStorage<'buf>,
    buf: &'buf [u8],
    config: ResponseParserConfig,
) -> Result<Status<Response<'headers, 'buf>>, Error> {
    let inline_capacity = initial_capacity(config.header_limit);
    match parse_response_once(&mut storage.inline[..inline_capacity], buf, config) {
        Err(Error::TooManyHeaders) => {}
        result => return result,
    }

    let capacity = match expanded_capacity(config.header_limit, inline_capacity, buf) {
        Some(capacity) => capacity,
        None => return Err(Error::TooManyHeaders),
    };

    prepare_expanded_headers(&mut storage.expanded, capacity)?;
    parse_response_once(&mut storage.expanded, buf, config)
}

pub fn parse_chunk_size(buf: &[u8]) -> Result<Status<u64>, InvalidChunkSize> {
    match httparse::parse_chunk_size(buf).map_err(|_| InvalidChunkSize)? {
        httparse::Status::Complete((consumed, size)) => Ok(Status::Complete(consumed, size)),
        httparse::Status::Partial => Ok(Status::Partial),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expansion_ignores_overread_body_lines() {
        let mut input = b"HTTP/1.1 200 OK\r\n".to_vec();
        for i in 0..=INLINE_HEADER_CAPACITY {
            input.extend_from_slice(format!("x-{i}: value\r\n").as_bytes());
        }
        input.extend_from_slice(b"\r\n");
        input.extend(std::iter::repeat_n(b'\n', 10_000));

        assert_eq!(
            expanded_capacity(HeaderLimit::Unlimited, INLINE_HEADER_CAPACITY, &input),
            Some(INLINE_HEADER_CAPACITY + 3)
        );
    }

    #[test]
    fn expansion_supports_lf_only_header_termination() {
        let input = b"HTTP/1.1 200 OK\nx-one: value\nx-two: value\n\nbody\nlines\n";

        assert_eq!(expanded_capacity(HeaderLimit::Unlimited, 2, input), Some(4));
    }
}
