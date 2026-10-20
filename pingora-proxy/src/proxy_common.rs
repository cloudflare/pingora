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
use log::debug;
use pingora_cache::fill_cancellation::ERR_CACHE_FILL_CANCELLED;
use pingora_cache::{CacheFillCancellationWatcher, HttpCache};
use pingora_core::protocols::http::authority::{raw_target_authority, RawTargetAuthority};
use pingora_core::protocols::http::HttpTask;
use pingora_core::upstreams::peer::{H1UpgradePolicy, HttpUpstreamRequestPolicy};
use pingora_error::{
    Error,
    ErrorType::{self, InvalidHTTPHeader, WriteError},
    OrErr, Result,
};
use pingora_http::RequestHeader;
use std::time::Duration;
use tokio::sync::mpsc;

const MAX_CONNECTION_NOMINATIONS: usize = 10;

/// Pace an upstream task, waking when an abandoned fill's writer cancels it.
///
/// Cancellation only interrupts the delay. The caller still processes the accepted task;
/// [`CacheFillCancellationState::recv_upstream`] then drains bounded queued work, allowing a complete
/// response already handed over to finish normally.
pub(crate) async fn wait_for_upstream_delay(
    cache: &HttpCache,
    downstream_abandoned: bool,
    duration: Duration,
) {
    if downstream_abandoned {
        if let Some(mut watcher) = cache.miss_cancellation_watcher() {
            tokio::select! {
                biased;
                _ = watcher.cancelled() => {}
                _ = tokio::time::sleep(duration) => {}
            }
            return;
        }
    }
    tokio::time::sleep(duration).await;
}

pub(crate) const KEEP_ALIVE: &str = "keep-alive";
pub(crate) const PROXY_CONNECTION: &str = "proxy-connection";
pub(crate) const HTTP2_SETTINGS: &str = "http2-settings";

/// Authority handling that avoids imposing standard HTTP semantics on custom downstream sessions.
///
/// - `Standard`:
///   - Reconciles `Host`, URI authority, and absolute-form authority.
///   - Revalidates after filters.
///   - Rejects conflicting or ambiguous representations.
/// - `Custom`:
///   - Preserves authority relationships defined by the custom protocol.
///   - Enforces only the chosen H1/H2 upstream protocol's wire requirements.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum AuthorityPolicy {
    Standard,
    Custom,
}

impl AuthorityPolicy {
    pub(crate) fn from(custom: bool) -> Self {
        if custom {
            Self::Custom
        } else {
            Self::Standard
        }
    }

    pub(crate) fn is_standard(self) -> bool {
        self == Self::Standard
    }

    pub(crate) fn is_custom(self) -> bool {
        self == Self::Custom
    }
}

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

/// Set H1 `Host` from the URI or absolute-form authority.
///
/// A single matching `Host` is preserved; others are replaced ([RFC 9113 section 8.3.1]). Without
/// an authority, `Host` is preserved or inserted empty ([RFC 9112 section 3.2]).
///
/// # Errors
///
/// Returns `InvalidHTTPHeader` when the authority contains userinfo or cannot be represented as a
/// `Host` field value.
///
/// [RFC 9112 section 3.2]: https://www.rfc-editor.org/rfc/rfc9112.html#section-3.2
/// [RFC 9113 section 8.3.1]: https://www.rfc-editor.org/rfc/rfc9113.html#section-8.3.1
pub(crate) fn set_h1_host_from_authority(req: &mut RequestHeader) -> Result<()> {
    let authority = req
        .uri
        .authority()
        .map(|authority| authority.as_str().as_bytes())
        .or_else(|| raw_target_authority(req.raw_path()).authority());

    let Some(authority) = authority else {
        // No authority, use Host header.
        if req.headers.contains_key(header::HOST) {
            return Ok(());
        }
        // H1.1 requires Host even when the target has no authority; the H1 caller skips this for
        // Host-less HTTP/1.0 origin-form requests.
        return req.insert_header(header::HOST, "");
    };

    if authority.contains(&b'@') {
        return Error::e_explain(InvalidHTTPHeader, "userinfo in request authority");
    }

    // Avoid replacing the header-map entry when exactly one Host already matches the authority.
    let mut hosts = req.headers.get_all(header::HOST).iter();
    if hosts
        .next()
        .is_some_and(|host| host.as_bytes() == authority)
        && hosts.next().is_none()
    {
        return Ok(());
    }

    let host = http::HeaderValue::from_bytes(authority)
        .or_err(InvalidHTTPHeader, "invalid authority for Host header")?;

    req.insert_header(header::HOST, host)
}

