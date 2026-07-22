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

//! Common functions and constants

use http::{header, HeaderValue};
use log::{debug, warn};
use pingora_error::{Error, ErrorType::*, Result};
use pingora_http::{HMap, RequestHeader, ResponseHeader};
use std::str;
use std::time::Duration;

use super::body::BodyWriter;
use crate::utils::KVRef;

pub(super) const MAX_HEADERS: usize = 256;

pub(super) const INIT_HEADER_BUF_SIZE: usize = 4096;
pub(super) const MAX_HEADER_SIZE: usize = 1048575;

pub(crate) const BODY_BUF_LIMIT: usize = 1024 * 64;

pub const CRLF: &[u8; 2] = b"\r\n";
/// The empty line that terminates a message's header section.
pub(super) const HEADER_SECTION_END: &[u8; 4] = b"\r\n\r\n";
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
    if is_chunked_encoding_from_headers(headers) {
        // transfer-encoding takes priority over content-length
        body_writer.init_chunked();
        return;
    }
    // Resolve Content-Length through the shared, range-checked helper so the
    // value is reconciled (identical duplicates / comma lists) and never
    // truncated. For responses, an absent Content-Length is close-delimited.
    // An invalid value should have been rejected on read; if validation was
    // bypassed, log the anomaly and fall back to close-delimited rather than
    // risk misframing the body.
    match content_length_for_framing(headers) {
        Ok(Some(length)) => body_writer.init_content_length(length),
        Ok(None) => {
            /* TODO: 1. connection: keepalive cannot be used,
            2. mark connection must be closed */
            body_writer.init_close_delimited()
        }
        Err(e) => {
            // Headers are validated on read, so this is not expected here; log
            // the fallback to a close-delimited body rather than failing
            // silently if validation was somehow bypassed.
            debug!("invalid Content-Length while writing; framing close-delimited: {e}");
            body_writer.init_close_delimited()
        }
    }
}

/// Find the last comma-separated token in a Transfer-Encoding header value.
/// Takes the literal last token after the last comma, even if empty.
#[inline]
fn find_last_te_token(bytes: &[u8]) -> &[u8] {
    let last_token = bytes
        .iter()
        .rposition(|&b| b == b',')
        .map(|pos| &bytes[pos + 1..])
        .unwrap_or(bytes);

    last_token.trim_ascii()
}

/// Check if chunked encoding is the final encoding across all transfer-encoding headers
pub(crate) fn is_chunked_encoding_from_headers(headers: &HMap) -> bool {
    // Get the last Transfer-Encoding header value
    let last_te = headers
        .get_all(http::header::TRANSFER_ENCODING)
        .into_iter()
        .next_back();

    let Some(last_header_value) = last_te else {
        return false;
    };

    let bytes = last_header_value.as_bytes();

    // Fast path: exact match for "chunked"
    if bytes.eq_ignore_ascii_case(b"chunked") {
        return true;
    }

    // Slow path: parse comma-separated values
    find_last_te_token(bytes).eq_ignore_ascii_case(b"chunked")
}

pub fn is_upgrade_req(req: &RequestHeader) -> bool {
    req.version == http::Version::HTTP_11 && req.headers.get(header::UPGRADE).is_some()
}

pub fn is_expect_continue_req(req: &RequestHeader) -> bool {
    req.version == http::Version::HTTP_11
        // https://www.rfc-editor.org/rfc/rfc9110#section-10.1.1
        && req.headers.get(header::EXPECT).is_some_and(|v| {
            v.as_bytes().eq_ignore_ascii_case(b"100-continue")
        })
}

// Unlike the upgrade check on request, this function doesn't check the Upgrade or Connection header
// because when seeing 101, we assume the server accepts to switch protocol.
// In reality it is not common that some servers don't send all the required headers to establish
// websocket connections.
pub fn is_upgrade_resp(header: &ResponseHeader) -> bool {
    header.status == 101 && header.version == http::Version::HTTP_11
}

#[inline]
pub fn header_value_content_length(
    header_value: Option<&http::header::HeaderValue>,
) -> Option<usize> {
    match header_value {
        Some(value) => buf_to_content_length(Some(value.as_bytes())).ok().flatten(),
        None => None,
    }
}

