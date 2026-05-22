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

use super::{BackendIter, BackendSelection, LoadBalancer};
use async_trait::async_trait;
use pingora_core::{
    server::ShutdownWatch,
    services::{background::BackgroundService, ServiceReadyNotifier},
};

async fn shutdown_changed(shutdown: &mut ShutdownWatch) -> bool {
    match shutdown.changed().await {
        Ok(()) => *shutdown.borrow(),
        Err(_) => true,
    }
}

impl<S: Send + Sync + BackendSelection + 'static> LoadBalancer<S>
where
    S::Iter: BackendIter,
{
    pub async fn run(
        &self,
        mut shutdown: ShutdownWatch,
        mut ready_opt: Option<ServiceReadyNotifier>,
    ) -> () {
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
                tokio::select! {
                    biased;
                    is_shutdown = shutdown_changed(&mut shutdown) => {
                        if is_shutdown {
                            return;
                        }
                        continue;
                    }
                    update = self.update() => {
                        let _ = update;
                        next_update = now + self.update_frequency.unwrap_or(NEVER);
                    }
                }
            }

            // After the first update, discovery and selection setup will be
            // done, so we will notify dependents
            if let Some(ready) = ready_opt.take() {
                ServiceReadyNotifier::notify_ready(ready)
            }

            if next_health_check <= now {
                tokio::select! {
                    biased;
                    is_shutdown = shutdown_changed(&mut shutdown) => {
                        if is_shutdown {
                            return;
                        }
                        continue;
                    }
                    _ = self.backends.run_health_check(self.parallel_health_check) => {
                        next_health_check = now + self.health_check_frequency.unwrap_or(NEVER);
                    }
                }
            }

            if self.update_frequency.is_none() && self.health_check_frequency.is_none() {
                return;
            }
            let to_wake = std::cmp::min(next_update, next_health_check);
            tokio::select! {
                biased;
                is_shutdown = shutdown_changed(&mut shutdown) => {
                    if is_shutdown {
                        return;
                    }
                }
                _ = tokio::time::sleep_until(to_wake.into()) => {}
            }
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

#[cfg(test)]
mod tests {
    use std::collections::{BTreeSet, HashMap};
    use std::future;
    use std::sync::Arc;

    use async_trait::async_trait;
    use pingora_error::Result;
    use tokio::sync::{watch, Notify};

    use super::*;
    use crate::discovery::ServiceDiscovery;
    use crate::health_check::HealthCheck;
    use crate::selection;
    use crate::{Backend, Backends};

    struct NotifyingDiscovery {
        notify: Arc<Notify>,
    }

    #[async_trait]
    impl ServiceDiscovery for NotifyingDiscovery {
        async fn discover(&self) -> Result<(BTreeSet<Backend>, HashMap<u64, bool>)> {
            self.notify.notify_one();
            Ok((BTreeSet::new(), HashMap::new()))
        }
    }

    struct PendingDiscovery {
        notify: Arc<Notify>,
    }

    #[async_trait]
    impl ServiceDiscovery for PendingDiscovery {
        async fn discover(&self) -> Result<(BTreeSet<Backend>, HashMap<u64, bool>)> {
            self.notify.notify_one();
            future::pending().await
        }
    }

    struct PendingHealthCheck {
        notify: Arc<Notify>,
        drop_notify: Arc<Notify>,
    }

    #[async_trait]
    impl HealthCheck for PendingHealthCheck {
        async fn check(&self, _target: &Backend) -> Result<()> {
            struct NotifyOnDrop(Arc<Notify>);

            impl Drop for NotifyOnDrop {
                fn drop(&mut self) {
                    self.0.notify_one();
                }
            }

            let _notify_on_drop = NotifyOnDrop(self.drop_notify.clone());
            self.notify.notify_one();
            future::pending().await
        }

        fn health_threshold(&self, _success: bool) -> usize {
            1
        }
    }

    async fn assert_run_exits_on_shutdown(
        lb: LoadBalancer<selection::RoundRobin>,
        notify: Arc<Notify>,
    ) {
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let handle = tokio::spawn(async move {
            lb.run(shutdown_rx, None).await;
        });

        notify.notified().await;
        shutdown_tx.send(true).unwrap();

        tokio::time::timeout(Duration::from_secs(1), handle)
            .await
            .expect("background service should observe shutdown promptly")
            .expect("background service task should not panic");
    }

    #[tokio::test]
    async fn run_returns_when_shutdown_while_sleeping() {
        let notify = Arc::new(Notify::new());
        let discovery = NotifyingDiscovery {
            notify: notify.clone(),
        };
        let mut lb = LoadBalancer::<selection::RoundRobin>::from_backends(Backends::new(Box::new(
            discovery,
        )));
        lb.update_frequency = Some(Duration::from_secs(60));

        assert_run_exits_on_shutdown(lb, notify).await;
    }

    #[tokio::test]
    async fn run_returns_when_shutdown_while_updating() {
        let notify = Arc::new(Notify::new());
        let discovery = PendingDiscovery {
            notify: notify.clone(),
        };
        let lb = LoadBalancer::<selection::RoundRobin>::from_backends(Backends::new(Box::new(
            discovery,
        )));

        assert_run_exits_on_shutdown(lb, notify).await;
    }

    #[tokio::test]
    async fn run_returns_when_shutdown_while_health_checking() {
        let notify = Arc::new(Notify::new());
        let drop_notify = Arc::new(Notify::new());
        let mut lb =
            LoadBalancer::<selection::RoundRobin>::try_from_iter(["127.0.0.1:80"]).unwrap();
        lb.set_health_check(Box::new(PendingHealthCheck {
            notify: notify.clone(),
            drop_notify: drop_notify.clone(),
        }));

        assert_run_exits_on_shutdown(lb, notify).await;
        tokio::time::timeout(Duration::from_secs(1), drop_notify.notified())
            .await
            .expect("pending health check should be cancelled");
    }

    #[tokio::test]
    async fn run_aborts_parallel_health_check_on_shutdown() {
        let notify = Arc::new(Notify::new());
        let drop_notify = Arc::new(Notify::new());
        let mut lb =
            LoadBalancer::<selection::RoundRobin>::try_from_iter(["127.0.0.1:80"]).unwrap();
        lb.parallel_health_check = true;
        lb.set_health_check(Box::new(PendingHealthCheck {
            notify: notify.clone(),
            drop_notify: drop_notify.clone(),
        }));

        assert_run_exits_on_shutdown(lb, notify).await;
        tokio::time::timeout(Duration::from_secs(1), drop_notify.notified())
            .await
            .expect("parallel health check task should be aborted");
    }
}
