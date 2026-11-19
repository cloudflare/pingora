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

//! Cancel a streaming cache fill that no longer has anyone waiting for it.

use pingora_error::{Error, ErrorType};
use std::sync::OnceLock;
use tokio::sync::watch;

/// Error type of a request whose cache fill ended through a [`CacheFillCancellation`].
///
/// Recognize it with [`is_cache_fill_cancelled`], which also finds it under errors that wrap it.
pub const ERR_CACHE_FILL_CANCELLED: ErrorType = ErrorType::Custom("cache fill cancelled");

/// No-cache reason for a fill cancelled after its downstream was abandoned.
///
/// It is recorded as [`NoCacheReason::Custom`](crate::NoCacheReason::Custom), so disabling the
/// cache with it releases a write lock still held with the status
/// [`CacheKeyLock::custom_lock_status`](crate::lock::CacheKeyLock::custom_lock_status) returns.
/// The default gives [`LockStatus::TransientError`](crate::lock::LockStatus::TransientError),
/// as for a storage error: a cancellation ends this writer's fill, not the response's
/// cacheability, so a request waiting on the lock becomes the next writer. A lock that
/// overrides `custom_lock_status` should do the same. Only a cancellation that storage raises
/// while the lock is still held, for example from
/// [`Storage::get_miss_handler`](crate::storage::Storage::get_miss_handler), reaches the lock.
/// A fill the proxy cancelled through a [`CacheFillCancellation`] holds no lock by then: the
/// proxy keeps filling for an abandoned downstream only on a storage with streaming partial
/// writes, whose writer's lock [`HttpCache::set_miss_handler`](crate::HttpCache::set_miss_handler)
/// already released as [`LockStatus::Done`](crate::lock::LockStatus::Done).
pub const REASON_CACHE_FILL_CANCELLED: &str = "CacheFillCancelled";

/// Whether `error` comes from a cancelled cache fill.
///
/// Matches the root error type, so an [`ERR_CACHE_FILL_CANCELLED`] error that storage or
/// application code wrapped with [`Error::because`] is still recognized.
pub fn is_cache_fill_cancelled(error: &Error) -> bool {
    *error.root_etype() == ERR_CACHE_FILL_CANCELLED
}

/// Application-owned, sticky request to end a streaming cache fill early.
///
/// The proxy acts on it only after the downstream of the filling request has been abandoned,
/// because until then the fill is also that downstream's response, and only while a miss handler
/// of that fill is active: the cancellation reaches the proxy through
/// [`HandleMiss::cancellation_watcher`](crate::storage::HandleMiss::cancellation_watcher), so a
/// cancellation requested before any handler exists waits for one, and an attempt whose handler
/// is created and finished without the proxy asking for more upstream data is unaffected. When it
/// does apply, the proxy cancels its upstream read instead of waiting for more upstream data and
/// drops the miss handler without calling
/// [`HandleMiss::finish`](crate::storage::HandleMiss::finish), so storage treats the admission
/// as incomplete. The cancellation is observed the next time the proxy asks for upstream data:
/// data the upstream already handed over is processed first, so a response whose final upstream
/// bytes have already arrived is committed normally. Only a bounded amount of further data
/// follows, so an upstream that always has more ready cannot keep a cancelled fill running.
///
/// Because a cancellation reaches the proxy only through an active handler, nothing the proxy
/// decides before admission consults it, including whether to retry an upstream attempt. An
/// application that cancels before the first handler exists therefore still sees the attempts
/// the proxy would make anyway, and the cancellation first applies to one whose handler is active
/// while the proxy is still asking for upstream data. An application that must stop sooner owns
/// that in its own retry policy.
///
/// One value covers one logical cache fill for as long as the proxy keeps that fill running
/// after abandonment, including miss handlers and upstream attempts created later. The
/// abandonment callback runs once and the abandoned downstream cannot re-signal, so every
/// [`HandleMiss::cancellation_watcher`](crate::storage::HandleMiss::cancellation_watcher) of that
/// fill must return a watcher of the same value. Do not reuse it for an unrelated later request
/// merely because the cache key matches: its cancellation would end that request's fill too.
///
/// The watch channel is created on the first [`cancel`](Self::cancel) or
/// [`watcher`](Self::watcher) call, so embedding a value in every eligible fill costs nothing
/// while fills run to completion. Dropping the value without cancelling lets the fill run until
/// the upstream completes.
#[derive(Debug, Default)]
pub struct CacheFillCancellation {
    cancelled: OnceLock<watch::Sender<bool>>,
}