#[inline]
pub(super) fn buf_to_content_length(header_value: Option<&[u8]>) -> Result<Option<usize>> {
    match header_value {
        Some(buf) => {
            match str::from_utf8(buf) {
                // check valid string
                Ok(str_cl_value) => match str_cl_value.parse::<i64>() {
                    Ok(cl_length) => {
                        if cl_length >= 0 {
                            Ok(Some(cl_length as usize))
                        } else {
                            warn!("negative content-length header value {cl_length}");
                            Error::e_explain(
                                InvalidHTTPHeader,
                                format!("negative Content-Length header value: {cl_length}"),
                            )
                        }
                    }
                    Err(_) => {
                        warn!("invalid content-length header value {str_cl_value}");
                        Error::e_explain(
                            InvalidHTTPHeader,
                            format!("invalid Content-Length header value: {str_cl_value}"),
                        )
                    }
                },
                Err(_) => {
                    warn!("invalid content-length header encoding");
                    Error::e_explain(InvalidHTTPHeader, "invalid Content-Length header encoding")
                }
            }
        }
        None => Ok(None),
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
                header.name.len(),
                header.value.as_ptr() as usize - base,
                header.value.len(),
            ));
            used_header_index += 1;
        }
    }
    used_header_index
}

/// Parse a `Content-Length` field value as an ASCII digit string, matching
/// hyper's `from_digits`
/// (<https://github.com/hyperium/hyper/blob/v1.9.0/src/headers.rs#L72-L97>).
///
/// Rejects empty input, any non-digit byte, a leading sign, and values that
/// overflow `u64`.
fn from_digits(bytes: &[u8]) -> Option<u64> {
    // cannot use FromStr for u64, since it allows a signed prefix
    const RADIX: u64 = 10;

    if bytes.is_empty() {
        return None;
    }

    let mut result = 0u64;
    for &b in bytes {
        // can't use char::to_digit, since we haven't verified these bytes
        // are utf-8.
        match b {
            b'0'..=b'9' => {
                result = result.checked_mul(RADIX)?;
                result = result.checked_add((b - b'0') as u64)?;
            }
            // not a DIGIT, get outta here!
            _ => return None,
        }
    }

    Some(result)
}

/// Reconcile every `Content-Length` value (across duplicate header lines and
/// comma-separated lists) into a single value, mirroring the behavior of
/// hyper's `content_length_parse_all`
/// (<https://github.com/hyperium/hyper/blob/v1.9.0/src/headers.rs#L40-L69>).
///
/// Returns `Some(n)` only when at least one value is present and every value
/// parses and is equal. Returns `None` when there are no values, when any value
/// is unparseable, or when two values disagree. Per [RFC 9110 section
/// 8.6](https://www.rfc-editor.org/rfc/rfc9110#section-8.6) this lets identical
/// duplicates (e.g. `Content-Length: 42, 42`) be collapsed to a single value
/// while genuinely conflicting framing is rejected.
pub(crate) fn content_length_parse_all(headers: &HMap) -> Option<u64> {
    // If multiple Content-Length headers were sent, everything can still be
    // alright if they all contain the same value, and all parse correctly. If
    // not, then it's an error.
    let mut content_length: Option<u64> = None;
    for value in headers.get_all(header::CONTENT_LENGTH) {
        // A non-visible-ASCII value can't be a valid Content-Length, so the
        // framing is ambiguous.
        let line = value.to_str().ok()?;
        for v in line.split(',') {
            let n = from_digits(v.trim().as_bytes())?;
            match content_length {
                None => content_length = Some(n),
                Some(prev) if prev != n => return None,
                Some(_) => {}
            }
        }
    }

    content_length
}

