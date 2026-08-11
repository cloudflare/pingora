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

//! HTTP authority consistency checks shared by protocol versions and proxy egress.

use http::{header, HeaderValue};
use pingora_error::{Error, ErrorType::InvalidHTTPHeader, Result};
use pingora_http::RequestHeader;

/// Validate that a request has one unambiguous authority.
///
/// CONNECT handling:
///
/// - A valid authority with a port may omit `Host`; a present `Host` must match.
/// - Malformed authority prefixes require a byte-exact `Host` match.
/// - Other nonstandard forms remain application-defined.
/// - Userinfo and ambiguous bracket or port forms are rejected.
///
/// Other requests reject duplicate `Host` ([RFC 9112 section 3.2]), userinfo
/// ([RFC 9110 section 4.2.4]), inconsistent absolute-form authority, and targets whose authority
/// changes under slash or backslash normalization. This is stricter than
/// [RFC 9112 section 3.2.2], which replaces conflicting `Host`. Userinfo is unsafe because
/// [`http::Uri::host`] strips it.
///
/// Authority bytes otherwise remain opaque. HTTP/1 ingress and standard proxy egress call this;
/// HTTP/2 performs equivalent stream-local checks.
///
/// [RFC 9110 section 4.2.4]: https://www.rfc-editor.org/rfc/rfc9110.html#section-4.2.4
/// [RFC 9112 section 3.2]: https://www.rfc-editor.org/rfc/rfc9112.html#section-3.2
/// [RFC 9112 section 3.2.2]: https://www.rfc-editor.org/rfc/rfc9112.html#section-3.2.2
pub fn validate_request_authority(req: &RequestHeader) -> Result<()> {
    validate_request_authority_fields(req)?;

    let host = req.headers.get(header::HOST);

    if req.method == http::Method::CONNECT {
        return validate_connect_authority(req.raw_path(), host);
    }

    match raw_target_authority(req.raw_path()) {
        RawTargetAuthority::None => {}
        RawTargetAuthority::AmbiguousAuthority => {
            return Error::e_explain(
                InvalidHTTPHeader,
                "ambiguous HTTP absolute-form request target",
            );
        }
        RawTargetAuthority::Absolute {
            scheme, authority, ..
        } => {
            if is_http_family_scheme(scheme) {
                http::uri::Authority::try_from(authority).map_err(|cause| {
                    Error::because(
                        InvalidHTTPHeader,
                        "unparseable absolute-form authority",
                        cause,
                    )
                })?;
            }
            if authority.contains(&b'@') {
                return Error::e_explain(InvalidHTTPHeader, "userinfo in request target");
            }
            if host.is_some_and(|host| host.as_bytes() != authority) {
                return Error::e_explain(
                    InvalidHTTPHeader,
                    "Host header differs from request-target authority",
                );
            }
        }
    }

    Ok(())
}

/// Validate authority fields shared by H1 and H2 ingress.
pub(super) fn validate_request_authority_fields(req: &RequestHeader) -> Result<()> {
    let mut hosts = req.headers.get_all(header::HOST).iter();
    let (host, duplicate_host) = (hosts.next(), hosts.next());

    if duplicate_host.is_some() {
        return Error::e_explain(InvalidHTTPHeader, "multiple Host header fields");
    }

    if host.is_some_and(|host| host.as_bytes().contains(&b'@')) {
        return Error::e_explain(InvalidHTTPHeader, "userinfo in Host header");
    }

    let uri_authority = req.uri.authority().map(|authority| authority.as_str());
    if uri_authority.is_some_and(|authority| authority.contains('@')) {
        return Error::e_explain(InvalidHTTPHeader, "userinfo in URI authority");
    }

    if host
        .zip(uri_authority)
        .is_some_and(|(host, authority)| host.as_bytes() != authority.as_bytes())
    {
        return Error::e_explain(InvalidHTTPHeader, "Host header differs from URI authority");
    }

    Ok(())
}

