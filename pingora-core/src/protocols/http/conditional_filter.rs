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

//! Conditional filter (not modified) utilities

use http::{header::*, StatusCode};
use httpdate::{parse_http_date, HttpDate};
use pingora_error::{ErrorType::InvalidHTTPHeader, OrErr, Result};
use pingora_http::{RequestHeader, ResponseHeader};

/// Evaluates conditional headers according to the [RFC](https://datatracker.ietf.org/doc/html/rfc9111#name-handling-a-received-validat).
///
/// Returns true if the request should receive 304 Not Modified.
pub fn not_modified_filter(req: &RequestHeader, resp: &ResponseHeader) -> bool {
    // https://datatracker.ietf.org/doc/html/rfc9110#name-304-not-modified
    // 304 can only validate 200
    if resp.status != StatusCode::OK {
        return false;
    }

    // Evulation of conditional headers, based on RFC:
    // https://datatracker.ietf.org/doc/html/rfc9111#name-handling-a-received-validat

    // TODO: If-Match and If-Unmodified-Since, and returning 412 Precondition Failed
    // Note that this function is currently used only for proxy cache,
    // and the current RFCs have some conflicting opinions as to whether
    // If-Match and If-Unmodified-Since can be used. https://github.com/httpwg/http-core/issues/1111

    // Conditional request precedence:
    // https://datatracker.ietf.org/doc/html/rfc9110#name-precedence-of-preconditions
    // If-None-Match should be handled before If-Modified-Since.
    // XXX: In nginx, IMS is actually checked first, which may cause compatibility issues
    // for certain origins/clients.

    if req.headers.contains_key(IF_NONE_MATCH) {
        if let Some(etag) = resp.headers.get(ETAG) {
            for inm in req.headers.get_all(IF_NONE_MATCH) {
                if weak_validate_etag(inm.as_bytes(), etag.as_bytes()) {
                    return true;
                }
            }
        }
        // https://datatracker.ietf.org/doc/html/rfc9110#field.if-modified-since
        // "MUST ignore If-Modified-Since if the request contains an If-None-Match header"
        return false;
    }

    // GET/HEAD only https://datatracker.ietf.org/doc/html/rfc9110#field.if-modified-since
    if matches!(req.method, http::Method::GET | http::Method::HEAD) {
        if let Ok(Some(if_modified_since)) = req_header_as_http_date(req, &IF_MODIFIED_SINCE) {
            if let Ok(Some(last_modified)) = resp_header_as_http_date(resp, &LAST_MODIFIED) {
                if if_modified_since >= last_modified {
                    return true;
                }
            }
        }
    }
    false
}

// Trim ASCII whitespace bytes from the start of the slice.
// This is pretty much copied from the nightly API.
// TODO: use `trim_ascii_start` when it stabilizes https://doc.rust-lang.org/std/primitive.slice.html#method.trim_ascii_start
fn trim_ascii_start(mut bytes: &[u8]) -> &[u8] {
    while let [first, rest @ ..] = bytes {
        if first.is_ascii_whitespace() {
            bytes = rest;
        } else {
            break;
        }
    }
    bytes
}