/// Resolve the framing `Content-Length` of a message following hyper's decode
/// rules: the reconciled value when valid, an error when a `Content-Length` is
/// present but cannot be resolved to a single value (conflicting duplicates or
/// an unparseable value), or `None` when the header is absent.
pub(crate) fn content_length_for_framing(headers: &HMap) -> Result<Option<usize>> {
    match content_length_parse_all(headers) {
        Some(cl) => {
            // Content-Length frames the body, so a value that does not fit in
            // `usize` cannot be represented and must not be silently truncated
            // (which would misframe bodies larger than `usize::MAX`, e.g. on
            // 32-bit targets). On 64-bit targets this conversion never fails.
            usize::try_from(cl).map(Some).or_else(|_| {
                // debug, not warn: this is a per-message, client-influenced
                // condition; the error is returned and surfaced by the caller.
                debug!("Content-Length {cl} exceeds the platform's usize::MAX");
                Error::e_explain(
                    InvalidHTTPHeader,
                    format!("Content-Length {cl} exceeds the platform maximum"),
                )
            })
        }
        None if headers.contains_key(header::CONTENT_LENGTH) => {
            // debug, not warn: per-message, client-influenced; avoids log floods
            // under adversarial load. The error is returned to the caller.
            debug!("invalid Content-Length header");
            Error::e_explain(InvalidHTTPHeader, "invalid Content-Length header")
        }
        None => Ok(None),
    }
}

/// Validate that a message's `Content-Length` framing is unambiguous.
///
/// Per [RFC 9110 section 8.6](https://www.rfc-editor.org/rfc/rfc9110#section-8.6),
/// a message received without `Transfer-Encoding` and with multiple
/// `Content-Length` fields having differing values (or a single invalid value)
/// has invalid framing and the recipient MUST treat it as an unrecoverable
/// error. Identical duplicates and comma-combined identical values are
/// reconciled to a single value via [`content_length_parse_all`].
///
/// HTTP/2 does not use `Content-Length` for framing, but reconciling it here
/// keeps the H2 and downgraded-H1 paths consistent and rejects genuinely
/// ambiguous values before they cross a proxy boundary.
pub(crate) fn validate_content_length(headers: &HMap) -> Result<()> {
    if headers.get(header::TRANSFER_ENCODING).is_some() {
        // HTTP/1: Transfer-Encoding takes precedence over Content-Length.
        return Ok(());
    }
    content_length_for_framing(headers).map(|_| ())
}

/// Validate `Content-Length` for transports where `Transfer-Encoding` is not
/// permitted.
///
/// HTTP/2 does not use `Content-Length` for framing and forbids the
/// `Transfer-Encoding` header entirely. The h2 codec should reject such messages
/// before they reach Pingora, but this keeps the shared H2 validation path
/// explicit and defensive.
pub(crate) fn validate_content_length_without_transfer_encoding(headers: &HMap) -> Result<()> {
    if headers.get(header::TRANSFER_ENCODING).is_some() {
        return Error::e_explain(InvalidHTTPHeader, "invalid Transfer-Encoding header");
    }
    content_length_for_framing(headers).map(|_| ())
}