/// Preserve the historical Custom behavior by synthesizing Host only when it is absent.
pub(crate) fn set_h1_host_from_authority_if_absent(req: &mut RequestHeader) -> Result<()> {
    if req.headers.contains_key(header::HOST) {
        return Ok(());
    }
    let host = req
        .uri
        .authority()
        .map_or("", |authority| authority.as_str())
        .to_owned();
    req.insert_header(header::HOST, host)
}

/// Apply a filter-selected `Host` to the outbound authority.
///
/// Rewrites URI authority or H1 absolute-form; origin-form is unchanged.
/// Ambiguous targets are also unchanged so final authority validation can reject them.
///
/// # Errors
///
/// Returns `InvalidHTTPHeader` when `Host` contains userinfo or an invalid authority, or when a URI
/// rewrite would discard non-UTF-8 path bytes or produce an invalid URI.
pub(crate) fn reconcile_upstream_authority(req: &mut RequestHeader) -> Result<()> {
    let Some(host) = req.headers.get(header::HOST) else {
        return Ok(());
    };
    if host.as_bytes().contains(&b'@') {
        return Error::e_explain(InvalidHTTPHeader, "userinfo in Host header");
    }

    if let Some(authority) = req.uri.authority() {
        // Rebuilding via set_uri() clears the raw-byte fallback. Fail instead of replacing a
        // non-UTF-8 path with the URI's lossy replacement characters.
        if !req.raw_path_is_utf8() {
            return Error::e_explain(
                InvalidHTTPHeader,
                "non-UTF-8 path cannot be reconciled with URI authority",
            );
        }
        if host.as_bytes() == authority.as_str().as_bytes() {
            return Ok(());
        }
        let authority = parse_host_authority(host)?;
        let mut parts = http::uri::Parts::default();
        parts.scheme = req.uri.scheme().cloned();
        parts.authority = Some(authority);
        parts.path_and_query = req.uri.path_and_query().cloned();
        let uri = http::Uri::from_parts(parts).map_err(|cause| {
            Error::because(
                InvalidHTTPHeader,
                "failed to apply Host override to URI authority",
                cause,
            )
        })?;
        req.set_uri(uri);
        return Ok(());
    }

    match raw_target_authority(req.raw_path()) {
        RawTargetAuthority::None | RawTargetAuthority::AmbiguousAuthority => Ok(()),
        RawTargetAuthority::Absolute {
            scheme,
            authority,
            path_and_query,
        } => {
            if host.as_bytes() == authority {
                return Ok(());
            }
            parse_host_authority(host)?;
            let mut target =
                Vec::with_capacity(scheme.len() + 3 + host.as_bytes().len() + path_and_query.len());
            target.extend_from_slice(scheme);
            target.extend_from_slice(b"://");
            target.extend_from_slice(host.as_bytes());
            target.extend_from_slice(path_and_query);
            req.set_raw_path(&target)
        }
    }
}