/// Reconcile authority-like CONNECT targets with `Host` while preserving custom target forms.
fn validate_connect_authority(target: &[u8], host: Option<&HeaderValue>) -> Result<()> {
    let authority_end = target
        .iter()
        .position(|byte| matches!(byte, b'/' | b'?' | b'#'))
        .unwrap_or(target.len());
    let authority_target = &target[..authority_end];

    // For ordinary CONNECT this is `host:port`; for absolute-form it is only the scheme (`http:`),
    // so the fallback classifier below must extract and validate the actual authority separately.
    if authority_target.contains(&b'@') {
        return Error::e_explain(InvalidHTTPHeader, "userinfo in request target");
    }

    if has_ambiguous_port_suffix(authority_target) {
        return Error::e_explain(InvalidHTTPHeader, "ambiguous CONNECT request target");
    }

    // Parse ordinary authority-form (`host:port`) and reconcile by components.
    let parsed_authority = http::uri::Authority::try_from(authority_target);
    if let Ok(authority) = &parsed_authority {
        if authority.port().is_some() {
            let Some(host) = host else {
                return Ok(());
            };
            return reconcile_connect_host(authority_target, authority.host().as_bytes(), host);
        }
    }

    // Handle absolute (`http://host/path`), ambiguous (`http:/\host`), and custom (`unix:/path`)
    // forms when ordinary parsing did not return above.
    match raw_target_authority(target) {
        RawTargetAuthority::Absolute { authority, .. } => {
            if authority.contains(&b'@') {
                return Error::e_explain(InvalidHTTPHeader, "userinfo in request target");
            }
            if has_ambiguous_port_suffix(authority) {
                return Error::e_explain(InvalidHTTPHeader, "ambiguous CONNECT request target");
            }
            let Some(host) = host else {
                return Error::e_explain(
                    InvalidHTTPHeader,
                    "missing Host header for absolute-form CONNECT request target",
                );
            };
            // Reparse the authority extracted from the absolute-form target.
            match http::uri::Authority::try_from(authority) {
                Ok(parsed) => reconcile_connect_host(authority, parsed.host().as_bytes(), host),
                Err(_) => reconcile_connect_host(authority, authority, host),
            }
        }
        RawTargetAuthority::AmbiguousAuthority => {
            Error::e_explain(InvalidHTTPHeader, "ambiguous CONNECT request target")
        }
        // An authority prefix that does not parse cannot be reconciled by component, so require it
        // to match `Host` byte-for-byte: otherwise one malformed byte would escape the reconciliation
        // the same target is subject to without it. An empty prefix carries no authority to disagree
        // about.
        RawTargetAuthority::None if !authority_target.is_empty() && parsed_authority.is_err() => {
            let Some(host) = host else {
                return Error::e_explain(
                    InvalidHTTPHeader,
                    "missing Host header for malformed CONNECT request target",
                );
            };
            reconcile_connect_host(authority_target, authority_target, host)
        }
        RawTargetAuthority::None => Ok(()),
    }
}

/// Accept `Host` only when it names the complete CONNECT authority or its host component.
///
/// The host-only form follows the [RFC 9112 section 3.2.3] example:
/// `CONNECT example.com:80` with `Host: example.com`.
///
/// [RFC 9112 section 3.2.3]: https://www.rfc-editor.org/rfc/rfc9112.html#section-3.2.3
fn reconcile_connect_host(
    authority: &[u8],
    authority_host: &[u8],
    host: &HeaderValue,
) -> Result<()> {
    if host.as_bytes() == authority || host.as_bytes() == authority_host {
        Ok(())
    } else {
        Error::e_explain(
            InvalidHTTPHeader,
            "Host header differs from CONNECT request target",
        )
    }
}

/// Detect multiple unbracketed port separators, whose interpretation differs between parsers.
pub(super) fn has_ambiguous_port_suffix(authority: &[u8]) -> bool {
    let mut inside_brackets = authority.first() == Some(&b'[');
    let mut suffix_colon_seen = false;

    for (index, &byte) in authority.iter().enumerate() {
        if inside_brackets {
            if index == 0 {
                continue;
            }
            match byte {
                b'[' => return true,
                b']' => inside_brackets = false,
                _ => {}
            }
            continue;
        }

        match byte {
            b'[' | b']' => return true,
            b':' if suffix_colon_seen => return true,
            b':' => suffix_colon_seen = true,
            _ => {}
        }
    }

    inside_brackets
}

