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
    fn parse(req: &RequestHeader) -> Result<Self> {
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

            if token.starts_with(b":") // `:` denotes pseudo-headers such as `:authority`.
                || [
                    b"host".as_slice(),
                    b"x-forwarded-for".as_slice(),
                    b"x-forwarded-host".as_slice(),
                    b"x-forwarded-proto".as_slice(),
                ]
                .iter()
                .any(|protected| token.eq_ignore_ascii_case(protected))
            {
                return Error::e_explain(
                    InvalidHTTPHeader,
                    "protected header cannot be nominated by the Connection header",
                );
            }

            if let Ok(name) = HeaderName::from_bytes(token) {
                headers[len] = Some(name);
                len += 1;
            }
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
        .then(|| ConnectionNominations::parse(req))
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
        ConnectionNominations::parse(req)?.remove_from(req);
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
