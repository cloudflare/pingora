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

/// Possible downstream states during request multiplexing
#[derive(Debug, Clone, Copy)]
pub(crate) enum DownstreamStateMachine {
    /// more request (body) to read
    Reading,
    /// no more data to read
    ReadingFinished,
    /// body was pre-buffered before upstream connection, skip all downstream polling
    #[cfg(feature = "early_body_buffer")]
    PreBuffered,
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
        #[cfg(feature = "early_body_buffer")]
        {
            !matches!(self, Self::Errored | Self::PreBuffered)
        }
        #[cfg(not(feature = "early_body_buffer"))]
        {
            !matches!(self, Self::Errored)
        }
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
