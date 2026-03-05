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

//! Distributed tracing helpers
//!
//! When the `trace` feature is enabled, this module re-exports the real
//! [`cf_rustracing`]/[`cf_rustracing_jaeger`] span types.
//!
//! When the `trace` feature is **disabled**, lightweight no-op shim types are
//! provided instead so that the rest of the crate compiles without pulling in
//! the tracing dependencies.

use std::time::SystemTime;

use crate::{CacheMeta, CachePhase, HitStatus};

// ---------------------------------------------------------------------------
// Real tracing implementation (feature = "trace")
// ---------------------------------------------------------------------------
#[cfg(feature = "trace")]
mod real {
    pub use cf_rustracing::tag::Tag;

    use cf_rustracing_jaeger::span::SpanContextState;

    pub type Span = cf_rustracing::span::Span<SpanContextState>;
    pub type SpanHandle = cf_rustracing::span::SpanHandle<SpanContextState>;
}

#[cfg(feature = "trace")]
pub use real::*;

// ---------------------------------------------------------------------------
// No-op shim types (feature = "trace" disabled)
// ---------------------------------------------------------------------------
#[cfg(not(feature = "trace"))]
mod noop {
    /// A no-op replacement for [`cf_rustracing::tag::Tag`].
    #[derive(Debug)]
    pub struct Tag {
        _priv: (),
    }

    impl Tag {
        /// Create a no-op tag.  All arguments are ignored.
        #[inline]
        pub fn new<N, V>(_name: N, _value: V) -> Self {
            Tag { _priv: () }
        }
    }

    /// A no-op replacement for a rustracing `Span`.
    #[derive(Debug)]
    pub struct Span {
        _priv: (),
    }

    impl Span {
        /// Return an inactive (no-op) span.
        #[inline]
        pub fn inactive() -> Self {
            Span { _priv: () }
        }

        /// Return a no-op handle.
        #[inline]
        pub fn handle(&self) -> SpanHandle {
            SpanHandle { _priv: () }
        }

        /// No-op: create a child span.
        #[inline]
        pub fn child<F>(&self, _name: &'static str, _f: F) -> Span
        where
            F: FnOnce(SpanOptionsPlaceholder) -> SpanOptionsPlaceholder,
        {
            Span::inactive()
        }

        /// No-op: set a single tag via a closure.
        #[inline]
        pub fn set_tag<F: FnOnce() -> Tag>(&self, _f: F) {}

        /// No-op: set multiple tags via a closure.
        #[inline]
        pub fn set_tags<F, I>(&self, _f: F)
        where
            F: FnOnce() -> I,
            I: IntoIterator<Item = Tag>,
        {
        }

        /// No-op: set a finish time.
        #[inline]
        pub fn set_finish_time<F: Fn() -> std::time::SystemTime>(&self, _f: F) {}
    }

    /// Placeholder type used in [`Span::child`] closure signatures so that
    /// existing call-sites like `span.child("name", |o| o.start())` compile.
    #[doc(hidden)]
    pub struct SpanOptionsPlaceholder {
        _priv: (),
    }

    impl SpanOptionsPlaceholder {
        /// No-op: mirrors `SpanOptions::start()`.
        #[inline]
        pub fn start(self) -> Self {
            self
        }
    }

    /// A no-op replacement for a rustracing `SpanHandle`.
    #[derive(Debug)]
    pub struct SpanHandle {
        _priv: (),
    }
}

#[cfg(not(feature = "trace"))]
pub use noop::*;

// ---------------------------------------------------------------------------
// Shared helpers (work with both real and no-op types)
// ---------------------------------------------------------------------------

/// Tag a span with metadata from a [`CacheMeta`].
pub fn tag_span_with_meta(span: &mut Span, meta: &CacheMeta) {
    fn ts2epoch(ts: SystemTime) -> f64 {
        ts.duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default() // should never overflow but be safe here
            .as_secs_f64()
    }
    let internal = &meta.0.internal;
    span.set_tags(|| {
        [
            Tag::new("created", ts2epoch(internal.created)),
            Tag::new("fresh_until", ts2epoch(internal.fresh_until)),
            Tag::new("updated", ts2epoch(internal.updated)),
            Tag::new("stale_if_error_sec", internal.stale_if_error_sec as i64),
            Tag::new(
                "stale_while_revalidate_sec",
                internal.stale_while_revalidate_sec as i64,
            ),
            Tag::new("variance", internal.variance.is_some()),
        ]
    });
}

#[derive(Debug)]
pub(crate) struct CacheTraceCTX {
    // parent span
    pub cache_span: Span,
    // only spans across multiple calls need to store here
    pub miss_span: Span,
    pub hit_span: Span,
}

impl CacheTraceCTX {
    pub fn new() -> Self {
        CacheTraceCTX {
            cache_span: Span::inactive(),
            miss_span: Span::inactive(),
            hit_span: Span::inactive(),
        }
    }

    pub fn enable(&mut self, cache_span: Span) {
        self.cache_span = cache_span;
    }

    pub fn get_cache_span(&self) -> SpanHandle {
        self.cache_span.handle()
    }

    #[inline]
    pub fn child(&self, name: &'static str) -> Span {
        self.cache_span.child(name, |o| o.start())
    }

    pub fn start_miss_span(&mut self) {
        self.miss_span = self.child("miss");
    }

    pub fn get_miss_span(&self) -> SpanHandle {
        self.miss_span.handle()
    }

    pub fn finish_miss_span(&mut self) {
        self.miss_span.set_finish_time(SystemTime::now);
    }

    pub fn start_hit_span(&mut self, phase: CachePhase, hit_status: HitStatus) {
        self.hit_span = self.child("hit");
        self.hit_span.set_tag(|| Tag::new("phase", phase.as_str()));
        self.hit_span
            .set_tag(|| Tag::new("status", hit_status.as_str()));
    }

    pub fn get_hit_span(&self) -> SpanHandle {
        self.hit_span.handle()
    }

    pub fn finish_hit_span(&mut self) {
        self.hit_span.set_finish_time(SystemTime::now);
    }

    pub fn log_meta_in_hit_span(&mut self, meta: &CacheMeta) {
        tag_span_with_meta(&mut self.hit_span, meta);
    }

    pub fn log_meta_in_miss_span(&mut self, meta: &CacheMeta) {
        tag_span_with_meta(&mut self.miss_span, meta);
    }
}
