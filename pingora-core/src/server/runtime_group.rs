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

//! Runtime groups: services that share one runtime.

use daggy::NodeIndex;
use log::warn;
use pingora_runtime::{BlockingPoolOpts, RuntimeOpts};
use std::collections::HashMap;
use std::time::Duration;

use crate::services::ServiceWithDependents;

/// Settings for a runtime group registered with
/// [`Server::add_runtime_group`](super::Server::add_runtime_group).
///
/// Every setting is optional. A setting left unset is resolved from the group's members and the
/// server configuration, as described on
/// [`Server::add_runtime_group`](super::Server::add_runtime_group).
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct RuntimeGroupConf {
    pub(super) threads: Option<usize>,
    pub(super) work_stealing: Option<bool>,
    pub(super) runtime_opts: Option<RuntimeOpts>,
    pub(super) max_blocking_threads: Option<usize>,
    pub(super) blocking_thread_keep_alive: Option<Duration>,
}

impl RuntimeGroupConf {
    /// Create a group configuration with no settings, so that every setting is resolved from the
    /// group's members and the server configuration.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the number of worker threads of the group's runtime.
    ///
    /// The members' own [`threads()`](ServiceWithDependents::threads) are then ignored.
    pub fn threads(mut self, threads: usize) -> Self {
        self.threads = Some(threads);
        self
    }

    /// Set whether the group's runtime uses work stealing, instead of
    /// [`ServerConf::work_stealing`](super::configuration::ServerConf::work_stealing).
    pub fn work_stealing(mut self, enabled: bool) -> Self {
        self.work_stealing = Some(enabled);
        self
    }

    /// Set the runtime options of the group's runtime.
    ///
    /// The members' own
    /// [`runtime_opts_override()`](ServiceWithDependents::runtime_opts_override) are then ignored.
    pub fn runtime_opts(mut self, opts: RuntimeOpts) -> Self {
        self.runtime_opts = Some(opts);
        self
    }

    /// Set the maximum number of blocking threads of the group's runtime, instead of
    /// [`ServerConf::max_blocking_threads`](super::configuration::ServerConf::max_blocking_threads).
    ///
    /// The limit applies to each runtime. Services that each had a runtime of their own share one
    /// blocking pool once they are grouped, so raise the limit to keep their combined capacity.
    pub fn max_blocking_threads(mut self, max: usize) -> Self {
        self.max_blocking_threads = Some(max);
        self
    }

    /// Set how long idle blocking threads of the group's runtime are kept alive, instead of
    /// [`ServerConf::blocking_threads_ttl_seconds`](super::configuration::ServerConf::blocking_threads_ttl_seconds).
    pub fn blocking_thread_keep_alive(mut self, keep_alive: Duration) -> Self {
        self.blocking_thread_keep_alive = Some(keep_alive);
        self
    }
}

/// Server-wide runtime settings that apply when neither a group nor a service overrides them.
pub(super) struct RuntimeDefaults<'a> {
    pub threads: usize,
    pub work_stealing: bool,
    pub blocking_opts: &'a BlockingPoolOpts,
    pub runtime_opts: &'a RuntimeOpts,
}

/// The runtimes to build, in the order their first service starts.
pub(super) struct RuntimePlan {
    pub runtimes: Vec<PlannedRuntime>,
    /// Index into `runtimes` for every planned service.
    pub runtime_of: HashMap<NodeIndex, usize>,
}

/// One runtime to build: a named group, or a single ungrouped service.
pub(super) struct PlannedRuntime {
    /// The group name, or the service name for an ungrouped service.
    pub name: String,
    pub grouped: bool,
    pub members: Vec<String>,
    pub threads: usize,
    pub work_stealing: bool,
    pub blocking_opts: BlockingPoolOpts,
    pub runtime_opts: RuntimeOpts,
}

impl PlannedRuntime {
    /// The name used in shutdown logs.
    pub fn label(&self) -> String {
        if self.grouped {
            format!("{} (group of {})", self.name, self.members.join(", "))
        } else {
            self.name.clone()
        }
    }
}

struct Member {
    name: String,
    threads: Option<usize>,
    runtime_opts: Option<RuntimeOpts>,
    listen_addresses: Option<Vec<String>>,
}

