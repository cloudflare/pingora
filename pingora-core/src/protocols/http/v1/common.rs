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

//! Common functions and constants

use http::{header, HeaderValue};
use log::warn;
use pingora_http::{HMap, RequestHeader, ResponseHeader};
use std::str;
use std::time::Duration;

use super::body::BodyWriter;
use crate::utils::KVRef;

pub(super) const MAX_HEADERS: usize = 256;

pub(super) const INIT_HEADER_BUF_SIZE: usize = 4096;
pub(super) const MAX_HEADER_SIZE: usize = 1048575;

pub(super) const BODY_BUF_LIMIT: usize = 1024 * 64;

pub const CRLF: &[u8; 2] = b"\r\n";
pub const HEADER_KV_DELIMITER: &[u8; 2] = b": ";

pub(super) enum HeaderParseState {
    Complete(usize),
    Partial,
    Invalid(httparse::Error),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum KeepaliveStatus {
    Timeout(Duration),
    Infinite,
    Off,
}

struct ConnectionValue {
    keep_alive: bool,
    upgrade: bool,
    close: bool,
}

impl ConnectionValue {
    fn new() -> Self {
        ConnectionValue {
            keep_alive: false,
            upgrade: false,
            close: false,
        }
    }

    fn close(mut self) -> Self {
        self.close = true;
        self
    }
    fn upgrade(mut self) -> Self {
        self.upgrade = true;
        self
    }
    fn keep_alive(mut self) -> Self {
        self.keep_alive = true;
        self
    }
}

fn parse_connection_header(value: &[u8]) -> ConnectionValue {
    // only parse keep-alive, close, and upgrade tokens
    // https://www.rfc-editor.org/rfc/rfc9110.html#section-7.6.1

    const KEEP_ALIVE: &str = "keep-alive";
    const CLOSE: &str = "close";
    const UPGRADE: &str = "upgrade";

    // fast path
    if value.eq_ignore_ascii_case(CLOSE.as_bytes()) {
        ConnectionValue::new().close()
    } else if value.eq_ignore_ascii_case(KEEP_ALIVE.as_bytes()) {
        ConnectionValue::new().keep_alive()
    } else if value.eq_ignore_ascii_case(UPGRADE.as_bytes()) {
        ConnectionValue::new().upgrade()
    } else {
        // slow path, parse the connection value
        let mut close = false;
        let mut upgrade = false;
        let value = str::from_utf8(value).unwrap_or("");
        for token in value
            .split(',')
            .map(|s| s.trim())
            .filter(|&x| !x.is_empty())
        {
            if token.eq_ignore_ascii_case(CLOSE) {
                close = true;
            } else if token.eq_ignore_ascii_case(UPGRADE) {
                upgrade = true;
            }
            if upgrade && close {
                return ConnectionValue::new().upgrade().close();
            }
        }
        if close {
            ConnectionValue::new().close()
        } else if upgrade {
            ConnectionValue::new().upgrade()
        } else {
            ConnectionValue::new()
        }
    }
}

pub(crate) fn init_body_writer_comm(body_writer: &mut BodyWriter, headers: &HMap) {
    let te_value = headers.get(http::header::TRANSFER_ENCODING);
    if is_header_value_chunked_encoding(te_value) {
        // transfer-encoding takes priority over content-length
        body_writer.init_chunked();
    } else {
        let content_length = header_value_content_length(headers.get(http::header::CONTENT_LENGTH));
        match content_length {
            Some(length) => {
                body_writer.init_content_length(length);
            }
            None => {
                /* TODO: 1. connection: keepalive cannot be used,
                2. mark connection must be closed */
                body_writer.init_http10();
            }
        }
    }
}

#[inline]
pub(super) fn is_header_value_chunked_encoding(
    header_value: Option<&http::header::HeaderValue>,
) -> bool {
    match header_value {
        Some(value) => value.as_bytes().eq_ignore_ascii_case(b"chunked"),
        None => false,
    }
}

pub(super) fn is_upgrade_req(req: &RequestHeader) -> bool {
    req.version == http::Version::HTTP_11 && req.headers.get(header::UPGRADE).is_some()
}

pub(super) fn is_expect_continue_req(req: &RequestHeader) -> bool {
    req.version == http::Version::HTTP_11
        // https://www.rfc-editor.org/rfc/rfc9110#section-10.1.1
        && req.headers.get(header::EXPECT).map_or(false, |v| {
            v.as_bytes().eq_ignore_ascii_case(b"100-continue")
        })
}

// Unlike the upgrade check on request, this function doesn't check the Upgrade or Connection header
// because when seeing 101, we assume the server accepts to switch protocol.
// In reality it is not common that some servers don't send all the required headers to establish
// websocket connections.
pub(super) fn is_upgrade_resp(header: &ResponseHeader) -> bool {
    header.status == 101 && header.version == http::Version::HTTP_11
}

#[inline]
pub fn header_value_content_length(
    header_value: Option<&http::header::HeaderValue>,
) -> Option<usize> {
    match header_value {
        Some(value) => buf_to_content_length(Some(value.as_bytes())),
        None => None,
    }
}

#[inline]
pub(super) fn buf_to_content_length(header_value: Option<&[u8]>) -> Option<usize> {
    match header_value {
        Some(buf) => {
            match str::from_utf8(buf) {
                // check valid string
                Ok(str_cl_value) => match str_cl_value.parse::<i64>() {
                    Ok(cl_length) => {
                        if cl_length >= 0 {
                            Some(cl_length as usize)
                        } else {
                            warn!("negative content-length header value {cl_length}");
                            None
                        }
                    }
                    Err(_) => {
                        warn!("invalid content-length header value {str_cl_value}");
                        None
                    }
                },
                Err(_) => {
                    warn!("invalid content-length header encoding");
                    None
                }
            }
        }
        None => None,
    }
}

#[inline]
pub(super) fn is_buf_keepalive(header_value: Option<&HeaderValue>) -> Option<bool> {
    header_value.and_then(|value| {
        let value = parse_connection_header(value.as_bytes());
        if value.keep_alive {
            Some(true)
        } else if value.close {
            Some(false)
        } else {
            None
        }
    })
}

#[inline]
pub(super) fn populate_headers(
    base: usize,
    header_ref: &mut Vec<KVRef>,
    headers: &[httparse::Header],
) -> usize {
    let mut used_header_index = 0;
    for header in headers.iter() {
        if !header.name.is_empty() {
            header_ref.push(KVRef::new(
                header.name.as_ptr() as usize - base,
                header.name.as_bytes().len(),
                header.value.as_ptr() as usize - base,
                header.value.len(),
            ));
            used_header_index += 1;
        }
    }
    used_header_index
}
