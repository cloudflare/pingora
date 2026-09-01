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

//! Implement [BackgroundService] for [LoadBalancer]

use std::time::{Duration, Instant};

use super::{BackendIter, BackendSelection, HealthCheckService, LoadBalancer, LoadBalancerGroup};
use async_trait::async_trait;
use pingora_core::services::{background::BackgroundService, ServiceReadyNotifier};

/// Timing state shared by update and health-check loops.
struct TaskSchedule {
    frequency: Option<Duration>,
    next: Option<Instant>,
}

/// The independently optional tasks run by a background service.
struct BackgroundSchedule {
    update: Option<TaskSchedule>,
    health_check: Option<TaskSchedule>,
}

impl TaskSchedule {
    fn new(frequency: Option<Duration>, now: Instant) -> Self {
        Self {
            frequency,
            next: Some(now),
        }
    }

    fn is_due(&self, now: Instant) -> bool {
        self.next.is_some_and(|next| next <= now)
    }

    fn finished_at(&mut self, base: Instant) {
        self.next = self.frequency.map(|frequency| base + frequency);
    }

    fn next(&self) -> Option<Instant> {
        self.next
    }

    fn is_once(&self) -> bool {
        self.frequency.is_none()
    }
}

impl BackgroundSchedule {
    // 136 years, used when no scheduled work remains but another event can wake the service.
    const NEVER: Duration = Duration::from_secs(u32::MAX as u64);

    fn new(
        update_frequency: Option<Option<Duration>>,
        health_check_frequency: Option<Option<Duration>>,
        now: Instant,
    ) -> Self {
        Self {
            update: update_frequency.map(|frequency| TaskSchedule::new(frequency, now)),
            health_check: health_check_frequency.map(|frequency| TaskSchedule::new(frequency, now)),
        }
    }

    fn update_is_due(&self, now: Instant) -> bool {
        self.update
            .as_ref()
            .is_some_and(|schedule| schedule.is_due(now))
    }

    fn health_check_is_due(&self, now: Instant) -> bool {
        self.health_check
            .as_ref()
            .is_some_and(|schedule| schedule.is_due(now))
    }

    fn update_finished_at(&mut self, base: Instant) {
        if let Some(schedule) = self.update.as_mut() {
            schedule.finished_at(base);
        }
    }

    fn health_check_finished_at(&mut self, base: Instant) {
        if let Some(schedule) = self.health_check.as_mut() {
            schedule.finished_at(base);
        }
    }

    fn next_scheduled(&self) -> Option<Instant> {
        self.update
            .iter()
            .chain(self.health_check.iter())
            .filter_map(TaskSchedule::next)
            .min()
    }

    fn next(&self, now: Instant) -> Instant {
        self.next_scheduled().unwrap_or(now + Self::NEVER)
    }

    fn next_health_check(&self) -> Option<Instant> {
        self.health_check.as_ref().and_then(TaskSchedule::next)
    }

    fn health_check_is_once(&self) -> bool {
        self.health_check
            .as_ref()
            .is_some_and(TaskSchedule::is_once)
    }

    fn is_idle(&self) -> bool {
        self.next_scheduled().is_none()
    }
}

impl<S: Send + Sync + BackendSelection + 'static> LoadBalancer<S>
where
    S::Iter: BackendIter,
{
    pub async fn run(
        &self,
        shutdown: pingora_core::server::ShutdownWatch,
        mut ready_opt: Option<ServiceReadyNotifier>,
    ) -> () {
        let mut now = Instant::now();
        let mut schedule = BackgroundSchedule::new(
            Some(self.update_frequency),
            // Private views schedule probes here. Shared views are probed once by
            // their registry's HealthCheckService.
            self.backends
                .owns_health_checks()
                .then_some(self.health_check_frequency),
            now,
        );
        loop {
            if *shutdown.borrow() {
                return;
            }

            if schedule.update_is_due(now) {
                // TODO: log err
                let _ = self.update().await;
                schedule.update_finished_at(now);
            }

            // After the first update, discovery and selection setup will be
            // done, so dependent services can start receiving traffic.
            if let Some(ready) = ready_opt.take() {
                ServiceReadyNotifier::notify_ready(ready)
            }

            if schedule.health_check_is_due(now) {
                self.backends
                    .run_health_check(self.parallel_health_check)
                    .await;
                schedule.health_check_finished_at(now);
            }

            if schedule.is_idle() {
                return;
            }
            let to_wake = schedule.next(now);
            tokio::time::sleep_until(to_wake.into()).await;
            now = Instant::now();
        }
    }
}