/// Search for an ETag matching `target_etag` from the input header, using
/// [weak comparison](https://datatracker.ietf.org/doc/html/rfc9110#section-8.8.3.2).
/// Multiple ETags can exist in the header as a comma-separated list.
///
/// Returns true if a matching ETag exists.
pub fn weak_validate_etag(input_etag_header: &[u8], target_etag: &[u8]) -> bool {
    // ETag comparison: https://datatracker.ietf.org/doc/html/rfc9110#section-8.8.3.2
    fn strip_weak_prefix(etag: &[u8]) -> &[u8] {
        // Weak ETags are prefaced with `W/`
        etag.strip_prefix(b"W/").unwrap_or(etag)
    }
    // https://datatracker.ietf.org/doc/html/rfc9110#section-13.1.2 unsafe method only
    if input_etag_header == b"*" {
        return true;
    }

    // The RFC defines ETags here: https://datatracker.ietf.org/doc/html/rfc9110#section-8.8.3
    // The RFC requires ETags to be wrapped in double quotes, though some legacy origins or clients
    // don't adhere to this.
    // Unfortunately by allowing non-quoted etags, parsing becomes a little more complicated.
    //
    // This implementation uses nginx's algorithm for parsing ETags, which can handle both quoted
    // and non-quoted ETags. It essentially does a substring comparison at each comma divider,
    // searching for an exact match of the ETag (optional double quotes included) followed by
    // either EOF or another comma.
    //
    // Clients and upstreams should still ideally adhere to quoted ETags to disambiguate
    // situations where commas are contained within the ETag (allowed by the RFC).
    // XXX: This nginx algorithm will handle matching against ETags with commas correctly, but only
    // if the target ETag is a quoted RFC-compliant ETag.
    //
    // For example, consider an if-none-match header: `"xyzzy,xyz,x,y", "xyzzy"`.
    // If the target ETag is double quoted as mandated by the RFC like `"xyz,x"`, this algorithm
    // will correctly report no matching ETag.
    // But if the target ETag is not double quoted like `xyz,x`, it will "incorrectly" match
    // against the substring after the first comma inside the first quoted ETag.

    // Search for the target at each comma delimiter
    let target_etag = strip_weak_prefix(target_etag);
    let mut remaining = strip_weak_prefix(input_etag_header);
    while let Some(search_slice) = remaining.get(0..target_etag.len()) {
        if search_slice == target_etag {
            remaining = &remaining[target_etag.len()..];
            // check if there's any content after the matched substring
            // skip any whitespace
            remaining = trim_ascii_start(remaining);
            if matches!(remaining.first(), None | Some(b',')) {
                // we are either at the end of the header, or at a comma delimiter
                // which means this is a match
                return true;
            }
        }
        // find the next delimiter (ignore any remaining part of the non-matching etag)
        let Some(next_delimiter_pos) = remaining.iter().position(|&b| b == b',') else {
            break;
        };
        remaining = &remaining[next_delimiter_pos..];
        // find the next etag slice to compare
        // ignore extraneous delimiters and whitespace
        let Some(next_etag_pos) = remaining
            .iter()
            .position(|&b| !b.is_ascii_whitespace() && b != b',')
        else {
            break;
        };
        remaining = &remaining[next_etag_pos..];

        remaining = strip_weak_prefix(remaining);
    }
    // remaining length < target etag length
    false
}

/// Utility function to parse an HTTP request header as an [HTTP-date](https://datatracker.ietf.org/doc/html/rfc9110#name-date-time-formats).
pub fn req_header_as_http_date<H>(req: &RequestHeader, header_name: H) -> Result<Option<HttpDate>>
where
    H: AsHeaderName,
{
    let Some(header_value) = req.headers.get(header_name) else {
        return Ok(None);
    };
    Ok(Some(parse_bytes_as_http_date(header_value.as_bytes())?))
}

/// Utility function to parse an HTTP response header as an [HTTP-date](https://datatracker.ietf.org/doc/html/rfc9110#name-date-time-formats).
pub fn resp_header_as_http_date<H>(
    resp: &ResponseHeader,
    header_name: H,
) -> Result<Option<HttpDate>>
where
    H: AsHeaderName,
{
    let Some(header_value) = resp.headers.get(header_name) else {
        return Ok(None);
    };
    Ok(Some(parse_bytes_as_http_date(header_value.as_bytes())?))
}

fn parse_bytes_as_http_date(bytes: &[u8]) -> Result<HttpDate> {
    let input_time = std::str::from_utf8(bytes).explain_err(InvalidHTTPHeader, |_| {
        "HTTP date has unsupported characters (bytes outside of UTF-8)"
    })?;
    Ok(parse_http_date(input_time)
        .or_err(InvalidHTTPHeader, "Invalid HTTP date")?
        .into())
}

