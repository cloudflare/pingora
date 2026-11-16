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

//! Limits a listening endpoint applies to the connections it accepts.

use log::warn;
use std::fmt;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::l4::Stream as L4Stream;

/// The handshake timeout of an endpoint without [`AcceptLimits`], and the default of
/// [`AcceptLimits::handshake_timeout`].
pub const DEFAULT_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(60);

/// Limits a listening endpoint applies to the connections it accepts.
///
/// They protect a server that is offered more connections than it can handshake with. Every
/// accepted connection holds a file descriptor until it closes, and a TLS handshake costs the
/// same CPU whether or not its client is still waiting. When clients time out and reconnect, a
/// server can spend most of its CPU on handshakes for clients that have already left.
///
/// The default sets no limit and keeps the 60 second handshake timeout, so the endpoint behaves
/// as one without limits. Fields may be added, so build it with `..Default::default()`:
///
/// ```rust
/// use pingora_core::listeners::{AcceptLimits, AcceptStats, ListenerConfig};
/// use std::sync::Arc;
/// use std::time::Duration;
///
/// let stats = Arc::new(AcceptStats::default());
/// let endpoint = ListenerConfig::tcp("0.0.0.0:443").accept_limits(AcceptLimits {
///     handshake_timeout: Duration::from_secs(5),
///     max_connections: Some(400_000),
///     max_pending_handshakes: Some(10_000),
///     skip_closed_peers: true,
///     stats: Some(stats.clone()),
///     ..Default::default()
/// });
/// ```
#[derive(Debug, Clone)]
pub struct AcceptLimits {
    /// The longest a connection's handshake may take, from when the server starts it until it
    /// finishes. This includes pre-TLS processing, such as reading a PROXY protocol header, and
    /// any wait for a TLS offload thread. Default: 60 seconds.
    pub handshake_timeout: Duration,
    /// The most connections the endpoint keeps open at once, counted from accept until the
    /// connection closes. A connection accepted over the limit is closed at once, before its
    /// handshake. The limit counts only this endpoint's connections, while the file descriptor
    /// limit covers the whole process: to keep `accept()` from failing with `EMFILE`, keep the
    /// sum of `max_connections` over all endpoints, plus the descriptors the process needs for
    /// everything else, below that limit. A connection counts until its stream is dropped, so
    /// one that an application hands to another task keeps counting there, and one whose
    /// handshake timed out while waiting for a TLS offload thread counts until that thread
    /// drops it. Default: no limit.
    pub max_connections: Option<usize>,
    /// The most connections the endpoint holds whose handshake has not finished, counted from
    /// accept until the handshake finishes, fails or times out. This includes connections
    /// waiting for pre-TLS processing or for a TLS offload thread. A connection accepted over the
    /// limit is closed at once, before its handshake. A handshake the server cannot finish before
    /// its client gives up is wasted work, so size it to what the endpoint can finish in that
    /// time. Default: no limit.
    pub max_pending_handshakes: Option<usize>,
    /// Close a connection without a handshake if its peer has already closed or reset it when
    /// the handshake would start. If a TLS offload thread runs the handshake, the check runs
    /// again when that thread starts it. A TLS client cannot finish a handshake after closing,
    /// but on a plaintext endpoint this also closes a connection whose client sent a complete
    /// request and then shut down its write side. If the kernel cannot report the connection's
    /// TCP state, nothing is skipped. Linux only; elsewhere it has no effect. Default: false.
    pub skip_closed_peers: bool,
    /// Counters for the endpoint. Endpoints given the same [`AcceptStats`] add to the same
    /// counters. Default: none.
    pub stats: Option<Arc<AcceptStats>>,
}

impl Default for AcceptLimits {
    fn default() -> Self {
        AcceptLimits {
            handshake_timeout: DEFAULT_HANDSHAKE_TIMEOUT,
            max_connections: None,
            max_pending_handshakes: None,
            skip_closed_peers: false,
            stats: None,
        }
    }
}

/// Counters for endpoints with [`AcceptLimits`], for export as metrics.
#[derive(Debug, Default)]
pub struct AcceptStats {
    open_connections: AtomicUsize,
    pending_handshakes: AtomicUsize,
    closed_at_connection_limit: AtomicU64,
    closed_at_handshake_limit: AtomicU64,
    closed_peer_gone: AtomicU64,
    handshake_timeouts: AtomicU64,
    handshake_errors: AtomicU64,
}

impl AcceptStats {
    /// Connections accepted and still open, counted until their stream is dropped; see
    /// [`max_connections`](AcceptLimits::max_connections).
    pub fn open_connections(&self) -> usize {
        self.open_connections.load(Ordering::Relaxed)
    }

