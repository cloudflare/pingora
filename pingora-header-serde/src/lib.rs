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
use pingora_error::{Error, ErrorType, Result};
use pingora_http::ResponseHeader;
use std::cell::RefCell;
use std::ops::DerefMut;
use thread_local::ThreadLocal;

/// HTTP Response header serialization
///
/// This struct provides the APIs to convert HTTP response header into compressed wired format for
/// storage.
pub struct HeaderSerde {
    compression: thread_zstd::Compression,
    level: i32,
    // internal buffer for uncompressed data to be compressed and vice versa
    buf: ThreadLocal<RefCell<Vec<u8>>>,
}

const MAX_HEADER_SIZE: usize = 64 * 1024;
const COMPRESS_LEVEL: i32 = 3;

impl HeaderSerde {
    /// Create a new [HeaderSerde]
    ///
    /// An optional zstd compression dictionary can be provided to improve the compression ratio
    /// and speed. See [dict] for more details.
    pub fn new(dict: Option<Vec<u8>>) -> Self {
        if let Some(dict) = dict {
            HeaderSerde {
                compression: thread_zstd::Compression::with_dict(dict),
                level: COMPRESS_LEVEL,
                buf: ThreadLocal::new(),
            }
        } else {
            HeaderSerde {
                compression: thread_zstd::Compression::new(),
                level: COMPRESS_LEVEL,
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
            .get_or(|| RefCell::new(Vec::with_capacity(MAX_HEADER_SIZE)))
            .borrow_mut();
        buf.clear(); // reset the buf
        resp_header_to_buf(header, &mut buf);
        self.compression
            .compress(&buf, self.level)
            .map_err(|e| into_error(e, "compress header"))
    }

    /// Deserialize the given response header
    pub fn deserialize(&self, data: &[u8]) -> Result<ResponseHeader> {
        let mut buf = self
            .buf
            .get_or(|| RefCell::new(Vec::with_capacity(MAX_HEADER_SIZE)))
            .borrow_mut();
        buf.clear(); // reset the buf
        self.compression
            .decompress_to_buffer(data, buf.deref_mut())
            .map_err(|e| into_error(e, "decompress header"))?;
        buf_to_http_header(&buf)
    }
}

#[inline]
fn into_error(e: &'static str, context: &'static str) -> Box<Error> {
    Error::because(ErrorType::InternalError, context, e)
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

// Should match pingora http1 setting
const MAX_HEADERS: usize = 256;

#[inline]
fn buf_to_http_header(buf: &[u8]) -> Result<ResponseHeader> {
    let mut headers = vec![httparse::EMPTY_HEADER; MAX_HEADERS];
    let mut resp = httparse::Response::new(&mut headers);

    match resp.parse(buf) {
        Ok(s) => match s {
            httparse::Status::Complete(_size) => parsed_to_header(&resp),
            // we always feed the but that contains the entire header to parse
            _ => Error::e_explain(ErrorType::InternalError, "incomplete uncompressed header"),
        },
        Err(e) => Error::e_because(
            ErrorType::InternalError,
            format!(
                "parsing failed on uncompressed header, {}",
                String::from_utf8_lossy(buf)
            ),
            e,
        ),
    }
}

#[inline]
fn parsed_to_header(parsed: &httparse::Response) -> Result<ResponseHeader> {
    // code should always be there
    let mut resp = ResponseHeader::build(parsed.code.unwrap(), Some(parsed.headers.len()))?;

    for header in parsed.headers.iter() {
        resp.append_header(header.name.to_string(), header.value)?;
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
}
