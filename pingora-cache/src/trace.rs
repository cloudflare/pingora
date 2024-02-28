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

//! Distributed tracing helpers

use rustracing_jaeger::span::SpanContextState;
use std::time::SystemTime;

use crate::{CacheMeta, CachePhase, HitStatus};

pub use rustracing::tag::Tag;

pub type Span = rustracing::span::Span<SpanContextState>;
pub type SpanHandle = rustracing::span::SpanHandle<SpanContextState>;

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

    pub fn finish_hit_span(&mut self) {
        self.hit_span.set_finish_time(SystemTime::now);
    }

    pub fn log_meta(&mut self, meta: &CacheMeta) {
        fn ts2epoch(ts: SystemTime) -> f64 {
            ts.duration_since(SystemTime::UNIX_EPOCH)
                .unwrap_or_default() // should never overflow but be safe here
                .as_secs_f64()
        }
        let internal = &meta.0.internal;
        self.hit_span.set_tags(|| {
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
}