    /// Connections accepted whose handshake has not finished, including those waiting for a
    /// thread to start it.
    pub fn pending_handshakes(&self) -> usize {
        self.pending_handshakes.load(Ordering::Relaxed)
    }

    /// Connections closed at accept because the endpoint had
    /// [`max_connections`](AcceptLimits::max_connections) open.
    pub fn closed_at_connection_limit(&self) -> u64 {
        self.closed_at_connection_limit.load(Ordering::Relaxed)
    }

    /// Connections closed at accept because the endpoint had
    /// [`max_pending_handshakes`](AcceptLimits::max_pending_handshakes) pending.
    pub fn closed_at_handshake_limit(&self) -> u64 {
        self.closed_at_handshake_limit.load(Ordering::Relaxed)
    }

    /// Connections closed without a handshake because the peer had already closed them
    /// ([`skip_closed_peers`](AcceptLimits::skip_closed_peers)).
    pub fn closed_peer_gone(&self) -> u64 {
        self.closed_peer_gone.load(Ordering::Relaxed)
    }

    /// Handshakes that did not finish within
    /// [`handshake_timeout`](AcceptLimits::handshake_timeout).
    pub fn handshake_timeouts(&self) -> u64 {
        self.handshake_timeouts.load(Ordering::Relaxed)
    }

    /// Handshakes that failed.
    pub fn handshake_errors(&self) -> u64 {
        self.handshake_errors.load(Ordering::Relaxed)
    }
}

/// Why a connection was closed without a handshake.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
// Only constructed on Linux, the only platform with the checks.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) enum SkipReason {
    /// The peer had already closed or reset the connection.
    PeerClosed,
}

impl fmt::Display for SkipReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SkipReason::PeerClosed => write!(f, "the peer had already closed it"),
        }
    }
}

/// An endpoint's [`AcceptLimits`] and the state that enforces them, shared by all of the
/// endpoint's accept loops.
#[derive(Debug)]
pub(crate) struct EndpointLimits {
    limits: AcceptLimits,
    endpoint: String,
    /// Open connections; only counted when `limits.max_connections` is set.
    open: AtomicUsize,
    /// Pending handshakes; only counted when `limits.max_pending_handshakes` is set.
    pending: AtomicUsize,
    connection_limit_log: LogLimiter,
    handshake_limit_log: LogLimiter,
}

impl EndpointLimits {
    pub(crate) fn new(limits: AcceptLimits, endpoint: String) -> Self {
        EndpointLimits {
            limits,
            endpoint,
            open: AtomicUsize::new(0),
            pending: AtomicUsize::new(0),
            connection_limit_log: LogLimiter::new(),
            handshake_limit_log: LogLimiter::new(),
        }
    }

    pub(crate) fn handshake_timeout(&self) -> Duration {
        self.limits.handshake_timeout
    }

    pub(crate) fn skips_closed_peers(&self) -> bool {
        self.limits.skip_closed_peers
    }

    /// Admit a newly accepted connection, or return `None` if the endpoint already has
    /// `max_connections` open or `max_pending_handshakes` pending, in which case the caller
    /// should close the connection.
    pub(crate) fn admit(self: &Arc<Self>) -> Option<(ConnectionPermit, PendingHandshake)> {
        let stats = self.limits.stats.as_deref();
        if let Some(max) = self.limits.max_connections {
            if !take_slot(&self.open, max) {
                if let Some(stats) = stats {
                    stats
                        .closed_at_connection_limit
                        .fetch_add(1, Ordering::Relaxed);
                }
                if let Some(closed) = self.connection_limit_log.hit() {
                    warn!(
                        "{}: closed {closed} new connections at the limit of {max} open connections",
                        self.endpoint
                    );
                }
                return None;
            }
        }
        if let Some(max) = self.limits.max_pending_handshakes {
            if !take_slot(&self.pending, max) {
                // Until this is given back, a racing accept loop sees one more open connection.
                if self.limits.max_connections.is_some() {
                    self.open.fetch_sub(1, Ordering::Relaxed);
                }
                if let Some(stats) = stats {
                    stats
                        .closed_at_handshake_limit
                        .fetch_add(1, Ordering::Relaxed);
                }
                if let Some(closed) = self.handshake_limit_log.hit() {
                    warn!(
                        "{}: closed {closed} new connections at the limit of {max} pending handshakes",
                        self.endpoint
                    );
                }
                return None;
            }
        }
        if let Some(stats) = stats {
            stats.open_connections.fetch_add(1, Ordering::Relaxed);
            stats.pending_handshakes.fetch_add(1, Ordering::Relaxed);
        }
        let open_counted = self.limits.max_connections.is_some() || stats.is_some();
        let pending_counted = self.limits.max_pending_handshakes.is_some() || stats.is_some();
        Some((
            ConnectionPermit(open_counted.then(|| self.clone())),
            PendingHandshake(pending_counted.then(|| self.clone())),
        ))
    }