fn parse_host_authority(host: &http::HeaderValue) -> Result<http::uri::Authority> {
    http::uri::Authority::try_from(host.as_bytes()).map_err(|cause| {
        Error::because(
            InvalidHTTPHeader,
            "invalid Host override for authority",
            cause,
        )
    })
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

pub(crate) fn downstream_closed_before_cache_admission() -> Box<Error> {
    Error::explain(WriteError, "downstream closed before cache admission").into_down()
}

/// Whether a failed downstream read means the client left: its connection or HTTP/2 stream
/// closed or failed.
///
/// A read timeout, a malformed request body, or data sent after the request is complete is the
/// proxy's verdict on a client that is still connected and can still get a response, so it is
/// not the client leaving.
pub(crate) fn downstream_read_error_means_client_left(error: &Error) -> bool {
    matches!(
        error.etype(),
        ErrorType::ConnectionClosed | ErrorType::ReadError | ErrorType::H2Error
    )
}

/// The application cancelled a cache fill whose downstream was already abandoned.
///
/// Not retryable: a retry would restart the fill nobody is waiting for.
pub(crate) fn cache_fill_cancelled() -> Box<Error> {
    let mut error = Error::explain(
        ERR_CACHE_FILL_CANCELLED,
        "cache fill cancelled after downstream abandonment",
    )
    .into_down();
    error.set_retry(false);
    error
}

/// Watches for the application's cancellation of an abandoned cache fill.
///
/// The watcher belongs to the miss writer, so watching starts once the downstream is
/// abandoned and a writer exists. A writer that offers no watcher ends the watch for the
/// rest of the upstream attempt.
///
/// The cancellation is honored while the transport loop waits for the next upstream task, see
/// [`recv_upstream`](Self::recv_upstream). Tasks the upstream side had already handed over are
/// processed first, so a complete response is committed even when the cancellation arrived just
/// before its final task, and the number of those tasks is bounded, so an upstream that always
/// has data ready cannot keep an abandoned fill alive.
#[derive(Debug)]
pub(crate) enum CacheFillCancellationState {
    /// No writer has been asked for a watcher yet.
    Unwatched,
    /// The writer offered no watcher; the fill runs until the upstream completes.
    Unsupported,
    Watching(CacheFillCancellationWatcher),
    /// The cancellation was observed. `remaining` bounds how many more tasks
    /// [`recv_upstream`](Self::recv_upstream) itself hands back: fixed at the number the
    /// upstream side had queued when the cancellation was observed, and delivery also stops as
    /// soon as the queue is empty. The transport loop batches each handoff with whatever else
    /// is already queued, so the data that follows a cancellation is bounded but not limited to
    /// that snapshot.
    Draining {
        remaining: usize,
    },
    /// The watched writer's admission ended, committed or not, so a cancellation no longer
    /// applies: an upstream that still sends tasks after a committed response (such as its
    /// final `Done`) must not fail it.
    Settled,
}

impl CacheFillCancellationState {
    pub fn new() -> Self {
        Self::Unwatched
    }

    /// Start watching the miss writer once the downstream is abandoned and a writer exists, and
    /// stop once that writer's admission is no longer active.
    ///
    /// Called at the top of every transport loop iteration.
    pub fn start_watching(&mut self, downstream_abandoned: bool, cache: &HttpCache) {
        match self {
            Self::Unwatched if downstream_abandoned && cache.is_cache_admission_active() => {
                *self = match cache.miss_cancellation_watcher() {
                    Some(watcher) => Self::Watching(watcher),
                    None => Self::Unsupported,
                };
            }
            Self::Watching(_) | Self::Draining { .. } if !cache.is_cache_admission_active() => {
                *self = Self::Settled;
            }
            _ => {}
        }
    }

    #[cfg(test)]
    pub fn is_watching(&self) -> bool {
        matches!(self, Self::Watching(_))
    }

    /// Receive the next upstream task, or fail with [`cache_fill_cancelled`] once the
    /// application cancelled the fill and every task it had already handed over is processed.
    ///
    /// Tasks queued when the cancellation is first observed are delivered first, so the final
    /// task of a complete response is never discarded, and the count handed back this way is
    /// bounded at that moment's queue length. The upstream producer and the transport loop are
    /// sibling futures of one `tokio::try_join!`, so nothing can refill the queue while a batch
    /// is being drained, and every handoff here decrements the budget, so an upstream that
    /// always has data ready cannot defer the cancellation indefinitely.
    /// `None` means the upstream side closed the channel, exactly like
    /// [`mpsc::Receiver::recv`].
    ///
    /// Cancel-safe: neither a task nor a cancellation is lost when the returned future is
    /// dropped before it resolves.
    pub async fn recv_upstream(
        &mut self,
        rx: &mut mpsc::Receiver<HttpTask>,
    ) -> Result<Option<HttpTask>> {
        if let Self::Watching(watcher) = self {
            if watcher.is_cancelled() {
                // Requested while the loop was busy: hand over what the upstream side already
                // queued, then fail. Counting the queue once is what bounds the wait.
                *self = Self::Draining {
                    remaining: rx.len(),
                };
            }
        }
        match self {
            Self::Unwatched | Self::Unsupported | Self::Settled => Ok(rx.recv().await),
            Self::Watching(watcher) => {
                tokio::select! {
                    biased;
                    task = rx.recv() => return Ok(task),
                    _ = watcher.cancelled() => {}
                }
                // The cancellation won the wait, so the upstream side had queued nothing.
                *self = Self::Draining { remaining: 0 };
                debug!("cache fill cancelled after downstream abandonment");
                Err(cache_fill_cancelled())
            }
            Self::Draining { remaining } => {
                if *remaining > 0 {
                    if let Ok(task) = rx.try_recv() {
                        *remaining -= 1;
                        return Ok(Some(task));
                    }
                }
                debug!("cache fill cancelled after downstream abandonment");
                Err(cache_fill_cancelled())
            }
        }
    }
}

/// Possible downstream states during request multiplexing
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

    /// Reconstruct downstream state at the start of an upstream attempt.
    pub fn for_proxy_attempt(finished: bool, downstream_abandoned: bool) -> Self {
        if downstream_abandoned {
            Self::Errored
        } else {
            Self::new(finished)
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

/// Per-attempt response progress. The upstream producer emitting its terminal task is tracked
/// separately from completing either downstream delivery path.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ResponseStateMachine {
    upstream_producer_done: bool,
    upstream_delivery_done: bool,
    cached_response_done: bool,
}

impl ResponseStateMachine {
    pub fn new() -> Self {
        ResponseStateMachine {
            upstream_producer_done: false,
            upstream_delivery_done: false,
            cached_response_done: true, // no cached response by default
        }
    }

    /// Whether direct upstream delivery and any cached delivery have both ended.
    /// This does not describe the lifetime of the upstream task producer.
    pub fn is_done(&self) -> bool {
        self.upstream_delivery_done && self.cached_response_done
    }

    /// Whether this attempt's upstream task producer emitted its terminal task.
    /// It can be true even when the task was diverted to cache instead of delivered.
    pub fn upstream_producer_done(&self) -> bool {
        self.upstream_producer_done
    }

    /// Whether no more direct upstream output needs downstream delivery.
    pub fn upstream_delivery_done(&self) -> bool {
        self.upstream_delivery_done
    }

    pub fn cached_done(&self) -> bool {
        self.cached_response_done
    }

    pub fn enable_cached_response(&mut self) {
        self.cached_response_done = false;
    }

    pub fn maybe_set_upstream_producer_done(&mut self, done: bool) {
        if done {
            self.upstream_producer_done = true;
        }
    }

    pub fn maybe_set_upstream_delivery_done(&mut self, done: bool) {
        if done {
            self.upstream_delivery_done = true;
        }
    }

    pub fn maybe_set_cache_done(&mut self, done: bool) {
        if done {
            self.cached_response_done = true;
        }
    }
}

/// Whether `task` is the last task an HTTP/2 or custom upstream producer sends.
///
/// Those producers always finish a response with [`HttpTask::Done`] or [`HttpTask::Failed`],
/// after any trailer or end-of-stream body. The HTTP/1 producer stops reading at the first
/// [`HttpTask::is_end`] task and never sends `Done` or a trailer, so HTTP/1 uses `is_end`.
pub(crate) fn ends_upstream_task_stream(task: &HttpTask) -> bool {
    matches!(task, HttpTask::Done | HttpTask::Failed(_))
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
    fn h1_host_is_copied_from_authority() {
        let request_with_uri = |uri: &str, host: Option<&str>| {
            let mut request = match host {
                Some(host) => request_with_headers(&[("Host", host)]),
                None => request_with_headers(&[]),
            };
            request.set_uri(uri.parse().unwrap());
            request
        };

        // Absent Host is synthesized from :authority.
        let mut request = request_with_uri("https://authority.example/test", None);
        set_h1_host_from_authority(&mut request).unwrap();
        assert_eq!(request.headers[header::HOST], "authority.example");

        // A conflicting Host is replaced, so the upstream cannot disagree with the filters.
        let mut request = request_with_uri("https://authority.example/test", Some("host.example"));
        set_h1_host_from_authority(&mut request).unwrap();
        assert_eq!(request.headers[header::HOST], "authority.example");
        assert_eq!(request.headers.get_all(header::HOST).iter().count(), 1);

        // No :authority: the existing Host is the only source and stands.
        let mut request = request_with_uri("/test", Some("host.example"));
        set_h1_host_from_authority(&mut request).unwrap();
        assert_eq!(request.headers[header::HOST], "host.example");

        // Neither present: the field is still required, with an empty value.
        let mut request = request_with_uri("/test", None);
        set_h1_host_from_authority(&mut request).unwrap();
        assert_eq!(request.headers[header::HOST], "");

        let mut request = request_with_uri("https://user@authority.example/test", None);
        let err = set_h1_host_from_authority(&mut request).unwrap_err();
        assert_eq!(err.etype(), &InvalidHTTPHeader);

        let mut custom = request_with_uri("https://user@authority.example/test", None);
        set_h1_host_from_authority_if_absent(&mut custom).unwrap();
        assert_eq!(custom.headers[header::HOST], "user@authority.example");
        custom
            .insert_header(header::HOST, "custom.example")
            .unwrap();
        set_h1_host_from_authority_if_absent(&mut custom).unwrap();
        assert_eq!(custom.headers[header::HOST], "custom.example");
    }

    #[test]
    fn filter_host_override_updates_outbound_authority() {
        let mut request = request_with_headers(&[("Host", "origin.example")]);
        request.set_uri("https://client.example/path?q=1".parse().unwrap());

        reconcile_upstream_authority(&mut request).unwrap();
        assert_eq!(request.uri.authority().unwrap(), "origin.example");
        assert_eq!(request.uri.path_and_query().unwrap(), "/path?q=1");

        let mut userinfo = request_with_headers(&[("Host", "user@evil.example")]);
        userinfo.set_uri("https://client.example/path".parse().unwrap());
        assert!(reconcile_upstream_authority(&mut userinfo).is_err());

        let mut origin_form = request_with_headers(&[("Host", "origin.example")]);
        reconcile_upstream_authority(&mut origin_form).unwrap();
        assert!(origin_form.uri.authority().is_none());

        let mut absolute =
            RequestHeader::build("GET", b"http://client.example/path?q=1", None).unwrap();
        absolute
            .append_header(header::HOST, "origin.example")
            .unwrap();
        reconcile_upstream_authority(&mut absolute).unwrap();
        assert_eq!(absolute.raw_path(), b"http://origin.example/path?q=1");

        let mut non_utf8 =
            RequestHeader::build("GET", b"http://client.example/\xff", None).unwrap();
        non_utf8
            .append_header(header::HOST, "origin.example")
            .unwrap();
        reconcile_upstream_authority(&mut non_utf8).unwrap();
        assert_eq!(non_utf8.raw_path(), b"http://origin.example/\xff");
        assert!(!non_utf8.raw_path_is_utf8());

        let mut h2_deleted = request_with_headers(&[]);
        h2_deleted.set_uri("https://client.example/path".parse().unwrap());
        set_h1_host_from_authority(&mut h2_deleted).unwrap();
        h2_deleted.remove_header(&header::HOST);
        reconcile_upstream_authority(&mut h2_deleted).unwrap();
        set_h1_host_from_authority(&mut h2_deleted).unwrap();
        assert_eq!(h2_deleted.headers[header::HOST], "client.example");

        let mut h1_deleted = request_with_headers(&[("Host", "client.example")]);
        h1_deleted.remove_header(&header::HOST);
        reconcile_upstream_authority(&mut h1_deleted).unwrap();
        set_h1_host_from_authority(&mut h1_deleted).unwrap();
        assert_eq!(h1_deleted.headers[header::HOST], "");

        let mut absolute_deleted =
            RequestHeader::build("GET", b"http://client.example/path", None).unwrap();
        absolute_deleted
            .append_header(header::HOST, "client.example")
            .unwrap();
        absolute_deleted.remove_header(&header::HOST);
        reconcile_upstream_authority(&mut absolute_deleted).unwrap();
        set_h1_host_from_authority(&mut absolute_deleted).unwrap();
        assert_eq!(absolute_deleted.headers[header::HOST], "client.example");
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
    fn proxy_attempt_reconstructs_downstream_state() {
        for (finished, abandoned, expected) in [
            (false, false, DownstreamStateMachine::Reading),
            (true, false, DownstreamStateMachine::ReadingFinished),
            (false, true, DownstreamStateMachine::Errored),
            (true, true, DownstreamStateMachine::Errored),
        ] {
            assert_eq!(
                DownstreamStateMachine::for_proxy_attempt(finished, abandoned),
                expected
            );
        }
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

    mod fill_cancellation {
        use super::*;
        use crate::test_support::CANCELLATION_WATCHED_STORAGE;
        use pingora_cache::{CacheKey, CacheMeta, HttpCache, MemCache, Storage};
        use pingora_error::{ErrorSource, RetryType};
        use pingora_http::ResponseHeader;
        use std::sync::LazyLock;
        use std::time::{Duration, SystemTime};

        static PLAIN_STORAGE: LazyLock<MemCache> = LazyLock::new(MemCache::new);

        async fn miss_with_writer(key: &str, storage: &'static (dyn Storage + Sync)) -> HttpCache {
            let mut cache = HttpCache::new();
            cache.enable(storage, None, None, None, None);
            cache.set_cache_key(CacheKey::new(key, ""));
            cache.cache_miss();
            let now = SystemTime::now();
            cache.set_cache_meta(CacheMeta::new(
                now + Duration::from_secs(60),
                now,
                0,
                0,
                ResponseHeader::build(200, None).unwrap(),
            ));
            cache.set_miss_handler().await.unwrap();
            assert!(cache.is_cache_admission_active());
            cache
        }

        #[tokio::test]
        async fn detached_pacing_observes_an_already_requested_cancellation() {
            let key = "/pacing-cancelled-before-wait";
            let cache = miss_with_writer(key, &*CANCELLATION_WATCHED_STORAGE).await;
            assert!(CANCELLATION_WATCHED_STORAGE.cancellation_for(key).cancel());
            tokio::time::timeout(
                Duration::from_secs(1),
                wait_for_upstream_delay(&cache, true, Duration::from_secs(65)),
            )
            .await
            .expect("an already cancelled fill waited for its pacing budget");
        }

        #[tokio::test]
        async fn attached_pacing_keeps_its_delay_despite_writer_cancellation() {
            let key = "/pacing-attached-cancelled-writer";
            let cache = miss_with_writer(key, &*CANCELLATION_WATCHED_STORAGE).await;
            assert!(CANCELLATION_WATCHED_STORAGE.cancellation_for(key).cancel());
            let duration = Duration::from_millis(30);
            let start = tokio::time::Instant::now();
            wait_for_upstream_delay(&cache, false, duration).await;
            assert!(start.elapsed() >= duration);
        }

        #[tokio::test]
        async fn detached_pacing_keeps_its_delay_without_a_cancellation() {
            let key = "/pacing-detached-live-writer";
            let cache = miss_with_writer(key, &*CANCELLATION_WATCHED_STORAGE).await;
            assert!(!CANCELLATION_WATCHED_STORAGE
                .cancellation_for(key)
                .is_cancelled());
            let duration = Duration::from_millis(30);
            let start = tokio::time::Instant::now();
            wait_for_upstream_delay(&cache, true, duration).await;
            assert!(start.elapsed() >= duration);
        }

        #[tokio::test]
        async fn detached_pacing_keeps_its_delay_without_a_storage_watcher() {
            let cache = miss_with_writer("/pacing-detached-plain-storage", &*PLAIN_STORAGE).await;
            assert!(cache.miss_cancellation_watcher().is_none());
            let duration = Duration::from_millis(30);
            let start = tokio::time::Instant::now();
            wait_for_upstream_delay(&cache, true, duration).await;
            assert!(start.elapsed() >= duration);
        }

        /// `recv_upstream` on `rx`, or `None` when it is still pending after a short wait.
        async fn recv_quickly(
            watch: &mut CacheFillCancellationState,
            rx: &mut mpsc::Receiver<HttpTask>,
        ) -> Option<Result<Option<HttpTask>>> {
            tokio::time::timeout(Duration::from_millis(20), watch.recv_upstream(rx))
                .await
                .ok()
        }

        fn assert_cancelled(result: Option<Result<Option<HttpTask>>>) {
            let error = result
                .expect("recv_upstream still pending")
                .expect_err("recv_upstream did not fail");
            assert_eq!(error.etype(), &ERR_CACHE_FILL_CANCELLED);
            assert_eq!(error.esource(), &ErrorSource::Downstream);
            assert_eq!(error.retry, RetryType::Decided(false));
        }

        #[tokio::test]
        async fn cancellation_is_ignored_until_the_downstream_is_abandoned() {
            let key = "/cancellation-before-abandonment";
            let cache = miss_with_writer(key, &*CANCELLATION_WATCHED_STORAGE).await;
            assert!(CANCELLATION_WATCHED_STORAGE.cancellation_for(key).cancel());
            let (_tx, mut rx) = mpsc::channel(1);

            let mut watch = CacheFillCancellationState::new();
            watch.start_watching(false, &cache);
            assert!(!watch.is_watching());
            assert!(recv_quickly(&mut watch, &mut rx).await.is_none());

            watch.start_watching(true, &cache);
            assert!(watch.is_watching());
            assert_cancelled(recv_quickly(&mut watch, &mut rx).await);
        }

        #[tokio::test]
        async fn watching_starts_with_the_writer_and_wakes_on_cancel() {
            let key = "/cancellation-after-writer";
            let mut cache = HttpCache::new();
            cache.enable(&*CANCELLATION_WATCHED_STORAGE, None, None, None, None);
            cache.set_cache_key(CacheKey::new(key, ""));
            cache.cache_miss();
            let (_tx, mut rx) = mpsc::channel(1);

            let mut watch = CacheFillCancellationState::new();
            watch.start_watching(true, &cache);
            assert!(
                !watch.is_watching(),
                "nothing to watch before a writer exists"
            );

            cache = miss_with_writer(key, &*CANCELLATION_WATCHED_STORAGE).await;
            watch.start_watching(true, &cache);
            assert!(watch.is_watching());
            assert!(recv_quickly(&mut watch, &mut rx).await.is_none());

            let pending = tokio::spawn(async move {
                let result = watch.recv_upstream(&mut rx).await;
                (watch, rx, result)
            });
            tokio::time::sleep(Duration::from_millis(10)).await;
            assert!(CANCELLATION_WATCHED_STORAGE.cancellation_for(key).cancel());
            let (mut watch, mut rx, result) = tokio::time::timeout(Duration::from_secs(1), pending)
                .await
                .expect("cancel did not wake the pending receive")
                .unwrap();
            assert_cancelled(Some(result));
            assert_cancelled(recv_quickly(&mut watch, &mut rx).await);
        }

        #[tokio::test]
        async fn queued_tasks_win_over_a_pending_cancellation() {
            let key = "/cancellation-with-queued-tasks";
            let cache = miss_with_writer(key, &*CANCELLATION_WATCHED_STORAGE).await;
            let mut watch = CacheFillCancellationState::new();
            watch.start_watching(true, &cache);
            assert!(watch.is_watching());
            assert!(CANCELLATION_WATCHED_STORAGE.cancellation_for(key).cancel());

            let (tx, mut rx) = mpsc::channel(2);
            tx.try_send(HttpTask::Body(None, false)).unwrap();
            tx.try_send(HttpTask::Done).unwrap();
            assert!(matches!(
                recv_quickly(&mut watch, &mut rx).await,
                Some(Ok(Some(HttpTask::Body(None, false))))
            ));
            assert!(matches!(
                recv_quickly(&mut watch, &mut rx).await,
                Some(Ok(Some(HttpTask::Done)))
            ));
            assert_cancelled(recv_quickly(&mut watch, &mut rx).await);

            // Once the queued tasks are gone the fill stays cancelled: reporting the closed
            // channel as a clean end would let the loop commit a response the upstream never
            // finished sending.
            drop(tx);
            assert_cancelled(recv_quickly(&mut watch, &mut rx).await);
        }

        #[tokio::test]
        async fn a_closed_upstream_channel_is_not_a_cancellation() {
            let key = "/cancellation-with-a-closed-upstream";
            let cache = miss_with_writer(key, &*CANCELLATION_WATCHED_STORAGE).await;
            let mut watch = CacheFillCancellationState::new();
            watch.start_watching(true, &cache);
            assert!(watch.is_watching());

            let (tx, mut rx) = mpsc::channel(2);
            drop(tx);
            assert!(
                matches!(recv_quickly(&mut watch, &mut rx).await, Some(Ok(None))),
                "a closed upstream channel ends the upstream, it is not a cancellation"
            );
        }

        #[tokio::test]
        async fn a_steadily_streaming_upstream_cannot_outrun_a_cancellation() {
            let key = "/cancellation-with-a-sustained-backlog";
            let cache = miss_with_writer(key, &*CANCELLATION_WATCHED_STORAGE).await;
            let mut watch = CacheFillCancellationState::new();
            watch.start_watching(true, &cache);
            assert!(watch.is_watching());

            // An upstream that always has data ready, exactly like an origin streaming a large
            // asset faster than the cache write path retires it. The queue is refilled
            // synchronously so the backlog is a precondition of the test rather than something
            // the scheduler has to provide: a spawned producer may not run before the first
            // receive, which would let the cancellation win against an empty queue.
            let (tx, mut rx) = mpsc::channel(4);
            for _ in 0..64 {
                while tx.try_send(HttpTask::Body(None, false)).is_ok() {}
                assert!(!rx.is_empty(), "the backlog must be ready at every receive");
                if let Some(Ok(Some(_))) = recv_quickly(&mut watch, &mut rx).await {
                    continue;
                }
                panic!("a queued task lost to a cancellation that was not yet requested");
            }

            // The cancellation must be honored while the backlog is still being handed over,
            // otherwise an abandoned fill of a large asset runs to completion despite `cancel`.
            assert!(CANCELLATION_WATCHED_STORAGE.cancellation_for(key).cancel());
            for _ in 0..64 {
                while tx.try_send(HttpTask::Body(None, false)).is_ok() {}
                assert!(!rx.is_empty(), "the backlog must be ready at every receive");
                match recv_quickly(&mut watch, &mut rx).await {
                    Some(Ok(Some(_))) => continue,
                    result => {
                        assert_cancelled(result);
                        return;
                    }
                }
            }
            panic!("a steadily streaming upstream starved the cancellation");
        }

        #[tokio::test]
        async fn writers_without_a_watcher_are_never_cancelled() {
            let cache = miss_with_writer("/no-watcher", &*PLAIN_STORAGE).await;
            let (_tx, mut rx) = mpsc::channel(1);
            let mut watch = CacheFillCancellationState::new();
            watch.start_watching(true, &cache);
            assert!(!watch.is_watching());
            assert!(recv_quickly(&mut watch, &mut rx).await.is_none());
            watch.start_watching(true, &cache);
            assert!(matches!(watch, CacheFillCancellationState::Unsupported));
        }
    }
}
