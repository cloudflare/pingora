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

use super::Service;
use crate::protocols::http::custom::server::Session as CustomServerSession;
use crate::server::{ExecutionPhase, Server};
use parking_lot::Mutex;
use std::sync::Arc;
use tokio::sync::broadcast::{self, error::TryRecvError};

impl<A, DS> Service<A, DS>
where
    DS: CustomServerSession,
{
    /// Keep accepting connections through a graceful terminate (SIGTERM) until `server` shuts its
    /// runtimes down, instead of closing the listeners when shutdown starts.
    ///
    /// Meant for endpoints that must stay reachable while the server drains, such as readiness
    /// or metrics. The listeners still close on a graceful upgrade (SIGQUIT), so the new process
    /// takes over the transferred sockets. `server` must be the server this service is added to.
    ///
    /// Only accepting is kept. The application still owns its drain behavior:
    /// - On terminate this bypasses the normal cleanup path: runtime shutdown drops the
    ///   listeners, so [`ServerApp::cleanup`](crate::apps::ServerApp::cleanup) is not called, and
    ///   an HTTP proxy's `Session::is_process_shutting_down()` stays `false`. Observe shutdown
    ///   separately, e.g. through a background service's
    ///   [`ShutdownWatch`](crate::server::ShutdownWatch).
    /// - Keepalive, response status, and readiness reporting are unchanged.
    ///
    /// A service that serves regular traffic should not opt in: it would keep admitting work that
    /// runtime shutdown then drops.
    pub fn keep_listening_on_terminate(&mut self, server: &Server) {
        self.keep_listening_with(server.watch_execution_phase());
    }

    pub(super) fn keep_listening_with(&mut self, phases: broadcast::Receiver<ExecutionPhase>) {
        self.terminate_phases = Some(Arc::new(TerminatePhases(Mutex::new(Decision {
            phases,
            upgrade: None,
        }))));
    }
}

/// Shared by a service's endpoints so they all make the same shutdown decision.
pub(super) struct TerminatePhases(Mutex<Decision>);

struct Decision {
    phases: broadcast::Receiver<ExecutionPhase>,
    upgrade: Option<bool>,
}

impl TerminatePhases {
    /// Whether the shutdown just broadcast is a graceful upgrade.
    ///
    /// The server sends the upgrade phases before it broadcasts shutdown, so they are already
    /// queued. Lost phases or a closed channel count as an upgrade, the default close.
    pub(super) fn is_graceful_upgrade(&self) -> bool {
        let mut decision = self.0.lock();
        if let Some(upgrade) = decision.upgrade {
            return upgrade;
        }
        let upgrade = loop {
            match decision.phases.try_recv() {
                Ok(
                    ExecutionPhase::GracefulUpgradeTransferringFds
                    | ExecutionPhase::GracefulUpgradeCloseTimeout,
                )
                | Err(TryRecvError::Lagged(_) | TryRecvError::Closed) => break true,
                Ok(_) => {}
                Err(TryRecvError::Empty) => break false,
            }
        };
        decision.upgrade = Some(upgrade);
        upgrade
    }
}