    /// Check whether a connection should be closed instead of starting its handshake.
    pub(crate) fn check_before_handshake(&self, stream: &L4Stream) -> Option<SkipReason> {
        if !self.limits.skip_closed_peers {
            return None;
        }
        let reason = skip_reason(stream)?;
        if let Some(stats) = &self.limits.stats {
            let counter = match reason {
                SkipReason::PeerClosed => &stats.closed_peer_gone,
            };
            counter.fetch_add(1, Ordering::Relaxed);
        }
        Some(reason)
    }

    pub(crate) fn count_handshake_timeout(&self) {
        if let Some(stats) = &self.limits.stats {
            stats.handshake_timeouts.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub(crate) fn count_handshake_error(&self) {
        if let Some(stats) = &self.limits.stats {
            stats.handshake_errors.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Counts a connection as open until dropped. The connection's stream holds it, so it is
/// dropped when the stream is, wherever that happens.
pub(crate) struct ConnectionPermit(Option<Arc<EndpointLimits>>);

// Every stream holds one, so keep the endpoint's state out of the stream's `Debug` output.
impl fmt::Debug for ConnectionPermit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ConnectionPermit")
            .field("counted", &self.0.is_some())
            .finish()
    }
}

impl ConnectionPermit {
    /// A permit for a connection that no limit counts.
    pub(crate) fn uncounted() -> Self {
        ConnectionPermit(None)
    }
}

impl Drop for ConnectionPermit {
    fn drop(&mut self) {
        if let Some(endpoint) = &self.0 {
            if endpoint.limits.max_connections.is_some() {
                endpoint.open.fetch_sub(1, Ordering::Relaxed);
            }
            if let Some(stats) = &endpoint.limits.stats {
                stats.open_connections.fetch_sub(1, Ordering::Relaxed);
            }
        }
    }
}

/// Counts a connection's handshake as pending until dropped.
#[derive(Debug)]
pub(crate) struct PendingHandshake(Option<Arc<EndpointLimits>>);

impl PendingHandshake {
    /// A pending handshake that no limit or counter tracks.
    pub(crate) fn uncounted() -> Self {
        PendingHandshake(None)
    }
}

impl Drop for PendingHandshake {
    fn drop(&mut self) {
        if let Some(endpoint) = &self.0 {
            if endpoint.limits.max_pending_handshakes.is_some() {
                endpoint.pending.fetch_sub(1, Ordering::Relaxed);
            }
            if let Some(stats) = &endpoint.limits.stats {
                stats.pending_handshakes.fetch_sub(1, Ordering::Relaxed);
            }
        }
    }
}

/// Take one of `max` slots counted by `count`. Returns false, taking nothing, if all are taken.
fn take_slot(count: &AtomicUsize, max: usize) -> bool {
    // Never count past `max`: an add over the limit, even one undone at once, could make a
    // racing accept loop close a connection that fits.
    let mut taken = count.load(Ordering::Relaxed);
    loop {
        if taken >= max {
            return false;
        }
        match count.compare_exchange_weak(taken, taken + 1, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return true,
            Err(current) => taken = current,
        }
    }
}

// From include/net/tcp_states.h.
#[cfg(target_os = "linux")]
const TCP_CLOSE: u8 = 7;
#[cfg(target_os = "linux")]
const TCP_CLOSE_WAIT: u8 = 8;

#[cfg(target_os = "linux")]
fn skip_reason(stream: &L4Stream) -> Option<SkipReason> {
    use crate::protocols::l4::ext::get_tcp_info;
    use std::os::unix::io::AsRawFd;

    // TCP_INFO fails on Unix domain sockets, which are never skipped.
    let info = get_tcp_info(stream.as_raw_fd()).ok()?;
    // CLOSE_WAIT: the peer sent a FIN. CLOSE: it sent a RST.
    matches!(info.tcpi_state, TCP_CLOSE_WAIT | TCP_CLOSE).then_some(SkipReason::PeerClosed)
}

#[cfg(not(target_os = "linux"))]
fn skip_reason(_stream: &L4Stream) -> Option<SkipReason> {
    None
}

/// Allows one log line per second, and counts the events in between.
#[derive(Debug)]
struct LogLimiter {
    start: Instant,
    /// Milliseconds after `start` before which no line is allowed.
    next_ms: AtomicU64,
    events: AtomicU64,
}

impl LogLimiter {
    const INTERVAL_MS: u64 = 1000;

    fn new() -> Self {
        LogLimiter {
            start: Instant::now(),
            next_ms: AtomicU64::new(0),
            events: AtomicU64::new(0),
        }
    }

    /// Count one event. If a line is allowed now, return the events since the last line,
    /// this one included.
    fn hit(&self) -> Option<u64> {
        self.events.fetch_add(1, Ordering::Relaxed);
        let now = self.start.elapsed().as_millis() as u64;
        let next = self.next_ms.load(Ordering::Relaxed);
        if now < next
            || self
                .next_ms
                .compare_exchange(
                    next,
                    now + Self::INTERVAL_MS,
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                )
                .is_err()
        {
            return None;
        }
        Some(self.events.swap(0, Ordering::Relaxed))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_limits_change_nothing() {
        let limits = AcceptLimits::default();
        assert_eq!(limits.handshake_timeout, Duration::from_secs(60));
        assert_eq!(limits.max_connections, None);
        assert_eq!(limits.max_pending_handshakes, None);
        assert!(!limits.skip_closed_peers);
        assert!(limits.stats.is_none());
    }

    #[test]
    fn default_limits_count_nothing() {
        let endpoint = Arc::new(EndpointLimits::new(AcceptLimits::default(), "test".into()));
        let (permit, pending) = endpoint.admit().unwrap();
        assert!(permit.0.is_none());
        assert!(pending.0.is_none());
        assert_eq!(endpoint.open.load(Ordering::Relaxed), 0);
        assert_eq!(endpoint.pending.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn admit_stops_at_max_connections_until_a_permit_drops() {
        let stats = Arc::new(AcceptStats::default());
        let endpoint = Arc::new(EndpointLimits::new(
            AcceptLimits {
                max_connections: Some(2),
                stats: Some(stats.clone()),
                ..Default::default()
            },
            "test".into(),
        ));
        let first = endpoint.admit().unwrap();
        let second = endpoint.admit().unwrap();
        assert!(endpoint.admit().is_none());
        assert_eq!(stats.open_connections(), 2);
        assert_eq!(stats.pending_handshakes(), 2);
        assert_eq!(stats.closed_at_connection_limit(), 1);

        // Finishing a handshake does not free a slot; closing the connection does.
        drop(first.1);
        assert_eq!(stats.pending_handshakes(), 1);
        assert!(endpoint.admit().is_none());
        drop(first.0);
        assert_eq!(stats.open_connections(), 1);
        assert!(endpoint.admit().is_some());
        drop(second);
        assert_eq!(stats.open_connections(), 0);
        assert_eq!(stats.pending_handshakes(), 0);
        assert_eq!(stats.closed_at_connection_limit(), 2);
    }

    #[test]
    fn admit_stops_at_max_pending_handshakes_until_a_handshake_ends() {
        let stats = Arc::new(AcceptStats::default());
        let endpoint = Arc::new(EndpointLimits::new(
            AcceptLimits {
                max_connections: Some(3),
                max_pending_handshakes: Some(2),
                stats: Some(stats.clone()),
                ..Default::default()
            },
            "test".into(),
        ));
        let (first_permit, first_pending) = endpoint.admit().unwrap();
        let _second = endpoint.admit().unwrap();
        assert!(endpoint.admit().is_none());
        assert_eq!(stats.closed_at_handshake_limit(), 1);
        assert_eq!(stats.pending_handshakes(), 2);
        // The closed connection gave back the connection slot it took first.
        assert_eq!(endpoint.open.load(Ordering::Relaxed), 2);
        assert_eq!(stats.open_connections(), 2);

        // Finishing a handshake frees a slot while its connection stays open.
        drop(first_pending);
        assert_eq!(stats.pending_handshakes(), 1);
        let _third = endpoint.admit().unwrap();
        assert_eq!(stats.open_connections(), 3);

        // At the connection limit, the closed connection takes no handshake slot.
        assert!(endpoint.admit().is_none());
        assert_eq!(stats.closed_at_connection_limit(), 1);
        assert_eq!(endpoint.pending.load(Ordering::Relaxed), 2);
        drop(first_permit);
        assert!(endpoint.admit().is_none());
        assert_eq!(stats.closed_at_handshake_limit(), 2);
        assert_eq!(endpoint.open.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn log_limiter_allows_one_line_per_interval() {
        let log = LogLimiter::new();
        assert_eq!(log.hit(), Some(1));
        assert_eq!(log.hit(), None);
        assert_eq!(log.hit(), None);
        // Move the window forward as if a second had passed.
        log.next_ms.store(0, Ordering::Relaxed);
        assert_eq!(log.hit(), Some(3));
    }
}