/// Raw request-target authority boundaries.
///
/// Used for H1 absolute-form and malformed H2 `:path`; authority bytes remain opaque.
#[derive(Debug, PartialEq, Eq)]
pub enum RawTargetAuthority<'a> {
    /// The target does not carry an authority in absolute-form.
    None,
    /// The target carries an absolute-form authority and the following path/query bytes.
    Absolute {
        /// The raw scheme bytes, excluding `:`.
        scheme: &'a [u8],
        /// The raw authority bytes, excluding the leading `//`.
        authority: &'a [u8],
        /// The path and query after the authority, or an empty slice when absent.
        path_and_query: &'a [u8],
    },
    /// Parser normalization could produce a different authority.
    ///
    /// Examples: `http:/\host` and `http://host\other/`
    AmbiguousAuthority,
}

impl<'a> RawTargetAuthority<'a> {
    /// Return the absolute-form authority, if this classification carries one.
    pub fn authority(&self) -> Option<&'a [u8]> {
        match self {
            Self::None | Self::AmbiguousAuthority => None,
            Self::Absolute { authority, .. } => Some(authority),
        }
    }
}

/// Classify authority and path/query boundaries in a raw request target.
pub fn raw_target_authority(target: &[u8]) -> RawTargetAuthority<'_> {
    // Origin form, the common case, cannot carry an authority.
    if target.first() == Some(&b'/') {
        return RawTargetAuthority::None;
    }

    // Phase 1: validate the scheme while locating its terminating colon.
    // RFC 3986 section 3.1: scheme = ALPHA *( ALPHA / DIGIT / "+" / "-" / "." ).
    // https://www.rfc-editor.org/rfc/rfc3986.html#section-3.1
    let mut valid_scheme = true;
    let mut scheme_end = None;
    for (index, &byte) in target.iter().enumerate() {
        if byte == b':' {
            scheme_end = Some(index);
            break;
        }
        valid_scheme &= if index == 0 {
            byte.is_ascii_alphabetic()
        } else {
            byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'-' | b'.')
        };
    }
    let Some(scheme_end) = scheme_end else {
        return RawTargetAuthority::None;
    };
    let scheme = &target[..scheme_end];
    let remainder = &target[scheme_end + 1..];

    let authority = remainder.strip_prefix(b"//");

    if scheme.is_empty() || !valid_scheme {
        // `://host` and `ht_tp://host` are ambiguous because `//` can make permissive parsers
        // derive an authority; `:opaque` and `ht_tp:opaque` carry no authority marker.
        return if authority.is_some() {
            RawTargetAuthority::AmbiguousAuthority
        } else {
            RawTargetAuthority::None
        };
    }

    let http_family_scheme = is_http_family_scheme(scheme);
    let Some(authority) = authority else {
        // HTTP/WebSocket schemes without `//` can gain authority during normalization
        // (`http:host`); custom forms remain application-defined (`myproto:opaque`).
        return if http_family_scheme {
            RawTargetAuthority::AmbiguousAuthority
        } else {
            RawTargetAuthority::None
        };
    };

    // Phase 2: locate the authority boundary and special-scheme backslashes in one scan.
    let mut end = authority.len();
    for (index, &byte) in authority.iter().enumerate() {
        if matches!(byte, b'/' | b'?' | b'#') {
            end = index;
            break;
        }
        // HTTP/WebSocket URL parsers treat `\` as `/`, which can change the authority boundary.
        if http_family_scheme && byte == b'\\' {
            return RawTargetAuthority::AmbiguousAuthority;
        }
    }
    if http_family_scheme && end == 0 {
        return RawTargetAuthority::AmbiguousAuthority;
    }
    RawTargetAuthority::Absolute {
        scheme,
        authority: &authority[..end],
        path_and_query: &authority[end..],
    }
}