/// Implement [BackgroundService] for [LoadBalancer]. For backward-compatibility
/// reasons, we implement both the `start` and `start_with_ready_notifier`
/// methods.
#[async_trait]
impl<S: Send + Sync + BackendSelection + 'static> BackgroundService for LoadBalancer<S>
where
    S::Iter: BackendIter,
{
    async fn start_with_ready_notifier(
        &self,
        shutdown: pingora_core::server::ShutdownWatch,
        ready: ServiceReadyNotifier,
    ) -> () {
        self.run(shutdown, Some(ready)).await
    }

    async fn start(&self, shutdown: pingora_core::server::ShutdownWatch) -> () {
        self.run(shutdown, None).await
    }
}

impl<S: Send + Sync + BackendSelection + 'static> LoadBalancerGroup<S>
where
    S::Config: 'static,
    S::Iter: BackendIter,
{
    /// Run discovery, selector rebuilds, and privately managed health checks
    /// until shutdown.
    pub async fn run(
        &self,
        mut shutdown: pingora_core::server::ShutdownWatch,
        mut ready_opt: Option<ServiceReadyNotifier>,
    ) {
        let mut now = Instant::now();
        let mut schedule = BackgroundSchedule::new(
            Some(self.update_frequency),
            // Private groups schedule probes here. Shared groups only consume the
            // health state maintained by their registry's HealthCheckService.
            self.backends()
                .owns_health_checks()
                .then_some(self.health_check_frequency),
            now,
        );
        let mut ready_generation = None;

        loop {
            if *shutdown.borrow() {
                return;
            }

            if schedule.update_is_due(now) {
                match self.update().await {
                    Ok(()) => {
                        // Until readiness is signaled, always target the latest
                        // backend generation. Pinning to the first successful
                        // update would let a burst of updates satisfy readiness
                        // with selectors that are already stale relative to the
                        // current membership.
                        if ready_opt.is_some() {
                            ready_generation = Some(self.backend_generation());
                        }
                    }
                    Err(error) => {
                        log::error!("load balancer group update failed: {error}");
                    }
                }
                schedule.update_finished_at(now);
            }

            if ready_generation.is_some_and(|generation| self.selectors_ready_for(generation)) {
                if let Some(ready) = ready_opt.take() {
                    // Every selector reached the target generation, so dependent
                    // services can start receiving traffic.
                    ServiceReadyNotifier::notify_ready(ready)
                }
            }

            if schedule.health_check_is_due(now) {
                self.backends()
                    .run_health_check(self.parallel_health_check)
                    .await;
                schedule.health_check_finished_at(now);
            }

            // Discovery and health checks have independent schedules. One-shot
            // discovery can still feed recurring checks over its last membership.
            if ready_opt.is_none() && schedule.is_idle() {
                // No readiness notification or periodic work remains.
                // Any queued selector rebuilds finish independently.
                return;
            }

            let to_wake = schedule.next(now);
            // Selector completion may satisfy startup readiness before the next
            // scheduled task. After readiness, rebuilds no longer wake this loop.
            tokio::select! {
                _ = tokio::time::sleep_until(to_wake.into()) => {}
                _ = self.rebuild_notified(), if ready_opt.is_some() => {} // re-trigger loop to notify waiters
                _ = shutdown.changed() => return, // exit on shutdown
            }
            now = Instant::now();
        }
    }
}

#[async_trait]
impl<S: Send + Sync + BackendSelection + 'static> BackgroundService for LoadBalancerGroup<S>
where
    S::Config: 'static,
    S::Iter: BackendIter,
{
    async fn start_with_ready_notifier(
        &self,
        shutdown: pingora_core::server::ShutdownWatch,
        ready: ServiceReadyNotifier,
    ) {
        self.run(shutdown, Some(ready)).await
    }

    async fn start(&self, shutdown: pingora_core::server::ShutdownWatch) {
        self.run(shutdown, None).await
    }
}