/// Utility function to convert the input response header to a 304 Not Modified response.
pub fn to_304(resp: &mut ResponseHeader) {
    // https://datatracker.ietf.org/doc/html/rfc9110#name-304-not-modified
    // XXX: https://datatracker.ietf.org/doc/html/rfc9110#name-content-length
    // "A server may send content-length in 304", but no common web server does it
    // So we drop both content-length and content-type for consistency/less surprise
    resp.set_status(StatusCode::NOT_MODIFIED).unwrap();
    resp.remove_header(&CONTENT_LENGTH);
    resp.remove_header(&CONTENT_TYPE);
    // https://datatracker.ietf.org/doc/html/rfc9110#section-15.4.5-4
    // "SHOULD NOT generate representation metadata other than the above listed fields
    // unless said metadata exists for the purpose of guiding cache updates"
    // Remove some more representation metadata headers
    resp.remove_header(&TRANSFER_ENCODING);
    // note that the following are also stripped by nginx
    resp.remove_header(&CONTENT_ENCODING);
    resp.remove_header(&ACCEPT_RANGES);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_if_modified_since() {
        fn build_req(if_modified_since: &[u8]) -> RequestHeader {
            let mut req = RequestHeader::build("GET", b"/", None).unwrap();
            req.insert_header("If-Modified-Since", if_modified_since)
                .unwrap();
            req
        }

        fn build_resp(last_modified: &[u8]) -> ResponseHeader {
            let mut resp = ResponseHeader::build(200, None).unwrap();
            resp.insert_header("Last-Modified", last_modified).unwrap();
            resp
        }

        // same date
        let last_modified = b"Fri, 26 Mar 2010 00:05:00 GMT";
        let req = build_req(b"Fri, 26 Mar 2010 00:05:00 GMT");
        let resp = build_resp(last_modified);
        assert!(not_modified_filter(&req, &resp));

        // before
        let req = build_req(b"Fri, 26 Mar 2010 00:03:00 GMT");
        let resp = build_resp(last_modified);
        assert!(!not_modified_filter(&req, &resp));

        // after
        let req = build_req(b"Sun, 28 Mar 2010 01:07:00 GMT");
        let resp = build_resp(last_modified);
        assert!(not_modified_filter(&req, &resp));
    }

    #[test]
    fn test_weak_validate_etag() {
        let target_weak_etag = br#"W/"xyzzy""#;
        let target_etag = br#""xyzzy""#;
        assert!(weak_validate_etag(b"*", target_weak_etag));
        assert!(weak_validate_etag(b"*", target_etag));

        assert!(weak_validate_etag(target_etag, target_etag));
        assert!(weak_validate_etag(target_etag, target_weak_etag));
        assert!(weak_validate_etag(target_weak_etag, target_etag));
        assert!(weak_validate_etag(target_weak_etag, target_weak_etag));

        let mismatch_weak_etag = br#"W/"abc""#;
        let mismatch_etag = br#""abc""#;
        assert!(!weak_validate_etag(mismatch_etag, target_etag));
        assert!(!weak_validate_etag(mismatch_etag, target_weak_etag));
        assert!(!weak_validate_etag(mismatch_weak_etag, target_etag));
        assert!(!weak_validate_etag(mismatch_weak_etag, target_weak_etag));

        let multiple_etags = br#"a, "xyzzy","r2d2xxxx", "c3piozzzz",zzzfoo"#;
        assert!(weak_validate_etag(multiple_etags, target_etag));
        assert!(weak_validate_etag(multiple_etags, target_weak_etag));

        let multiple_mismatch_etags = br#"foobar", "r2d2xxxx", "c3piozzzz",zzzfoo"#;
        assert!(!weak_validate_etag(multiple_mismatch_etags, target_etag));
        assert!(!weak_validate_etag(
            multiple_mismatch_etags,
            target_weak_etag
        ));

        let multiple_mismatch_etags =
            br#"foobar", "r2d2xxxxyzzy", "c3piozzzz",zzzfoo, "xyzzy,xyzzy""#;
        assert!(!weak_validate_etag(multiple_mismatch_etags, target_etag));
        assert!(!weak_validate_etag(
            multiple_mismatch_etags,
            target_weak_etag
        ));

        let target_comma_etag = br#"",,,""#;
        let multiple_mismatch_etags = br#",", ",,,,", ,,,,,,,,",,",",,,,,,""#;
        assert!(!weak_validate_etag(
            multiple_mismatch_etags,
            target_comma_etag
        ));
        let multiple_etags = br#",", ",,,,", ,,,,,,,,",,,",",,,,,,""#;
        assert!(weak_validate_etag(multiple_etags, target_comma_etag));
    }

    #[test]
    fn test_weak_validate_etag_unquoted() {
        // legacy unquoted etag
        let target_unquoted = b"xyzzy";
        assert!(weak_validate_etag(b"*", target_unquoted));

        let strong_etag = br#""xyzzy""#;
        assert!(!weak_validate_etag(strong_etag, target_unquoted));
        assert!(!weak_validate_etag(target_unquoted, strong_etag));

        let multiple_etags = br#"a, "r2d2xxxx", "c3piozzzz",   xyzzy"#;
        assert!(weak_validate_etag(multiple_etags, target_unquoted));

        let multiple_mismatch_etags =
            br#"foobar", "r2d2xxxxyzzy", "c3piozzzz",zzzfoo, "xyzzy,xyzzy""#;
        assert!(!weak_validate_etag(
            multiple_mismatch_etags,
            target_unquoted
        ));

        // in certain edge cases where commas are used alongside quoted ETags,
        // the test can fail if target is unquoted (the last ETag is intended to be one ETag)
        let multiple_mismatch_etags =
            br#"foobar", "r2d2xxxxyzzy", "c3piozzzz",zzzfoo, "xyzzy,xyzzy,xy""#;
        assert!(weak_validate_etag(multiple_mismatch_etags, target_unquoted));
    }
}