/// Decide which runtime each service runs on and how each runtime is built.
///
/// `services` must be in startup order. An ungrouped service gets a runtime of its own, named
/// after it and built from its own `threads()` and runtime options override.
pub(super) fn plan_runtimes<'a>(
    services: impl IntoIterator<Item = (NodeIndex, &'a dyn ServiceWithDependents)>,
    registry: &HashMap<String, RuntimeGroupConf>,
    defaults: &RuntimeDefaults,
) -> Result<RuntimePlan, String> {
    let mut runtimes = Vec::new();
    let mut runtime_of = HashMap::new();
    let mut group_index: HashMap<String, usize> = HashMap::new();
    let mut group_members: Vec<(usize, Vec<Member>)> = Vec::new();

    for (id, service) in services {
        let name = service.name().to_string();
        let runtime_opts = service.runtime_opts_override(defaults.runtime_opts);
        let Some(group) = service.runtime_group() else {
            runtime_of.insert(id, runtimes.len());
            runtimes.push(PlannedRuntime {
                members: vec![name.clone()],
                name,
                grouped: false,
                threads: service.threads().unwrap_or(defaults.threads),
                work_stealing: defaults.work_stealing,
                blocking_opts: defaults.blocking_opts.clone(),
                runtime_opts: runtime_opts.unwrap_or_else(|| defaults.runtime_opts.clone()),
            });
            continue;
        };
        let index = *group_index.entry(group.to_string()).or_insert_with(|| {
            group_members.push((runtimes.len(), Vec::new()));
            // Resolved below, once every member is known.
            runtimes.push(PlannedRuntime {
                name: group.to_string(),
                grouped: true,
                members: Vec::new(),
                threads: 0,
                work_stealing: defaults.work_stealing,
                blocking_opts: BlockingPoolOpts::default(),
                runtime_opts: RuntimeOpts::default(),
            });
            group_members.len() - 1
        });
        runtime_of.insert(id, group_members[index].0);
        group_members[index].1.push(Member {
            name,
            threads: service.threads(),
            runtime_opts,
            listen_addresses: service.listen_addresses(),
        });
    }

    for (index, members) in group_members {
        let planned = &mut runtimes[index];
        let conf = registry.get(&planned.name).cloned().unwrap_or_default();
        resolve_group(planned, conf, members, defaults)?;
    }
    for name in registry.keys() {
        if !runtimes.iter().any(|r| r.grouped && &r.name == name) {
            warn!("Runtime group '{name}' is registered but no service runs in it");
        }
    }
    for group in runtimes.iter().filter(|r| r.grouped) {
        if runtimes.iter().any(|r| !r.grouped && r.name == group.name) {
            warn!(
                "Runtime group '{0}' has the same name as service '{0}', which has a runtime of \
                 its own; their worker threads and dial9 runtimes share that name",
                group.name
            );
        }
    }

    Ok(RuntimePlan {
        runtimes,
        runtime_of,
    })
}