impl HealthCheckService {
    /// Run health checks for the shared registry until shutdown.
    pub async fn run(
        &self,
        mut shutdown: pingora_core::server::ShutdownWatch,
        mut ready_opt: Option<ServiceReadyNotifier>,
    ) {
        if !self.registry.has_health_check() {
            log::error!("HealthCheckService requires a configured HealthRegistry health check");
            // Keep the notifier alive so dependents cannot observe a false-ready
            // signal from its Drop implementation.
            if ready_opt.is_some() && !*shutdown.borrow() {
                _ = shutdown.changed().await;
            }
            return;
        }

        let mut schedule =
            BackgroundSchedule::new(None, Some(self.health_check_frequency), Instant::now());
        loop {
            if *shutdown.borrow() {
                return;
            }

            // One-shot mode (no frequency) runs a single pass and returns, so it
            // must observe at least one target first. Otherwise it would check
            // an empty registry, signal ready, and never look at targets
            // published afterwards. Periodic mode instead signals ready eagerly
            // and relies on later passes to pick up newly published targets.
            if schedule.health_check_is_once() && self.registry.target_count() == 0 {
                tokio::select! {
                    _ = self.registry.wait_for_targets() => {}
                    _ = shutdown.changed() => return,
                }
                // Both branches can become ready together; do not start a check
                // if `select!` chose the target while shutdown was also signaled.
                if *shutdown.borrow() {
                    return;
                }
            }

            self.registry
                .run_health_check(self.parallel_health_check)
                .await;
            if let Some(ready) = ready_opt.take() {
                // The initial health pass completed, so services depending on
                // this registry can start receiving traffic.
                ServiceReadyNotifier::notify_ready(ready);
            }
            schedule.health_check_finished_at(Instant::now());

            let Some(next_health_check) = schedule.next_health_check() else {
                // no more checks
                return;
            };
            loop {
                let has_targets = self.registry.target_count() > 0;
                tokio::select! {
                    _ = self.registry.wait_for_targets(), if !has_targets => break,
                    _ = tokio::time::sleep_until(next_health_check.into()), if has_targets => break,
                    _ = self.registry.wait_for_view_removal() => {},
                    _ = shutdown.changed() => return,
                }
            }
        }
    }
}

#[async_trait]
impl BackgroundService for HealthCheckService {
    async fn start_with_ready_notifier(
        &self,
        shutdown: pingora_core::server::ShutdownWatch,
        ready: ServiceReadyNotifier,
    ) {
        self.run(shutdown, Some(ready)).await
    }

    async fn start(&self, shutdown: pingora_core::server::ShutdownWatch) {
        self.run(shutdown, None).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schedule_without_tasks_is_idle() {
        let now = Instant::now();
        let mut schedule = BackgroundSchedule::new(None, None, now);

        assert!(!schedule.update_is_due(now));
        assert!(!schedule.health_check_is_due(now));
        schedule.update_finished_at(now);
        schedule.health_check_finished_at(now);
        assert!(schedule.is_idle());
        assert_eq!(schedule.next(now), now + BackgroundSchedule::NEVER);
        assert_eq!(schedule.next_health_check(), None);
        assert!(!schedule.health_check_is_once());
    }

    #[test]
    fn one_time_update_schedule_finishes_after_first_run() {
        let now = Instant::now();
        let mut schedule = BackgroundSchedule::new(Some(None), None, now);

        assert!(schedule.update_is_due(now));
        assert!(!schedule.is_idle());
        schedule.update_finished_at(now);
        assert!(!schedule.update_is_due(now));
        assert!(schedule.is_idle());
        assert_eq!(schedule.next(now), now + BackgroundSchedule::NEVER);
    }

    #[test]
    fn periodic_health_check_schedule_tracks_next_pass() {
        let now = Instant::now();
        let frequency = Duration::from_secs(5);
        let mut schedule = BackgroundSchedule::new(None, Some(Some(frequency)), now);

        assert!(schedule.health_check_is_due(now));
        assert!(!schedule.health_check_is_once());
        schedule.health_check_finished_at(now);
        assert!(!schedule.health_check_is_due(now));
        assert!(schedule.health_check_is_due(now + frequency));
        assert!(!schedule.is_idle());
        assert_eq!(schedule.next_health_check(), Some(now + frequency));
    }

    #[test]
    fn one_time_health_check_schedule_finishes_after_first_run() {
        let now = Instant::now();
        let mut schedule = BackgroundSchedule::new(None, Some(None), now);

        assert!(schedule.health_check_is_once());
        assert!(schedule.health_check_is_due(now));
        schedule.health_check_finished_at(now);
        assert!(!schedule.health_check_is_due(now));
        assert_eq!(schedule.next_health_check(), None);
        assert!(schedule.is_idle());
        assert_eq!(schedule.next(now), now + BackgroundSchedule::NEVER);
    }

    #[test]
    fn schedule_uses_earliest_task_deadline() {
        let now = Instant::now();
        let update_frequency = Duration::from_secs(10);
        let health_check_frequency = Duration::from_secs(5);
        let mut schedule = BackgroundSchedule::new(
            Some(Some(update_frequency)),
            Some(Some(health_check_frequency)),
            now,
        );

        schedule.update_finished_at(now);
        schedule.health_check_finished_at(now);
        assert_eq!(schedule.next(now), now + health_check_frequency);
        assert!(!schedule.update_is_due(now + health_check_frequency));
        assert!(schedule.health_check_is_due(now + health_check_frequency));
        assert!(schedule.update_is_due(now + update_frequency));
    }
}
