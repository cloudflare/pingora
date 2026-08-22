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

//! Request-target authority classification.
//!
//! Locates the authority and path boundaries within a raw request-target, without
//! interpreting them. Classification is kept separate from the policy that acts on it so
//! that the URI stored on a [`RequestHeader`](crate::RequestHeader) and the authority a
//! protocol implementation reconciles against `Host` derive from the same boundaries and
//! cannot disagree about where the authority ends.
//!
//! These functions read raw bytes and never allocate.

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

/// Check whether a scheme is HTTP, HTTPS, WebSocket, or secure WebSocket.
///
/// These schemes receive special parsing rules that differ from opaque schemes:
/// normalization can introduce an authority where none appears in the request-target
/// (e.g. `http:host` becoming `http://host`), and backslash is treated as a path
/// separator.
pub fn is_http_family_scheme(scheme: &[u8]) -> bool {
    [b"http".as_slice(), b"https", b"ws", b"wss"]
        .iter()
        .any(|candidate| scheme.eq_ignore_ascii_case(candidate))
}

#[cfg(test)]
mod tests {
    use super::*;

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