fn resolve_group(
    planned: &mut PlannedRuntime,
    conf: RuntimeGroupConf,
    members: Vec<Member>,
    defaults: &RuntimeDefaults,
) -> Result<(), String> {
    let group = planned.name.as_str();
    let names = members.iter().map(|m| m.name.as_str()).collect::<Vec<_>>();

    // Listeners share descriptors by address through the server's listening-fd table: a second
    // listener on an address adopts the first one's descriptor without duplicating it, whether
    // that descriptor was inherited or freshly bound. Two members doing so in one runtime would
    // register the same descriptor with its reactor twice. Addresses are compared as exact
    // strings, as the table does.
    let mut listeners: HashMap<&str, usize> = HashMap::new();
    for (index, member) in members.iter().enumerate() {
        for addr in member.listen_addresses.iter().flatten() {
            if let Some(other) = listeners.insert(addr.as_str(), index) {
                if other != index {
                    return Err(format!(
                        "services '{}' and '{}' in runtime group '{group}' both listen on \
                         '{addr}'; listeners on one address share a descriptor through the \
                         server's listening-fd table, so they must run in separate runtimes or \
                         listen on distinct addresses",
                        members[other].name, member.name
                    ));
                }
            }
        }
    }

    planned.threads = match conf.threads {
        Some(threads) => {
            for member in &members {
                if let Some(own) = member.threads.filter(|own| *own != threads) {
                    warn!(
                        "Service '{}' asks for {own} threads, but runtime group '{group}' is \
                         configured with {threads}",
                        member.name
                    );
                }
            }
            threads
        }
        None => {
            if let Some(member) = members.iter().find(|m| m.threads == Some(0)) {
                return Err(format!(
                    "runtime group '{group}' must have at least one thread, but service '{}' \
                     asks for 0",
                    member.name
                ));
            }
            members
                .iter()
                .map(|m| m.threads.unwrap_or(defaults.threads))
                .max()
                .unwrap_or(defaults.threads)
        }
    };
    if planned.threads == 0 {
        return Err(format!(
            "runtime group '{group}' must have at least one thread (services {:?})",
            members.iter().map(|m| m.name.as_str()).collect::<Vec<_>>()
        ));
    }

    let overrides = members
        .iter()
        .filter(|m| m.runtime_opts.is_some())
        .collect::<Vec<_>>();
    planned.runtime_opts = match conf.runtime_opts {
        Some(opts) => {
            for member in &overrides {
                warn!(
                    "Runtime options override of service '{}' is ignored: runtime group '{group}' \
                     is configured with its own runtime options",
                    member.name
                );
            }
            opts
        }
        // `RuntimeOpts` cannot be compared, so two overrides are never merged or picked between.
        None if overrides.len() > 1 => {
            return Err(format!(
                "services {:?} in runtime group '{group}' each override the runtime options; \
                 register the group's runtime options with Server::add_runtime_group instead",
                overrides
                    .iter()
                    .map(|m| m.name.as_str())
                    .collect::<Vec<_>>()
            ));
        }
        None => overrides
            .first()
            .and_then(|member| member.runtime_opts.clone())
            .unwrap_or_else(|| defaults.runtime_opts.clone()),
    };

    planned.work_stealing = conf.work_stealing.unwrap_or(defaults.work_stealing);
    planned.blocking_opts = BlockingPoolOpts {
        max_threads: conf
            .max_blocking_threads
            .or(defaults.blocking_opts.max_threads),
        thread_keep_alive: conf
            .blocking_thread_keep_alive
            .or(defaults.blocking_opts.thread_keep_alive),
    };
    planned.members = names.into_iter().map(str::to_string).collect();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use crate::server::ListenFds;
    use crate::server::ShutdownWatch;
    use crate::services::ServiceReadyNotifier;
    use async_trait::async_trait;
    use pingora_runtime::RuntimeMetricsOpts;

    /// A service that only reports its runtime settings.
    #[derive(Default)]
    struct Spec {
        name: &'static str,
        group: Option<&'static str>,
        threads: Option<usize>,
        /// Overrides the runtime options, marked with this histogram bucket count.
        opts_marker: Option<usize>,
        addrs: Option<Vec<&'static str>>,
    }

    #[async_trait]
    impl ServiceWithDependents for Spec {
        async fn start_service(
            &mut self,
            #[cfg(unix)] _fds: Option<ListenFds>,
            _shutdown: ShutdownWatch,
            _listeners_per_fd: usize,
            _ready_notifier: ServiceReadyNotifier,
        ) {
        }
        fn name(&self) -> &str {
            self.name
        }
        fn threads(&self) -> Option<usize> {
            self.threads
        }
        fn runtime_group(&self) -> Option<&str> {
            self.group
        }
        fn runtime_opts_override(&self, global: &RuntimeOpts) -> Option<RuntimeOpts> {
            self.opts_marker.map(|marker| marked(marker, global))
        }
        fn listen_addresses(&self) -> Option<Vec<String>> {
            let addrs = self.addrs.as_ref()?;
            Some(addrs.iter().map(|addr| addr.to_string()).collect())
        }
    }

    fn marked(marker: usize, base: &RuntimeOpts) -> RuntimeOpts {
        RuntimeOpts {
            metrics: RuntimeMetricsOpts {
                poll_time_histogram_buckets: Some(marker),
                ..Default::default()
            },
            ..base.clone()
        }
    }

    fn plan(
        services: &[Spec],
        registry: &[(&str, RuntimeGroupConf)],
    ) -> Result<RuntimePlan, String> {
        let blocking_opts = BlockingPoolOpts {
            max_threads: Some(256),
            thread_keep_alive: Some(Duration::from_secs(10)),
        };
        let registry = registry
            .iter()
            .map(|(name, conf)| (name.to_string(), conf.clone()))
            .collect();
        plan_runtimes(
            services
                .iter()
                .enumerate()
                .map(|(i, s)| (NodeIndex::new(i), s as &dyn ServiceWithDependents)),
            &registry,
            &RuntimeDefaults {
                threads: 4,
                work_stealing: true,
                blocking_opts: &blocking_opts,
                runtime_opts: &RuntimeOpts::default(),
            },
        )
    }

    /// `(name, members, threads, override marker)` of each planned runtime.
    fn summary(plan: &RuntimePlan) -> Vec<(&str, Vec<&str>, usize, Option<usize>)> {
        plan.runtimes
            .iter()
            .map(|r| {
                let members = r.members.iter().map(String::as_str).collect();
                let marker = r.runtime_opts.metrics.poll_time_histogram_buckets;
                (r.name.as_str(), members, r.threads, marker)
            })
            .collect()
    }

    fn spec(name: &'static str, group: Option<&'static str>) -> Spec {
        Spec {
            name,
            group,
            ..Default::default()
        }
    }

    #[test]
    fn ungrouped_services_keep_their_own_runtimes() {
        let b = Spec {
            threads: Some(2),
            opts_marker: Some(7),
            ..spec("b", None)
        };
        let plan = plan(&[spec("a", None), b], &[]).unwrap();
        assert_eq!(
            summary(&plan),
            [("a", vec!["a"], 4, None), ("b", vec!["b"], 2, Some(7))]
        );
        assert!(plan.runtimes.iter().all(|r| !r.grouped && r.work_stealing));
        assert_eq!(plan.runtime_of[&NodeIndex::new(1)], 1);
    }

    #[test]
    fn unregistered_group_takes_largest_threads_and_single_override() {
        let y = Spec {
            threads: Some(6),
            opts_marker: Some(5),
            ..spec("y", Some("g"))
        };
        let plan = plan(&[spec("x", Some("g")), spec("solo", None), y], &[]).unwrap();
        assert_eq!(
            summary(&plan),
            [
                ("g", vec!["x", "y"], 6, Some(5)),
                ("solo", vec!["solo"], 4, None)
            ]
        );
        assert_eq!(plan.runtime_of[&NodeIndex::new(2)], 0);
        assert_eq!(plan.runtimes[0].blocking_opts.max_threads, Some(256));
    }

    #[test]
    fn registered_group_settings_win() {
        let conf = RuntimeGroupConf::new()
            .threads(2)
            .runtime_opts(marked(9, &RuntimeOpts::default()))
            .work_stealing(false)
            .max_blocking_threads(768);
        let x = Spec {
            threads: Some(6),
            opts_marker: Some(1),
            ..spec("x", Some("g"))
        };
        let y = Spec {
            opts_marker: Some(2),
            ..spec("y", Some("g"))
        };
        let plan = plan(&[x, y], &[("g", conf)]).unwrap();
        assert_eq!(summary(&plan), [("g", vec!["x", "y"], 2, Some(9))]);
        let group = &plan.runtimes[0];
        assert!(!group.work_stealing);
        assert_eq!(group.blocking_opts.max_threads, Some(768));
        assert_eq!(
            group.blocking_opts.thread_keep_alive,
            Some(Duration::from_secs(10))
        );
    }

    #[test]
    fn single_member_group_is_named_after_the_group() {
        let x = Spec {
            threads: Some(3),
            opts_marker: Some(4),
            ..spec("x", Some("g"))
        };
        // A service named like the group only draws a warning.
        let plan = plan(&[x, spec("g", None)], &[]).unwrap();
        assert_eq!(
            summary(&plan),
            [("g", vec!["x"], 3, Some(4)), ("g", vec!["g"], 4, None)]
        );
        assert!(plan.runtimes[0].grouped && !plan.runtimes[1].grouped);
        assert_eq!(plan.runtimes[0].label(), "g (group of x)");
    }

    #[test]
    fn conflicting_overrides_and_zero_threads_fail() {
        let x = Spec {
            opts_marker: Some(1),
            ..spec("x", Some("g"))
        };
        let y = Spec {
            opts_marker: Some(2),
            ..spec("y", Some("g"))
        };
        let err = plan(&[x, y], &[]).err().unwrap();
        assert!(
            err.contains("'g'") && err.contains(r#"["x", "y"]"#),
            "{err}"
        );

        let zero = RuntimeGroupConf::new().threads(0);
        let err = plan(&[spec("x", Some("g"))], &[("g", zero)]).err().unwrap();
        assert!(
            err.contains("'g'") && err.contains("at least one thread") && err.contains(r#"["x"]"#),
            "{err}"
        );

        let members = || {
            let x = Spec {
                threads: Some(0),
                ..spec("x", Some("g"))
            };
            let y = Spec {
                threads: Some(4),
                ..spec("y", Some("g"))
            };
            [x, y]
        };
        let err = plan(&members(), &[]).err().unwrap();
        assert!(
            err.contains("'g'") && err.contains("service 'x' asks for 0"),
            "{err}"
        );
        // The group's own thread count wins over the member's.
        let two = RuntimeGroupConf::new().threads(2);
        let plan = plan(&members(), &[("g", two)]).unwrap();
        assert_eq!(plan.runtimes[0].threads, 2);
    }

    #[test]
    fn duplicate_address_in_group_fails() {
        let services = |second_group| {
            let x = Spec {
                addrs: Some(vec!["0.0.0.0:80", "/tmp/a.sock"]),
                ..spec("x", Some("g"))
            };
            let y = Spec {
                addrs: Some(vec!["/tmp/a.sock"]),
                ..spec("y", second_group)
            };
            [x, y]
        };
        let err = plan(&services(Some("g")), &[]).err().unwrap();
        assert!(
            err.contains("'x' and 'y'") && err.contains("'/tmp/a.sock'"),
            "{err}"
        );
        // On separate runtimes the services are left to behave as they always have.
        assert!(plan(&services(None), &[]).is_ok());

        // Members are told apart by identity, not by name.
        let twin = || Spec {
            addrs: Some(vec!["/tmp/a.sock"]),
            ..spec("x", Some("g"))
        };
        let err = plan(&[twin(), twin()], &[]).err().unwrap();
        assert!(
            err.contains("'x' and 'x'") && err.contains("'/tmp/a.sock'"),
            "{err}"
        );
        // A service that lists an address twice is not the group's concern.
        let repeated = Spec {
            addrs: Some(vec!["/tmp/a.sock", "/tmp/a.sock"]),
            ..spec("x", Some("g"))
        };
        assert!(plan(&[repeated], &[]).is_ok());
    }
}

/// Tests that run a [`Server`](super::Server) with runtime groups.
#[cfg(all(test, unix))]
mod server_tests {
    use super::*;
    use crate::apps::ServerApp;
    use crate::protocols::Stream;
    use crate::server::configuration::ServerConf;
    use crate::server::{
        ListenFds, RunArgs, Server, ShutdownSignal, ShutdownSignalWatch, ShutdownWatch,
    };
    use crate::services::{listening, ServiceReadyNotifier};
    use async_trait::async_trait;
    use std::io::Read;
    use std::os::unix::io::IntoRawFd;
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::sync::{mpsc, Arc};
    use std::thread::JoinHandle;
    use std::time::Instant;
    use tokio::sync::{watch, Notify};

    /// `(service, thread name, worker count)`, sent when a service starts.
    type Event = (&'static str, String, usize);

    fn next(events: &mpsc::Receiver<Event>) -> Event {
        events
            .recv_timeout(Duration::from_secs(10))
            .expect("no event from the services")
    }

    struct TestSignal {
        notify: Arc<Notify>,
        graceful: bool,
    }

    #[async_trait]
    impl ShutdownSignalWatch for TestSignal {
        async fn recv(&self) -> ShutdownSignal {
            self.notify.notified().await;
            if self.graceful {
                ShutdownSignal::GracefulTerminate
            } else {
                ShutdownSignal::FastShutdown
            }
        }
    }

    /// Run `server` on its own thread until the returned [`Notify`] fires.
    fn start(server: Server, graceful: bool) -> (Arc<Notify>, JoinHandle<()>) {
        let notify = Arc::new(Notify::new());
        let shutdown_signal = Box::new(TestSignal {
            notify: notify.clone(),
            graceful,
        });
        let join = std::thread::spawn(move || server.run(RunArgs { shutdown_signal }));
        (notify, join)
    }

    /// Reports an [`Event`] when started, becomes ready once its gate opens, then waits for
    /// shutdown and reports it with the thread name `"shutdown"`.
    struct Probe {
        name: &'static str,
        group: Option<&'static str>,
        events: mpsc::Sender<Event>,
        gate: Option<watch::Receiver<bool>>,
        stuck: bool,
    }

    impl Probe {
        fn new(
            name: &'static str,
            group: Option<&'static str>,
            events: &mpsc::Sender<Event>,
        ) -> Self {
            Probe {
                name,
                group,
                events: events.clone(),
                gate: None,
                stuck: false,
            }
        }
    }

    #[async_trait]
    impl ServiceWithDependents for Probe {
        async fn start_service(
            &mut self,
            _fds: Option<ListenFds>,
            mut shutdown: ShutdownWatch,
            _listeners_per_fd: usize,
            ready_notifier: ServiceReadyNotifier,
        ) {
            let thread = std::thread::current()
                .name()
                .unwrap_or_default()
                .to_string();
            let workers = tokio::runtime::Handle::current().metrics().num_workers();
            self.events.send((self.name, thread, workers)).unwrap();
            if let Some(gate) = self.gate.as_mut() {
                let _ = gate.wait_for(|&open| open).await;
            }
            ready_notifier.notify_ready();
            if self.stuck {
                let sleep = || std::thread::sleep(Duration::from_secs(30));
                let _ = tokio::task::spawn_blocking(sleep).await;
            }
            let _ = shutdown.wait_for(|&shutting_down| shutting_down).await;
            let _ = self.events.send((self.name, "shutdown".to_string(), 0));
        }
        fn name(&self) -> &str {
            self.name
        }
        fn runtime_group(&self) -> Option<&str> {
            self.group
        }
    }

    #[test]
    fn grouped_services_share_worker_threads() {
        let (tx, events) = mpsc::channel();
        let conf = ServerConf {
            threads: 1,
            ..Default::default()
        };
        let mut server = Server::new_with_opt_and_conf(None, conf);
        server.add_runtime_group("shared", RuntimeGroupConf::new().threads(2));
        for (name, group) in [("a", Some("shared")), ("b", Some("shared")), ("solo", None)] {
            server.add_service(Probe::new(name, group, &tx));
        }
        let (stop, join) = start(server, false);
        let mut started = (0..3).map(|_| next(&events)).collect::<Vec<_>>();
        started.sort();
        stop.notify_one();
        join.join().unwrap();
        let shared = |name| (name, "shared".to_string(), 2);
        assert_eq!(
            started,
            [shared("a"), shared("b"), ("solo", "solo".into(), 1)]
        );
    }

    #[test]
    fn members_keep_their_own_dependencies_and_readiness() {
        let (tx, events) = mpsc::channel();
        let mut server = Server::new_with_opt_and_conf(None, ServerConf::default());
        let (open_first, first_gate) = watch::channel(false);
        let (_never_open, late_gate) = watch::channel(false);
        let first = server.add_service(Probe {
            gate: Some(first_gate),
            ..Probe::new("first", Some("g"), &tx)
        });
        server.add_service(Probe {
            gate: Some(late_gate),
            ..Probe::new("late", Some("g"), &tx)
        });
        let second = server.add_service(Probe::new("second", Some("g"), &tx));
        let outside = server.add_service(Probe::new("outside", None, &tx));
        second.add_dependency(&first);
        outside.add_dependency(&second);

        let (stop, join) = start(server, false);
        let mut started = [next(&events).0, next(&events).0];
        started.sort();
        assert_eq!(started, ["first", "late"]);
        // `second` waits for `first` inside the group's runtime.
        assert!(events.recv_timeout(Duration::from_millis(300)).is_err());
        open_first.send(true).unwrap();
        assert_eq!(next(&events).0, "second");
        // `outside` waits only for `second`, although `late` never becomes ready.
        assert_eq!(next(&events).0, "outside");
        stop.notify_one();
        join.join().unwrap();
    }

    #[test]
    fn no_steal_group() {
        let (tx, events) = mpsc::channel();
        let mut server = Server::new_with_opt_and_conf(None, ServerConf::default());
        let conf = RuntimeGroupConf::new().threads(2).work_stealing(false);
        server.add_runtime_group("ns", conf);
        let a = server.add_service(Probe::new("a", Some("ns"), &tx));
        let b = server.add_service(Probe::new("b", Some("ns"), &tx));
        b.add_dependency(&a);
        let (stop, join) = start(server, false);
        let started = [next(&events), next(&events)];
        stop.notify_one();
        join.join().unwrap();
        // Each NoSteal thread is a current-thread runtime of one worker.
        assert_eq!(started, [("a", "ns".into(), 1), ("b", "ns".into(), 1)]);
    }

    #[test]
    fn group_shutdown_is_bounded_by_the_timeout() {
        let (tx, events) = mpsc::channel();
        let conf = ServerConf {
            // The runtime is shut down when the grace period ends, dropping any member task that
            // has not yet been polled, so the polite member needs time to see the broadcast.
            grace_period_seconds: Some(1),
            graceful_shutdown_timeout_seconds: Some(1),
            ..Default::default()
        };
        let mut server = Server::new_with_opt_and_conf(None, conf);
        server.add_service(Probe::new("polite", Some("g"), &tx));
        server.add_service(Probe {
            stuck: true,
            ..Probe::new("stuck", Some("g"), &tx)
        });
        let (stop, join) = start(server, true);
        next(&events);
        next(&events);
        let shutdown_started = Instant::now();
        stop.notify_one();
        assert_eq!(next(&events), ("polite", "shutdown".into(), 0));
        join.join().unwrap();
        // About 2s (grace period plus timeout). The stuck member's blocking task would hold the
        // runtime for 30s without the timeout.
        assert!(shutdown_started.elapsed() < Duration::from_secs(10));
    }

    /// Reports the thread that accepted each connection.
    struct Accept(&'static str, mpsc::Sender<Event>);

    #[async_trait]
    impl ServerApp for Accept {
        async fn process_new(
            self: &Arc<Self>,
            _session: Stream,
            _shutdown: &ShutdownWatch,
        ) -> Option<Stream> {
            let thread = std::thread::current()
                .name()
                .unwrap_or_default()
                .to_string();
            let _ = self.1.send((self.0, thread, 0));
            None
        }
    }

    #[test]
    fn grouped_listeners_adopt_inherited_sockets_and_cleanup_still_runs() {
        let dir = std::env::temp_dir().join(format!("pingora-rt-group-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = |name| dir.join(name).to_str().unwrap().to_string();
        let (inherited, fresh) = (path("inherited.sock"), path("fresh.sock"));

        let (tx, events) = mpsc::channel();
        let mut server = Server::new_with_opt_and_conf(None, ServerConf::default());
        for (name, addr) in [("inherited", &inherited), ("fresh", &fresh)] {
            let mut service = listening::Service::new(name.to_string(), Accept(name, tx.clone()));
            service.add_uds(addr, None);
            service.set_runtime_group("edge");
            server.add_service(service);
        }
        // Sockets left by a previous process: one a member claims, and one nobody claims.
        let previous = UnixListener::bind(&inherited).unwrap();
        previous.set_nonblocking(true).unwrap();
        let (mut unclaimed_peer, unclaimed) = UnixStream::pair().unwrap();
        unclaimed_peer.set_nonblocking(true).unwrap();
        {
            let fds = server.listen_fds();
            let mut fds = fds.lock();
            fds.add(inherited.clone(), previous.into_raw_fd());
            fds.add("unclaimed".to_string(), unclaimed.into_raw_fd());
        }

        let (stop, join) = start(server, false);
        for (name, addr) in [("inherited", &inherited), ("fresh", &fresh)] {
            let deadline = Instant::now() + Duration::from_secs(10);
            let _client = loop {
                match UnixStream::connect(addr) {
                    Ok(client) => break client,
                    Err(e) if Instant::now() > deadline => panic!("connect {addr}: {e}"),
                    Err(_) => std::thread::sleep(Duration::from_millis(20)),
                }
            };
            assert_eq!(next(&events), (name, "edge".into(), 0));
        }
        // End-of-file means the peer was closed; an open peer reports `WouldBlock`.
        let unclaimed_read = unclaimed_peer.read(&mut [0u8; 1]);
        stop.notify_one();
        join.join().unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(
            matches!(unclaimed_read, Ok(0)),
            "the unclaimed inherited socket was not closed: {unclaimed_read:?}"
        );
    }
}
