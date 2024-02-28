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

//! Implement [BackgroundService] for [LoadBalancer]

use std::time::{Duration, Instant};

use super::{BackendIter, BackendSelection, LoadBalancer};
use async_trait::async_trait;
use pingora_core::services::background::BackgroundService;

#[async_trait]
impl<S: Send + Sync + BackendSelection + 'static> BackgroundService for LoadBalancer<S>
where
    S::Iter: BackendIter,
{
    async fn start(&self, shutdown: pingora_core::server::ShutdownWatch) -> () {
        // 136 years
        const NEVER: Duration = Duration::from_secs(u32::MAX as u64);
        let mut now = Instant::now();
        // run update and health check once
        let mut next_update = now;
        let mut next_health_check = now;
        loop {
            if *shutdown.borrow() {
                return;
            }

            if next_update <= now {
                // TODO: log err
                let _ = self.update().await;
                next_update = now + self.update_frequency.unwrap_or(NEVER);
            }

            if next_health_check <= now {
                self.backends
                    .run_health_check(self.parallel_health_check)
                    .await;
                next_health_check = now + self.health_check_frequency.unwrap_or(NEVER);
            }

            if self.update_frequency.is_none() && self.health_check_frequency.is_none() {
                return;
            }
            let to_wake = std::cmp::min(next_update, next_health_check);
            tokio::time::sleep_until(to_wake.into()).await;
            now = Instant::now();
        }
    }
}
