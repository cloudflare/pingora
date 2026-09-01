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

use http::header::{self, HeaderName};
use pingora_core::upstreams::peer::{H1UpgradePolicy, HttpUpstreamRequestPolicy};
use pingora_error::{Error, ErrorType::InvalidHTTPHeader, Result};
use pingora_http::RequestHeader;

const MAX_CONNECTION_NOMINATIONS: usize = 10;
pub(crate) const KEEP_ALIVE: &str = "keep-alive";
pub(crate) const PROXY_CONNECTION: &str = "proxy-connection";
pub(crate) const HTTP2_SETTINGS: &str = "http2-settings";

/// Whether `byte` is a `tchar`, the character set of an HTTP `token` (RFC 9110 §5.6.2). Checked
/// here because `HeaderName::from_bytes` may accept bytes outside the `token` set.
fn is_tchar(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        || matches!(
            byte,
            b'!' | b'#'
                | b'$'
                | b'%'
                | b'&'
                | b'\''
                | b'*'
                | b'+'
                | b'-'
                | b'.'
                | b'^'
                | b'_'
                | b'`'
                | b'|'
                | b'~'
        )
}

fn is_websocket_upgrade_request(req: &RequestHeader, downstream_is_http11: bool) -> bool {
    downstream_is_http11
        && req
            .headers
            .get(header::UPGRADE)
            .is_some_and(|value| value.as_bytes().eq_ignore_ascii_case(b"websocket"))
}

struct ConnectionNominations {
    headers: [Option<HeaderName>; MAX_CONNECTION_NOMINATIONS],
    len: usize,
}

impl ConnectionNominations {
    fn parse(req: &RequestHeader, reject_malformed: bool) -> Result<Self> {
        let mut headers = std::array::from_fn(|_| None);
        let mut len = 0;
        let mut nomination_count = 0;

        // This is inspired by Envoy's defensive Connection-header sanitization checks. Bound the
        // amount of token processing so it cannot become a request-time DoS vector.
        for token in req
            .headers
            .get_all(header::CONNECTION)
            .iter()
            .flat_map(|value| value.as_bytes().split(|byte| *byte == b','))
            .map(|token| token.trim_ascii())
            .filter(|token| !token.is_empty())
        {
            nomination_count += 1;
            if nomination_count >= MAX_CONNECTION_NOMINATIONS {
                return Error::e_explain(
                    InvalidHTTPHeader,
                    "too many Connection header nominations",
                );
            }

            // `:`-prefixed tokens nominate pseudo-headers (e.g. `:authority`); rejected in both modes.
            if token.starts_with(b":") {
                return Error::e_explain(
                    InvalidHTTPHeader,
                    "protected header cannot be nominated by the Connection header",
                );
            }

            // A nomination is an HTTP `token` (RFC 9110 §5.6.2). We validate that ourselves rather
            // than trust `HeaderName::from_bytes`, which may accept non-`token` bytes and let a
            // decorated spelling like `Connection: "X-Forwarded-For"` slip past the protected-name
            // check below. The RFC lets a recipient reject or ignore a malformed option, so
            // `reject_malformed` is a policy choice: fail closed (default) or tolerate it.
            if reject_malformed && !token.iter().all(|&byte| is_tchar(byte)) {
                return Error::e_explain(
                    InvalidHTTPHeader,
                    "invalid token nominated by the Connection header",
                );
            }

            // `HeaderName` lowercases, so the protected-set check below cannot be evaded via casing.
            let name = match HeaderName::from_bytes(token) {
                Ok(name) => name,
                // Strict mode: every token is valid `tchar` and parses; a residual failure (e.g. a
                // length limit) still fails closed. Lenient mode: an unparsable token names
                // nothing, so ignore it.
                Err(_) if reject_malformed => {
                    return Error::e_explain(
                        InvalidHTTPHeader,
                        "invalid token nominated by the Connection header",
                    );
                }
                Err(_) => continue,
            };

            if matches!(
                name.as_str(),
                "host" | "x-forwarded-for" | "x-forwarded-host" | "x-forwarded-proto"
            ) {
                return Error::e_explain(
                    InvalidHTTPHeader,
                    "protected header cannot be nominated by the Connection header",
                );
            }

            headers[len] = Some(name);
            len += 1;
        }

        Ok(Self { headers, len })
    }

