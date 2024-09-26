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

//! The background service
//!
//! A [BackgroundService] can be run as part of a Pingora application to add supporting logic that
//! exists outside of the request/response lifecycle.
//! Examples might include service discovery (load balancing) and background updates such as
//! push-style metrics.

use async_trait::async_trait;
use std::sync::Arc;

use super::Service;
#[cfg(unix)]
use crate::server::ListenFds;
use crate::server::ShutdownWatch;

/// The background service interface
#[async_trait]
pub trait BackgroundService {
    /// This function is called when the pingora server tries to start all the
    /// services. The background service can return at anytime or wait for the
    /// `shutdown` signal.
    async fn start(&self, mut shutdown: ShutdownWatch);
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
impl<A> Service for GenBackgroundService<A>
where
    A: BackgroundService + Send + Sync + 'static,
{
    async fn start_service(
        &mut self,
        #[cfg(unix)] _fds: Option<ListenFds>,
        shutdown: ShutdownWatch,
    ) {
        self.task.start(shutdown).await;
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn threads(&self) -> Option<usize> {
        self.threads
    }
}

// Helper function to create a background service with a human readable name
pub fn background_service<SV>(name: &str, task: SV) -> GenBackgroundService<SV> {
    GenBackgroundService::new(format!("BG {name}"), Arc::new(task))
}