fn is_http_family_scheme(scheme: &[u8]) -> bool {
    [b"http".as_slice(), b"https", b"ws", b"wss"]
        .iter()
        .any(|candidate| scheme.eq_ignore_ascii_case(candidate))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(method: &str, target: &str, hosts: &[&str]) -> RequestHeader {
        let mut request = RequestHeader::build(method, target.as_bytes(), None).unwrap();
        for host in hosts {
            request.append_header(header::HOST, *host).unwrap();
        }
        request
    }

    #[test]
    fn validate_authority_sources() {
        assert!(validate_request_authority(&request("GET", "/test", &[])).is_ok());
        assert!(
            validate_request_authority(&request("GET", "/test", &["authority.example:8443"]))
                .is_ok()
        );
        assert!(validate_request_authority(&request(
            "GET",
            "/test",
            &["authority.example", "other.example"]
        ))
        .is_err());
        assert!(validate_request_authority(&request(
            "GET",
            "/test",
            &["authority.example", "authority.example"]
        ))
        .is_err());
        assert!(
            validate_request_authority(&request("GET", "/test", &["user@authority.example"]))
                .is_err()
        );
        assert!(validate_request_authority(&request(
            "GET",
            "/test",
            &["user:pass@authority.example"]
        ))
        .is_err());
        assert!(validate_request_authority(&request(
            "GET",
            "/test",
            &["user%40authority.example"]
        ))
        .is_ok());

        let uri_request = |uri: &str, host: Option<&str>| {
            let mut request = http::Request::builder().uri(uri).body(()).unwrap();
            if let Some(host) = host {
                request
                    .headers_mut()
                    .insert(header::HOST, HeaderValue::from_str(host).unwrap());
            }
            RequestHeader::from(request.into_parts().0)
        };
        assert!(validate_request_authority(&uri_request(
            "https://user@authority.example/test",
            None
        ))
        .is_err());
        assert!(validate_request_authority(&uri_request(
            "https://authority.example/test",
            Some("other.example")
        ))
        .is_err());
    }

    #[test]
    fn validate_absolute_form() {
        assert!(validate_request_authority(&request(
            "GET",
            "http://authority.example/test",
            &["authority.example"]
        ))
        .is_ok());
        assert!(validate_request_authority(&request(
            "GET",
            "http://user@authority.example/test",
            &["authority.example"]
        ))
        .is_err());
        for authority in [
            "good.example%5Cevil.example",
            "user%40good.example",
            "a%2f.example",
            "good.example:443:8080",
            "[good.example",
        ] {
            let target = format!("http://{authority}/test");
            let err =
                validate_request_authority(&request("GET", &target, &[authority])).unwrap_err();
            assert_eq!(
                err.context.as_ref().map(|context| context.as_str()),
                Some("unparseable absolute-form authority"),
                "{target}"
            );
        }
        for target in ["http:///path", "https://?query"] {
            let err = validate_request_authority(&request("GET", target, &[""])).unwrap_err();
            assert_eq!(
                err.context.as_ref().map(|context| context.as_str()),
                Some("ambiguous HTTP absolute-form request target"),
                "{target}"
            );
        }
        for target in ["ht_tp://other.example/admin", "9x://other.example/admin"] {
            assert!(
                validate_request_authority(&request("GET", target, &["other.example"])).is_err(),
                "{target}"
            );
        }
        let mut hostless = request("GET", "http://authority.example/test", &[]);
        assert!(validate_request_authority(&hostless).is_ok());
        hostless.set_version(http::Version::HTTP_10);
        assert!(validate_request_authority(&hostless).is_ok());
        let err = validate_request_authority(&request(
            "GET",
            "http://authority.example/test",
            &["other.example"],
        ))
        .unwrap_err();
        assert_eq!(
            err.context.as_ref().map(|context| context.as_str()),
            Some("Host header differs from request-target authority")
        );
        let err = validate_request_authority(&request(
            "GET",
            "http:/\\/\\other.example/admin",
            &["authority.example"],
        ))
        .unwrap_err();
        assert_eq!(
            err.context.as_ref().map(|context| context.as_str()),
            Some("ambiguous HTTP absolute-form request target")
        );
        let err = validate_request_authority(&request(
            "GET",
            "http://good.example\\evil.example/",
            &["good.example\\evil.example"],
        ))
        .unwrap_err();
        assert_eq!(
            err.context.as_ref().map(|context| context.as_str()),
            Some("ambiguous HTTP absolute-form request target")
        );
        assert!(validate_request_authority(&request("GET", "/users/foo@bar.example", &[])).is_ok());
        assert!(
            validate_request_authority(&request("GET", "/test?email=foo@bar.example", &[])).is_ok()
        );
        assert!(validate_request_authority(&request(
            "GET",
            "/redirect?next=http://other.example/admin",
            &["authority.example"]
        ))
        .is_ok());
        assert!(validate_request_authority(&request(
            "GET",
            "/test#http://user@evil.example/",
            &["authority.example"]
        ))
        .is_ok());
    }

    #[test]
    fn validate_connect() {
        // Empty authority prefixes carry no authority to reconcile with Host.
        for target in [
            "/",
            "/foo",
            "/ws?arr[]=1",
            "/ws?token=a@b.com",
            "?query",
            "?a=:1:2",
            "#frag",
        ] {
            assert!(
                validate_request_authority(&request("CONNECT", target, &["other.example"])).is_ok(),
                "{target} should remain application-defined"
            );
        }
        assert!(validate_request_authority(&request("CONNECT", "http:443", &["http"])).is_ok());
        assert!(
            validate_request_authority(&request("CONNECT", "[::1]", &["other.example"])).is_ok()
        );
        assert!(validate_request_authority(&request("CONNECT", "[::1]:443", &["[::1]"])).is_ok());
        assert!(
            validate_request_authority(&request("CONNECT", "authority.example:443", &[])).is_ok()
        );
        assert!(validate_request_authority(&request(
            "CONNECT",
            "authority.example:443",
            &["authority.example:443"]
        ))
        .is_ok());
        assert!(validate_request_authority(&request(
            "CONNECT",
            "authority.example:443/path?arr[]=1&port=:1:2",
            &["authority.example:443"]
        ))
        .is_ok());
        assert!(validate_request_authority(&request(
            "CONNECT",
            "unix:/var/run/x.sock",
            &["other.example"]
        ))
        .is_ok());
        for target in [
            "a.example",
            "a.example:",
            "a.example:443x",
            "a.example:65536",
            "myproto:opaque",
        ] {
            assert!(
                validate_request_authority(&request("CONNECT", target, &["other.example"])).is_ok(),
                "{target} should remain application-defined"
            );
        }
        for target in ["a%2f.example:443", "[0:1:2:3:4:5:6:7:8:9]:443"] {
            assert!(
                validate_request_authority(&request("CONNECT", target, &[target])).is_ok(),
                "{target} should be accepted when Host is byte-identical"
            );
        }
        assert!(validate_request_authority(&request(
            "CONNECT",
            "http://a%2f.example:443/",
            &["a%2f.example:443"]
        ))
        .is_ok());
        for target in [
            "authority.example:443",
            "authority.example:443/path",
            "authority.example:443?query",
            "authority.example:443:80",
            "[::1]:443:80",
            "a.example:80x:443",
            "[a.example:443:80",
            "a.example:443[:80]",
            "[::1]:443",
            "::1",
            "a%2f.example:443",
            "[a[b]:443",
            "[0:1:2:3:4:5:6:7:8:9]:443",
            "http://a%2f.example:443/",
            "https://authority.example/",
        ] {
            assert!(
                validate_request_authority(&request("CONNECT", target, &["other.example"]))
                    .is_err(),
                "{target} should be rejected"
            );
        }

        assert!(validate_request_authority(&request(
            "CONNECT",
            "a.example:443:80",
            &["a.example:443:80"]
        ))
        .is_err());
        assert!(
            validate_request_authority(&request("CONNECT", "a[b]:443", &["a[b]:443"])).is_err()
        );
        assert!(
            validate_request_authority(&request("CONNECT", "[a[b]:443", &["[a[b]:443"])).is_err()
        );
        assert!(
            validate_request_authority(&request("CONNECT", "http://a[b]:443/", &["a[b]:443"]))
                .is_err()
        );
        assert!(validate_request_authority(&request(
            "CONNECT",
            "http://user@authority.example:443/",
            &["user@authority.example:443"]
        ))
        .is_err());

        let mut non_utf8 = RequestHeader::build("CONNECT", b"a.example:443\xff#:80", None).unwrap();
        non_utf8.append_header(header::HOST, "a.example").unwrap();
        assert!(validate_request_authority(&non_utf8).is_err());

        let mut non_utf8 = RequestHeader::build("CONNECT", b"a\xff.example:443", None).unwrap();
        non_utf8
            .append_header(header::HOST, "other.example")
            .unwrap();
        assert!(validate_request_authority(&non_utf8).is_err());
    }

    #[test]
    fn classify_raw_target_authority() {
        assert_eq!(
            raw_target_authority(b"http://authority.example/test"),
            RawTargetAuthority::Absolute {
                scheme: b"http",
                authority: b"authority.example",
                path_and_query: b"/test"
            }
        );
        assert_eq!(
            raw_target_authority(b"http://authority.example/test").authority(),
            Some(b"authority.example".as_slice())
        );
        assert_eq!(
            raw_target_authority(b"http://user@authority.example:8443/test?a=b"),
            RawTargetAuthority::Absolute {
                scheme: b"http",
                authority: b"user@authority.example:8443",
                path_and_query: b"/test?a=b"
            }
        );
        assert_eq!(
            raw_target_authority(b"http://authority.example"),
            RawTargetAuthority::Absolute {
                scheme: b"http",
                authority: b"authority.example",
                path_and_query: b""
            }
        );
        assert_eq!(
            raw_target_authority(b"http:/\\/\\authority.example/test"),
            RawTargetAuthority::AmbiguousAuthority
        );
        assert_eq!(
            raw_target_authority(b"https:authority.example/test"),
            RawTargetAuthority::AmbiguousAuthority
        );
        assert_eq!(
            raw_target_authority(b"http://good.example\\evil.example/"),
            RawTargetAuthority::AmbiguousAuthority
        );
        for target in [
            b"ht_tp://other.example/admin".as_slice(),
            b"9x://other.example/admin",
            b"http:///path",
            b"https://?query",
            b"ws://good.example\\evil.example/",
            b"wss://good.example\\evil.example/",
        ] {
            assert_eq!(
                raw_target_authority(target),
                RawTargetAuthority::AmbiguousAuthority,
                "{}",
                String::from_utf8_lossy(target)
            );
        }
        assert_eq!(
            raw_target_authority(b"file:///path"),
            RawTargetAuthority::Absolute {
                scheme: b"file",
                authority: b"",
                path_and_query: b"/path"
            }
        );
        assert_eq!(
            raw_target_authority(b"ftp://good.example\\evil.example/"),
            RawTargetAuthority::Absolute {
                scheme: b"ftp",
                authority: b"good.example\\evil.example",
                path_and_query: b"/"
            }
        );
        assert_eq!(raw_target_authority(b"/test"), RawTargetAuthority::None);
        assert_eq!(raw_target_authority(b":opaque"), RawTargetAuthority::None);
        assert_eq!(
            raw_target_authority(b"/redirect?next=http://user@evil.example/"),
            RawTargetAuthority::None
        );
        assert_eq!(
            raw_target_authority(b"/test#http://user@evil.example/"),
            RawTargetAuthority::None
        );
        assert_eq!(
            raw_target_authority(b"foo:bar://user@example/path"),
            RawTargetAuthority::None
        );
    }
}
