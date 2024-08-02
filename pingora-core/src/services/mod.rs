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

//! The service interface
//!
//! A service to the pingora server is just something runs forever until the server is shutting
//! down.
//!
//! Two types of services are particularly useful
//! - services that are listening to some (TCP) endpoints
//! - services that are just running in the background.

use async_trait::async_trait;

#[cfg(unix)]
use crate::server::ListenFds;
use crate::server::ShutdownWatch;

pub mod background;
pub mod listening;

/// The service interface
#[async_trait]
pub trait Service: Sync + Send {
    /// This function will be called when the server is ready to start the service.
    ///
    /// - `fds`: a collection of listening file descriptors. During zero downtime restart
    /// the `fds` would contain the listening sockets passed from the old service, services should
    /// take the sockets they need to use then. If the sockets the service looks for don't appear in
    /// the collection, the service should create its own listening sockets and then put them into
    /// the collection in order for them to be passed to the next server.
    /// - `shutdown`: the shutdown signal this server would receive.
    async fn start_service(
        &mut self,
        #[cfg(unix)] fds: Option<ListenFds>,
        mut shutdown: ShutdownWatch,
    );

    /// The name of the service, just for logging and naming the threads assigned to this service
    ///
    /// Note that due to the limit of the underlying system, only the first 16 chars will be used
    fn name(&self) -> &str;

    /// The preferred number of threads to run this service
    ///
    /// If `None`, the global setting will be used
    fn threads(&self) -> Option<usize> {
        None
    }
}
