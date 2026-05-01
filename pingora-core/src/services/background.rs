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

//! The background service
//!
//! A [BackgroundService] can be run as part of a Pingora application to add supporting logic that
//! exists outside of the request/response lifecycle.
//! Examples might include service discovery (load balancing) and background updates such as
//! push-style metrics.

use async_trait::async_trait;
use std::sync::Arc;

use super::{ServiceReadyNotifier, ServiceWithDependents};
#[cfg(unix)]
use crate::server::ListenFds;
use crate::server::ShutdownWatch;

/// The background service interface
///
/// You can implement a background service with or without the ready notifier,
/// but you shouldn't implement both. Under the hood, the pingora service will
/// call the `start_with_ready_notifier` function. By default this function will
/// call the regular `start` function.
#[async_trait]
pub trait BackgroundService {
    /// This function is called when the pingora server tries to start all the
    /// services. The background service should signal readiness by calling
    /// `ready_notifier.notify_ready()` once initialization is complete.
    /// The service can return at anytime or wait for the `shutdown` signal.
    ///
    /// By default this method will immediately signal readiness and call
    /// through to the regular `start` function
    async fn start_with_ready_notifier(
        &self,
        shutdown: ShutdownWatch,
        ready_notifier: ServiceReadyNotifier,
    ) {
        ready_notifier.notify_ready();
        self.start(shutdown).await;
    }

    /// This function is called when the pingora server tries to start all the
    /// services. The background service can return at anytime or wait for the
    /// `shutdown` signal.
    async fn start(&self, mut _shutdown: ShutdownWatch) {}
}

/// A generic type of background service
pub struct GenBackgroundService<A> {
    // Name of the service
    name: String,
    // Task the service will execute
    task: Arc<A>,
    /// The number of threads. Default is 1
    pub threads: Option<usize>,
}

impl<A> GenBackgroundService<A> {
    /// Generates a background service that can run in the pingora runtime
    pub fn new(name: String, task: Arc<A>) -> Self {
        Self {
            name,
            task,
            threads: Some(1),
        }
    }

    /// Return the task behind [Arc] to be shared other logic.
    pub fn task(&self) -> Arc<A> {
        self.task.clone()
    }
}

#[async_trait]
impl<A> ServiceWithDependents for GenBackgroundService<A>
where
    A: BackgroundService + Send + Sync + 'static,
{
    // Use default start_service implementation which signals ready immediately
    // and then calls start_service

    async fn start_service(
        &mut self,
        #[cfg(unix)] _fds: Option<ListenFds>,
        shutdown: ShutdownWatch,
        _listeners_per_fd: usize,
        ready: ServiceReadyNotifier,
    ) {
        self.task.start_with_ready_notifier(shutdown, ready).await;
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn threads(&self) -> Option<usize> {
        self.threads
    }
}

/// Helper function to create a background service with a human readable name
pub fn background_service<SV>(name: &str, task: SV) -> GenBackgroundService<SV> {
    GenBackgroundService::new(format!("BG {name}"), Arc::new(task))
}
