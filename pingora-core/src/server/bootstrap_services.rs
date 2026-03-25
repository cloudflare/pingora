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

#[cfg(unix)]
pub use super::transfer_fd::Fds;
use async_trait::async_trait;
use log::{debug, error, info};
use parking_lot::Mutex;
use std::sync::Arc;
use tokio::sync::broadcast;

#[cfg(feature = "sentry")]
use sentry::ClientOptions;

#[cfg(unix)]
use crate::server::ListenFds;

use crate::{
    prelude::Opt,
    server::{configuration::ServerConf, ExecutionPhase, ShutdownWatch},
    services::{background::BackgroundService, ServiceReadyNotifier},
};

/// Service that allows the bootstrap process to be delayed until after
/// dependencies are ready
pub struct BootstrapService {
    inner: Arc<Mutex<Bootstrap>>,
}

impl BootstrapService {
    pub fn new(inner: &Arc<Mutex<Bootstrap>>) -> Self {
        BootstrapService {
            inner: Arc::clone(inner),
        }
    }
}

/// Encapsulation of the data needed to bootstrap the server
pub struct Bootstrap {
    completed: bool,

    test: bool,
    upgrade: bool,

    upgrade_sock: String,

    execution_phase_watch: broadcast::Sender<ExecutionPhase>,

    #[cfg(unix)]
    listen_fds: ListenFds,

    #[cfg(feature = "sentry")]
    #[cfg_attr(docsrs, doc(cfg(feature = "sentry")))]
    /// The Sentry ClientOptions.
    ///
    /// Panics and other events sentry captures will be sent to this DSN **only
    /// in release mode**
    pub sentry: Option<ClientOptions>,

    /// The Sentry [`ClientInitGuard`](sentry::ClientInitGuard) returned by
    /// [`sentry::init`].
    ///
    /// This guard must be kept alive for the lifetime of the server, because
    /// dropping it flushes and disables the Sentry client.
    #[cfg(all(not(debug_assertions), feature = "sentry"))]
    sentry_guard: Option<sentry::ClientInitGuard>,
}

impl Bootstrap {
    pub fn new(
        options: &Option<Opt>,
        conf: &ServerConf,
        execution_phase_watch: &broadcast::Sender<ExecutionPhase>,
    ) -> Self {
        let (test, upgrade) = options
            .as_ref()
            .map(|opt| (opt.test, opt.upgrade))
            .unwrap_or_default();

        let upgrade_sock = conf.upgrade_sock.clone();

        Bootstrap {
            test,
            upgrade,
            upgrade_sock,
            #[cfg(unix)]
            listen_fds: Arc::new(Mutex::new(Fds::new())),
            execution_phase_watch: execution_phase_watch.clone(),
            completed: false,
            #[cfg(feature = "sentry")]
            sentry: None,
            #[cfg(all(not(debug_assertions), feature = "sentry"))]
            sentry_guard: None,
        }
    }

    #[cfg(feature = "sentry")]
    pub fn set_sentry_config(&mut self, sentry_config: Option<ClientOptions>) {
        self.sentry = sentry_config;
    }

    /// Initialize the Sentry client from the configured [`ClientOptions`] and
    /// store the resulting guard.
    ///
    /// The [`ClientOptions`] are preserved (not consumed) so that sentry can be
    /// re-initialized after daemonization, when the transport thread spawned by
    /// the previous [`sentry::init`] call is lost due to `fork()`.
    ///
    /// The resulting [`sentry::ClientInitGuard`] is stored in `self` so that it
    /// lives as long as the [`Bootstrap`] (and therefore the
    /// [`Server`](super::Server)), keeping the Sentry client active for the
    /// lifetime of the process.
    ///
    /// Sentry is only initialized in release builds; in debug builds this is a
    /// no-op.
    #[cfg(feature = "sentry")]
    pub(super) fn start_sentry(&mut self) {
        #[cfg(not(debug_assertions))]
        {
            self.sentry_guard = self.sentry.as_ref().map(|opts| sentry::init(opts.clone()));
        }
    }

    pub fn bootstrap(&mut self) {
        // already bootstrapped
        if self.completed {
            return;
        }

        info!("Bootstrap starting");

        self.execution_phase_watch
            .send(ExecutionPhase::Bootstrap)
            .ok();

        // Temporarily initialize sentry if it isn't already active, so that
        // errors during fd loading are captured. If sentry was already
        // initialized with a persistent guard by `Server::run()` (as in
        // the `bootstrap_as_a_service` path), we skip this to avoid
        // clobbering the persistent guard.
        #[cfg(all(not(debug_assertions), feature = "sentry"))]
        let _guard = if self.sentry_guard.is_none() {
            self.sentry.as_ref().map(|opts| sentry::init(opts.clone()))
        } else {
            None
        };

        if self.test {
            info!("Server Test passed, exiting");
            std::process::exit(0);
        }

        // load fds
        #[cfg(unix)]
        match self.load_fds(self.upgrade) {
            Ok(_) => {
                info!("Bootstrap done");
            }
            Err(e) => {
                // sentry log error on fd load failure
                #[cfg(all(not(debug_assertions), feature = "sentry"))]
                sentry::capture_error(&e);

                error!("Bootstrap failed on error: {:?}, exiting.", e);
                std::process::exit(1);
            }
        }

        self.completed = true;

        self.execution_phase_watch
            .send(ExecutionPhase::BootstrapComplete)
            .ok();
    }

    #[cfg(unix)]
    fn load_fds(&mut self, upgrade: bool) -> Result<(), nix::Error> {
        if upgrade {
            debug!("Trying to receive socks");
            let mut fds = Fds::new();
            fds.get_from_sock(self.upgrade_sock.as_str())?;
            // Mutate through the existing Arc so all clones held by services see the update.
            *self.listen_fds.lock() = fds;
        }
        Ok(())
    }

    #[cfg(unix)]
    pub fn get_fds(&self) -> ListenFds {
        self.listen_fds.clone()
    }
}

#[async_trait]
impl BackgroundService for BootstrapService {
    async fn start_with_ready_notifier(
        &self,
        _shutdown: ShutdownWatch,
        notifier: ServiceReadyNotifier,
    ) {
        self.inner.lock().bootstrap();
        notifier.notify_ready();
    }
}