    fn remove_from(self, req: &mut RequestHeader) {
        for name in self.headers.into_iter().take(self.len).flatten() {
            req.remove_header(&name);
        }
    }
}

fn strip_standard_hop_by_hop_headers(req: &mut RequestHeader) {
    req.remove_header(KEEP_ALIVE);
    req.remove_header(PROXY_CONNECTION);
    req.remove_header(&header::PROXY_AUTHENTICATE);
    req.remove_header(&header::PROXY_AUTHORIZATION);
    req.remove_header(&header::TE);
    req.remove_header(&header::TRAILER);
    req.remove_header(&header::TRANSFER_ENCODING);
    req.remove_header(&header::CONNECTION);
    req.remove_header(&header::UPGRADE);
    req.remove_header(HTTP2_SETTINGS);
}

/// Apply automatic request policy before application upstream request filtering.
pub(crate) fn sanitize_h1_upstream_request(
    req: &mut RequestHeader,
    policy: HttpUpstreamRequestPolicy,
    downstream_is_http11: bool,
) -> Result<()> {
    if policy == HttpUpstreamRequestPolicy::preserve() {
        return Ok(());
    }

    let nominations = policy
        .strip_connection_nominated
        .then(|| ConnectionNominations::parse(req, policy.reject_malformed_connection_nominations))
        .transpose()?;

    if policy.h1_upgrade == H1UpgradePolicy::Preserve && req.headers.contains_key(header::UPGRADE) {
        // An arbitrary upgrade may require any of the connection-nominated fields. Preserve the
        // complete request metadata rather than forwarding a partial handshake.
        return Ok(());
    }

    let websocket_upgrade = policy.h1_upgrade == H1UpgradePolicy::WebSocketOnly
        && is_websocket_upgrade_request(req, downstream_is_http11);
    if let Some(nominations) = nominations {
        nominations.remove_from(req);
    }

    if policy.strip_hop_by_hop {
        strip_standard_hop_by_hop_headers(req);
    }

    match policy.h1_upgrade {
        H1UpgradePolicy::WebSocketOnly => {
            req.remove_header(&header::CONNECTION);
            req.remove_header(&header::UPGRADE);
            req.remove_header(HTTP2_SETTINGS);
            if websocket_upgrade {
                req.insert_header(header::CONNECTION, "Upgrade")?;
                req.insert_header(header::UPGRADE, "websocket")?;
            }
        }
        H1UpgradePolicy::Deny => {
            req.remove_header(&header::CONNECTION);
            req.remove_header(&header::UPGRADE);
            req.remove_header(HTTP2_SETTINGS);
        }
        H1UpgradePolicy::Preserve => {}
    }

    Ok(())
}

/// Frame a body-bearing HTTP/1 upstream request after application request filtering.
pub(crate) fn finalize_h1_upstream_request_framing(
    req: &mut RequestHeader,
    downstream_has_body: bool,
) -> Result<()> {
    if downstream_has_body
        && req.headers.get(header::CONTENT_LENGTH).is_none()
        && req.headers.get(header::TRANSFER_ENCODING).is_none()
    {
        req.insert_header(header::TRANSFER_ENCODING, "chunked")?;
    }
    Ok(())
}

/// Remove downstream connection-nominated fields before an HTTP/2 conversion.
pub(crate) fn sanitize_h2_upstream_request(
    req: &mut RequestHeader,
    policy: HttpUpstreamRequestPolicy,
) -> Result<()> {
    if policy.strip_connection_nominated {
        ConnectionNominations::parse(req, policy.reject_malformed_connection_nominations)?
            .remove_from(req);
    }
    if policy.strip_hop_by_hop {
        strip_standard_hop_by_hop_headers(req);
    }
    Ok(())
}

/// Possible downstream states during request multiplexing
#[derive(Debug, Clone, Copy)]
pub(crate) enum DownstreamStateMachine {
    /// more request (body) to read
    Reading,
    /// no more data to read
    ReadingFinished,
    /// downstream is already errored or closed
    Errored,
}