/// Whether exactly one `Content-Length` token is present: a single header line
/// with no comma-separated list.
///
/// A recipient may choose to tolerate such a value when it is otherwise
/// unparseable (treating the message as close-delimited), but multi-token
/// framing (duplicate header lines or comma-separated lists) that fails to
/// reconcile is always an unrecoverable error and must never be tolerated.
pub(crate) fn content_length_is_single_token(headers: &HMap) -> bool {
    let mut values = headers.get_all(header::CONTENT_LENGTH).into_iter();
    match (values.next(), values.next()) {
        (Some(value), None) => !value.as_bytes().contains(&b','),
        _ => false,
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use http::{
        header::{CONTENT_LENGTH, TRANSFER_ENCODING},
        StatusCode, Version,
    };
    use rstest::rstest;

    #[test]
    fn test_validate_content_length() {
        let mut headers = HMap::new();

        // no CL header is fine
        assert!(validate_content_length(&headers).is_ok());

        // a single valid CL is fine
        headers.append(CONTENT_LENGTH, "1".try_into().unwrap());
        assert!(validate_content_length(&headers).is_ok());

        // an identical duplicate is reconciled to a single value (RFC 9110 8.6)
        headers.append(CONTENT_LENGTH, "1".try_into().unwrap());
        assert!(validate_content_length(&headers).is_ok());

        // a differing duplicate is an unrecoverable framing error
        headers.append(CONTENT_LENGTH, "2".try_into().unwrap());
        assert!(validate_content_length(&headers).is_err());

        // Transfer-Encoding overrides Content-Length, so CL is ignored
        headers.append(TRANSFER_ENCODING, "chunked".try_into().unwrap());
        assert!(validate_content_length(&headers).is_ok());
    }

    #[test]
    fn test_validate_content_length_invalid_value() {
        let mut headers = HMap::new();
        headers.insert(CONTENT_LENGTH, "not-a-number".try_into().unwrap());
        assert!(validate_content_length(&headers).is_err());
    }

    #[test]
    fn test_validate_content_length_without_transfer_encoding_rejects_te() {
        let mut headers = HMap::new();
        headers.append(TRANSFER_ENCODING, "chunked".try_into().unwrap());

        let err = validate_content_length_without_transfer_encoding(&headers).unwrap_err();
        assert_eq!(err.etype(), &InvalidHTTPHeader);
        assert_eq!(
            err.context.as_ref().map(|c| c.as_str()),
            Some("invalid Transfer-Encoding header")
        );
    }

    #[rstest]
    #[case::single("5", Some(5))]
    #[case::comma_identical("5, 5", Some(5))]
    #[case::comma_identical_no_space("5,5", Some(5))]
    #[case::comma_conflict("5, 6", None)]
    #[case::invalid("abc", None)]
    #[case::empty("", None)]
    #[case::negative("-1", None)]
    #[case::trailing_comma("5,", None)]
    #[case::overflow("99999999999999999999999999", None)]
    fn test_content_length_parse_all_single_header(
        #[case] value: &str,
        #[case] expected: Option<u64>,
    ) {
        let mut headers = HMap::new();
        headers.insert(CONTENT_LENGTH, value.try_into().unwrap());
        assert_eq!(content_length_parse_all(&headers), expected);
    }

    #[rstest]
    #[case::none(&[], None)]
    #[case::two_identical(&["5", "5"], Some(5))]
    #[case::three_identical(&["7", "7", "7"], Some(7))]
    #[case::two_conflict(&["5", "6"], None)]
    #[case::mixed_comma_identical(&["5", "5, 5"], Some(5))]
    #[case::mixed_comma_conflict(&["5", "5, 6"], None)]
    fn test_content_length_parse_all_multi_header(
        #[case] values: &[&str],
        #[case] expected: Option<u64>,
    ) {
        let mut headers = HMap::new();
        for value in values {
            headers.append(CONTENT_LENGTH, (*value).try_into().unwrap());
        }
        assert_eq!(content_length_parse_all(&headers), expected);
    }

    #[test]
    fn test_content_length_for_framing() {
        // absent -> Ok(None)
        let headers = HMap::new();
        assert_eq!(content_length_for_framing(&headers).unwrap(), None);

        // valid -> Ok(Some)
        let mut headers = HMap::new();
        headers.insert(CONTENT_LENGTH, "5".try_into().unwrap());
        assert_eq!(content_length_for_framing(&headers).unwrap(), Some(5));

        // identical duplicate -> Ok(Some) (reconciled)
        let mut headers = HMap::new();
        headers.append(CONTENT_LENGTH, "5".try_into().unwrap());
        headers.append(CONTENT_LENGTH, "5".try_into().unwrap());
        assert_eq!(content_length_for_framing(&headers).unwrap(), Some(5));

        // present but invalid -> Err
        let mut headers = HMap::new();
        headers.insert(CONTENT_LENGTH, "abc".try_into().unwrap());
        let err = content_length_for_framing(&headers).unwrap_err();
        assert_eq!(
            err.context.as_ref().map(|c| c.as_str()),
            Some("invalid Content-Length header")
        );

        // present but conflicting -> Err
        let mut headers = HMap::new();
        headers.append(CONTENT_LENGTH, "5".try_into().unwrap());
        headers.append(CONTENT_LENGTH, "6".try_into().unwrap());
        let err = content_length_for_framing(&headers).unwrap_err();
        assert_eq!(
            err.context.as_ref().map(|c| c.as_str()),
            Some("invalid Content-Length header")
        );
    }

    #[test]
    fn test_content_length_is_single_token_absent() {
        // no Content-Length header at all is not a single token
        assert!(!content_length_is_single_token(&HMap::new()));
    }

    #[rstest]
    // single header line, no comma -> single token (even if otherwise invalid)
    #[case::valid("5", true)]
    #[case::invalid("abc", true)]
    #[case::empty("", true)]
    // a comma-separated list in one header line is multiple tokens
    #[case::comma_identical("5, 5", false)]
    #[case::comma_conflict("5, 6", false)]
    #[case::trailing_comma("5,", false)]
    fn test_content_length_is_single_token_one_header(#[case] value: &str, #[case] expected: bool) {
        let mut headers = HMap::new();
        headers.insert(CONTENT_LENGTH, value.try_into().unwrap());
        assert_eq!(content_length_is_single_token(&headers), expected);
    }

    #[rstest]
    // duplicate header lines are multiple tokens, regardless of values
    #[case::identical(&["5", "5"])]
    #[case::conflict(&["5", "6"])]
    fn test_content_length_is_single_token_multi_header(#[case] values: &[&str]) {
        let mut headers = HMap::new();
        for value in values {
            headers.append(CONTENT_LENGTH, (*value).try_into().unwrap());
        }
        assert!(!content_length_is_single_token(&headers));
    }

    #[test]
    fn test_is_upgrade_resp() {
        let mut response = ResponseHeader::build(StatusCode::SWITCHING_PROTOCOLS, None).unwrap();
        response.set_version(Version::HTTP_11);
        response.insert_header("Upgrade", "websocket").unwrap();
        response.insert_header("Connection", "upgrade").unwrap();
        assert!(is_upgrade_resp(&response));

        // wrong http version
        response.set_version(Version::HTTP_10);
        response.insert_header("Upgrade", "websocket").unwrap();
        response.insert_header("Connection", "upgrade").unwrap();
        assert!(!is_upgrade_resp(&response));

        // not 101
        response.set_status(StatusCode::OK).unwrap();
        response.set_version(Version::HTTP_11);
        assert!(!is_upgrade_resp(&response));
    }

    #[test]
    fn test_is_chunked_encoding_from_headers_empty() {
        let empty_headers = HMap::new();
        assert!(!is_chunked_encoding_from_headers(&empty_headers));
    }

    #[rstest]
    #[case::single_chunked("chunked", true)]
    #[case::comma_separated_final("identity, chunked", true)]
    #[case::whitespace_around("  chunked  ", true)]
    #[case::empty_elements_before(", , , chunked", true)]
    #[case::only_identity("identity", false)]
    #[case::trailing_comma("chunked, ", false)]
    #[case::multiple_trailing_commas("chunked, , ", false)]
    #[case::empty_value("", false)]
    #[case::whitespace_only("   ", false)]
    fn test_is_chunked_encoding_single_header(#[case] value: &str, #[case] expected: bool) {
        let mut headers = HMap::new();
        headers.insert(TRANSFER_ENCODING, value.try_into().unwrap());
        assert_eq!(is_chunked_encoding_from_headers(&headers), expected);
    }

    #[rstest]
    #[case::two_headers_chunked_last(&["identity", "chunked"], true)]
    #[case::three_headers_chunked_last(&["gzip", "identity", "chunked"], true)]
    #[case::last_has_comma_separated(&["gzip", "identity, chunked"], true)]
    #[case::whitespace_in_last(&["gzip", "  chunked  "], true)]
    #[case::two_headers_no_chunked(&["identity", "gzip"], false)]
    #[case::chunked_not_last(&["chunked", "identity"], false)]
    #[case::last_has_chunked_not_final(&["gzip", "chunked, identity"], false)]
    #[case::chunked_overridden(&["chunked", "identity, gzip"], false)]
    #[case::trailing_comma_in_last(&["gzip", "chunked, "], false)]
    fn test_is_chunked_encoding_multiple_headers(#[case] values: &[&str], #[case] expected: bool) {
        let mut headers = HMap::new();
        for value in values {
            headers.append(TRANSFER_ENCODING, (*value).try_into().unwrap());
        }
        assert_eq!(is_chunked_encoding_from_headers(&headers), expected);
    }
}