impl CacheFillCancellation {
    /// Create a value that has not requested cancellation.
    pub const fn new() -> Self {
        Self {
            cancelled: OnceLock::new(),
        }
    }

    fn channel(&self) -> &watch::Sender<bool> {
        self.cancelled.get_or_init(|| watch::Sender::new(false))
    }

    /// Request the cancellation. Returns `true` on the call that made the request.
    ///
    /// Later calls change nothing and wake no watcher.
    pub fn cancel(&self) -> bool {
        self.channel().send_if_modified(|cancelled| {
            if *cancelled {
                false
            } else {
                *cancelled = true;
                true
            }
        })
    }

    /// Whether the cancellation has been requested.
    pub fn is_cancelled(&self) -> bool {
        self.cancelled
            .get()
            .is_some_and(|cancelled| *cancelled.borrow())
    }

    /// Create the watcher a miss handler hands to the proxy.
    pub fn watcher(&self) -> CacheFillCancellationWatcher {
        CacheFillCancellationWatcher {
            cancelled: self.channel().subscribe(),
        }
    }
}

/// Proxy-side view of a [`CacheFillCancellation`], returned by
/// [`HandleMiss::cancellation_watcher`](crate::storage::HandleMiss::cancellation_watcher).
#[derive(Debug, Clone)]
pub struct CacheFillCancellationWatcher {
    cancelled: watch::Receiver<bool>,
}

impl CacheFillCancellationWatcher {
    /// Whether the cancellation has been requested.
    pub fn is_cancelled(&self) -> bool {
        *self.cancelled.borrow()
    }

    /// Resolve once the cancellation is requested.
    ///
    /// Stays pending forever when the [`CacheFillCancellation`] is dropped without cancelling.
    /// Cancel-safe: a request made while no future is polled is observed by the next call.
    pub async fn cancelled(&mut self) {
        if self
            .cancelled
            .wait_for(|cancelled| *cancelled)
            .await
            .is_err()
        {
            std::future::pending::<()>().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn cancel_reports_only_the_first_request() {
        let cancellation = CacheFillCancellation::new();
        let watcher = cancellation.watcher();
        assert!(!cancellation.is_cancelled());
        assert!(!watcher.is_cancelled());

        assert!(cancellation.cancel());
        assert!(!cancellation.cancel());
        assert!(cancellation.is_cancelled());
        assert!(watcher.is_cancelled());
    }

    #[test]
    fn cancellation_before_any_watcher_is_observed_later() {
        let cancellation = CacheFillCancellation::new();
        assert!(
            !cancellation.is_cancelled(),
            "an untouched value allocates nothing to read"
        );
        assert!(cancellation.cancel());
        assert!(cancellation.is_cancelled());
        assert!(
            cancellation.watcher().is_cancelled(),
            "a watcher created after the request must still see it"
        );
    }

    #[tokio::test]
    async fn repeated_cancel_does_not_wake_the_watcher_again() {
        let cancellation = CacheFillCancellation::new();
        let mut watcher = cancellation.watcher();
        assert!(cancellation.cancel());
        tokio::time::timeout(Duration::from_secs(1), watcher.cancelled())
            .await
            .expect("watcher did not observe the cancellation");
        // `cancelled()` marked the current value seen; a no-op cancel must not publish a new one.
        assert!(!cancellation.cancel());
        assert!(
            !watcher.cancelled.has_changed().unwrap(),
            "a repeated cancel woke the watcher"
        );
    }

    #[tokio::test]
    async fn watcher_resolves_for_a_cancellation_made_between_polls() {
        let cancellation = CacheFillCancellation::new();
        let mut watcher = cancellation.watcher();

        assert!(
            tokio::time::timeout(Duration::from_millis(20), watcher.cancelled())
                .await
                .is_err(),
            "watcher resolved before any cancellation was requested"
        );

        cancellation.cancel();
        tokio::time::timeout(Duration::from_secs(1), watcher.cancelled())
            .await
            .expect("watcher did not observe a cancellation requested while unpolled");
        // Observed cancellations stay observable.
        tokio::time::timeout(Duration::from_secs(1), watcher.cancelled())
            .await
            .expect("watcher forgot an observed cancellation");
    }

    #[tokio::test]
    async fn dropped_cancellation_never_resolves_the_watcher() {
        let cancellation = CacheFillCancellation::new();
        let mut watcher = cancellation.watcher();
        drop(cancellation);
        assert!(!watcher.is_cancelled());
        assert!(
            tokio::time::timeout(Duration::from_millis(20), watcher.cancelled())
                .await
                .is_err(),
            "a dropped value must not read as a cancellation"
        );
    }
}