#[allow(clippy::wrong_self_convention)]
impl DownstreamStateMachine {
    pub fn new(finished: bool) -> Self {
        if finished {
            Self::ReadingFinished
        } else {
            Self::Reading
        }
    }

    // Can call read() to read more data or wait on closing
    pub fn can_poll(&self) -> bool {
        !matches!(self, Self::Errored)
    }

    pub fn is_reading(&self) -> bool {
        matches!(self, Self::Reading)
    }

    pub fn is_done(&self) -> bool {
        !matches!(self, Self::Reading)
    }

    pub fn is_errored(&self) -> bool {
        matches!(self, Self::Errored)
    }

    /// Move the state machine to Finished state if `set` is true.
    ///
    /// No-op when the current state is [`Errored`](Self::Errored) — once errored the
    /// downstream connection must not be reused, and late upstream chunks arriving
    /// via `rx.recv()` must not overwrite that decision.
    pub fn maybe_finished(&mut self, set: bool) {
        if set && !self.is_errored() {
            *self = Self::ReadingFinished
        }
    }

    /// Reset to [`Reading`](Self::Reading) for upgraded connections when body mode changes.
    ///
    /// No-op when the current state is [`Errored`](Self::Errored).
    pub fn reset(&mut self) {
        if !self.is_errored() {
            *self = Self::Reading;
        }
    }

    /// Transition to [`Errored`](Self::Errored). This is a terminal state: once entered,
    /// no other state transition is permitted and the connection must not be reused.
    pub fn to_errored(&mut self) {
        *self = Self::Errored
    }
}

/// Possible upstream states during request multiplexing
#[derive(Debug, Clone, Copy)]
pub(crate) struct ResponseStateMachine {
    upstream_response_done: bool,
    cached_response_done: bool,
}

impl ResponseStateMachine {
    pub fn new() -> Self {
        ResponseStateMachine {
            upstream_response_done: false,
            cached_response_done: true, // no cached response by default
        }
    }

    pub fn is_done(&self) -> bool {
        self.upstream_response_done && self.cached_response_done
    }

    pub fn upstream_done(&self) -> bool {
        self.upstream_response_done
    }

    pub fn cached_done(&self) -> bool {
        self.cached_response_done
    }

    pub fn enable_cached_response(&mut self) {
        self.cached_response_done = false;
    }

    pub fn maybe_set_upstream_done(&mut self, done: bool) {
        if done {
            self.upstream_response_done = true;
        }
    }

    pub fn maybe_set_cache_done(&mut self, done: bool) {
        if done {
            self.cached_response_done = true;
        }
    }
}

/// Shared signal from the downstream proxy half to the upstream half: set to
/// [`DownstreamComplete`](Self::DownstreamComplete) right before the downstream
/// half returns successfully, so the upstream half can tell an expected pipe
/// closure (downstream finished the response by its own framing) from an
/// unexpected one.
///
/// Stored in an `AtomicU8` shared via `Arc`: the downstream half stores with
/// [`Release`](std::sync::atomic::Ordering::Release) and the upstream half loads
/// with [`Acquire`](std::sync::atomic::Ordering::Acquire), comparing against
/// `PipeState::DownstreamComplete as u8`.
#[derive(Debug)]
#[repr(u8)]
pub(crate) enum PipeState {
    Active = 0,
    DownstreamComplete = 1,
}

impl PipeState {
    /// Whether `raw` — a value previously read from the shared `AtomicU8` — is
    /// [`DownstreamComplete`](Self::DownstreamComplete). Centralizes the `as u8`
    /// comparison the upstream halves perform on a task-pipe closure.
    pub(crate) fn is_downstream_complete(raw: u8) -> bool {
        raw == PipeState::DownstreamComplete as u8
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request_with_headers(headers: &[(&str, &str)]) -> RequestHeader {
        let mut request = RequestHeader::build("GET", b"/", Some(headers.len())).unwrap();
        request.set_version(http::Version::HTTP_11);
        for (name, value) in headers {
            request
                .append_header(
                    HeaderName::from_bytes(name.as_bytes()).unwrap(),
                    http::HeaderValue::from_str(value).unwrap(),
                )
                .unwrap();
        }
        request
    }

    #[test]
    fn h2_upstream_removes_connection_nominated_fields_by_default() {
        let mut request = request_with_headers(&[
            ("Connection", "X-Private-Hop, HTTP2-Settings"),
            ("X-Private-Hop", "secret"),
            ("HTTP2-Settings", "settings"),
            ("Proxy-Authorization", "secret"),
            ("TE", "trailers"),
            ("Trailer", "X-Trailer"),
        ]);

        sanitize_h2_upstream_request(&mut request, HttpUpstreamRequestPolicy::standard()).unwrap();

        assert!(request.headers.get("x-private-hop").is_none());
        assert!(request.headers.get("http2-settings").is_none());
        assert!(request.headers.get("proxy-authorization").is_none());
        assert!(request.headers.get("te").is_none());
        assert!(request.headers.get("trailer").is_none());
    }

    #[test]
    fn h2_upstream_can_retain_connection_nominated_fields() {
        let mut request =
            request_with_headers(&[("Connection", "X-Private-Hop"), ("X-Private-Hop", "secret")]);
        let mut policy = HttpUpstreamRequestPolicy::standard();
        policy.strip_connection_nominated = false;

        sanitize_h2_upstream_request(&mut request, policy).unwrap();

        assert_eq!(request.headers["x-private-hop"], "secret");
    }

    #[test]
    fn h2_upstream_removes_nominations_after_connection_self_nomination() {
        let mut request = request_with_headers(&[
            ("Connection", "Connection, X-Private-Hop"),
            ("X-Private-Hop", "secret"),
        ]);

        sanitize_h2_upstream_request(&mut request, HttpUpstreamRequestPolicy::standard()).unwrap();

        assert!(request.headers.get("connection").is_none());
        assert!(request.headers.get("x-private-hop").is_none());
    }

    #[test]
    fn h2_upstream_rejects_excessive_unparseable_connection_nominations() {
        let mut request = request_with_headers(&[("Connection", "@, @, @, @, @, @, @, @, @, @")]);

        assert!(
            sanitize_h2_upstream_request(&mut request, HttpUpstreamRequestPolicy::standard())
                .is_err()
        );
    }

    #[test]
    fn connection_nomination_rejects_protected_header() {
        for token in [
            "Host",
            "x-forwarded-for",
            "X-Forwarded-For",
            "X-FORWARDED-HOST",
            "x-Forwarded-Proto",
        ] {
            let mut request = request_with_headers(&[("Connection", token)]);
            assert!(
                sanitize_h2_upstream_request(&mut request, HttpUpstreamRequestPolicy::standard())
                    .is_err(),
                "protected nomination should be rejected regardless of casing: {token:?}"
            );
        }
    }

    #[test]
    fn connection_nomination_rejects_pseudo_header() {
        for token in [":authority", ":method", ":path"] {
            let mut request = request_with_headers(&[("Connection", token)]);
            assert!(
                sanitize_h2_upstream_request(&mut request, HttpUpstreamRequestPolicy::standard())
                    .is_err(),
                "pseudo-header nomination should be rejected: {token:?}"
            );
        }
    }

    /// A nomination that is not a valid `token` is rejected outright instead of silently dropped.
    #[test]
    fn connection_nomination_rejects_malformed_token() {
        let mut request = request_with_headers(&[("Connection", "keep-alive, bad token")]);
        assert!(
            sanitize_h2_upstream_request(&mut request, HttpUpstreamRequestPolicy::standard())
                .is_err()
        );
    }

    /// A protected name decorated with any non-`token` byte is rejected, independent of how
    /// permissive the header-name parser is.
    #[test]
    fn connection_nomination_rejects_decorated_protected_header() {
        for token in [
            "\"X-Forwarded-For\"",
            "(X-Forwarded-For",
            "X-Forwarded-For)",
            "X-Forwarded-For/",
            "X-Forwarded-For:",
            "X -Forwarded-For",
            "@X-Forwarded-For",
        ] {
            let mut request =
                request_with_headers(&[("Connection", token), ("X-Forwarded-For", "6.6.6.6")]);
            assert!(
                sanitize_h2_upstream_request(&mut request, HttpUpstreamRequestPolicy::standard())
                    .is_err(),
                "decorated protected nomination should be rejected: {token:?}"
            );
        }
    }

    /// A protected name decorated with a valid `tchar` (e.g. `'X-Forwarded-For'`) is a well-formed
    /// nomination of a *distinct* header: accepted, but harmless — the real header is untouched.
    #[test]
    fn connection_nomination_allows_tchar_decorated_lookalike() {
        for token in ["'X-Forwarded-For'", "X-Forwarded-For.", "!X-Forwarded-For"] {
            let mut request =
                request_with_headers(&[("Connection", token), ("X-Forwarded-For", "6.6.6.6")]);
            assert!(
                sanitize_h2_upstream_request(&mut request, HttpUpstreamRequestPolicy::standard())
                    .is_ok(),
                "tchar-decorated lookalike is a distinct header, not a protected match: {token:?}"
            );
            assert_eq!(request.headers["x-forwarded-for"], "6.6.6.6");
        }
    }

    /// A policy that tolerates malformed `Connection` nominations while still stripping them.
    fn lenient_policy() -> HttpUpstreamRequestPolicy {
        let mut policy = HttpUpstreamRequestPolicy::standard();
        policy.reject_malformed_connection_nominations = false;
        policy
    }

    /// In lenient mode a malformed nomination is tolerated: it targets a distinct field and leaves
    /// the real protected header intact.
    #[test]
    fn lenient_connection_nomination_tolerates_malformed_token() {
        for token in [
            "\"X-Forwarded-For\"",
            "(X-Forwarded-For",
            "@X-Forwarded-For",
            "X -Forwarded-For",
            "keep-alive, bad token",
        ] {
            let mut request =
                request_with_headers(&[("Connection", token), ("X-Forwarded-For", "6.6.6.6")]);
            assert!(
                sanitize_h2_upstream_request(&mut request, lenient_policy()).is_ok(),
                "malformed nomination should be tolerated in lenient mode: {token:?}"
            );
            assert_eq!(request.headers["x-forwarded-for"], "6.6.6.6");
        }
    }

    /// Even in lenient mode, an exact protected or pseudo-header nomination is still rejected.
    #[test]
    fn lenient_connection_nomination_still_rejects_exact_protected() {
        for token in ["x-forwarded-for", "X-Forwarded-For", "host", ":authority"] {
            let mut request = request_with_headers(&[("Connection", token)]);
            assert!(
                sanitize_h2_upstream_request(&mut request, lenient_policy()).is_err(),
                "exact protected/pseudo nomination must be rejected even in lenient mode: {token:?}"
            );
        }
    }

    #[test]
    fn normal_lifecycle() {
        let mut ds = DownstreamStateMachine::new(false);
        assert!(ds.is_reading());
        assert!(ds.can_poll());
        assert!(!ds.is_errored());

        ds.maybe_finished(true);
        assert!(!ds.is_reading());
        assert!(ds.is_done());
        assert!(ds.can_poll()); // ReadingFinished still allows polling (for idle)
        assert!(!ds.is_errored());
    }

    #[test]
    fn errored_is_terminal() {
        let mut ds = DownstreamStateMachine::new(false);
        ds.to_errored();
        assert!(ds.is_errored());
        assert!(!ds.can_poll());
        assert!(ds.is_done());
    }

    /// `maybe_finished(false)` is always a no-op regardless of state.
    #[test]
    fn maybe_finished_false_is_noop() {
        let mut ds = DownstreamStateMachine::new(false);
        ds.to_errored();
        ds.maybe_finished(false); // must not panic
        assert!(ds.is_errored());
        assert!(!ds.can_poll());
    }

    /// `maybe_finished(true)` on `Errored` is a no-op — `Errored` is terminal.
    #[test]
    fn maybe_finished_true_noop_on_errored() {
        let mut ds = DownstreamStateMachine::new(false);
        ds.to_errored();
        ds.maybe_finished(true); // must not overwrite Errored
        assert!(ds.is_errored());
        assert!(!ds.can_poll());
    }

    /// `reset()` on `Errored` is a no-op — `Errored` is terminal.
    #[test]
    fn reset_noop_on_errored() {
        let mut ds = DownstreamStateMachine::new(false);
        ds.to_errored();
        ds.reset(); // must not overwrite Errored
        assert!(ds.is_errored());
        assert!(!ds.can_poll());
    }
}
