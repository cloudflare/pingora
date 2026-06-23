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

//! Server configurations
//!
//! Server configurations define startup settings such as:
//! * User and group to run as after daemonization
//! * Number of threads per service
//! * Error log file path

use clap::Parser;
use log::{debug, trace};
use pingora_error::{Error, ErrorType::*, OrErr, Result};
pub use pingora_runtime::RuntimeMetricsPollTimeHistogramScale;
use pingora_runtime::{RuntimeMetricsOpts, RuntimeOpts};
use serde::{Deserialize, Serialize};
use std::ffi::OsString;
use std::fs;
use std::num::NonZeroU64;
use std::path::PathBuf;
use std::time::Duration;

// default maximum upstream retries for retry-able proxy errors
const DEFAULT_MAX_RETRIES: usize = 16;
const MAX_RUNTIME_METRICS_POLL_TIME_HISTOGRAM_BUCKETS: usize = 1024;

/// The configuration file
///
/// Pingora configuration files are by default YAML files, but any key value format can potentially
/// be used.
///
/// # Extension
/// New keys can be added to the configuration files which this configuration object will ignore.
/// Then, users can parse these key-values to pass to their code to use.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ServerConf {
    /// Version
    pub version: usize,
    /// Whether to run this process in the background.
    pub daemon: bool,
    /// When configured and `daemon` setting is `true`, error log will be written to the given
    /// file. Otherwise StdErr will be used.
    pub error_log: Option<String>,
    /// The pid (process ID) file of this server to be created when running in background
    pub pid_file: String,
    /// the path to the upgrade socket
    ///
    /// In order to perform zero downtime restart, both the new and old process need to agree on the
    /// path to this sock in order to coordinate the upgrade.
    pub upgrade_sock: String,
    /// If configured, after daemonization, this process will switch to the given user before
    /// starting to serve traffic.
    pub user: Option<String>,
    /// Similar to `user`, the group this process should switch to.
    pub group: Option<String>,
    /// Working directory for the daemonized process.
    ///
    /// Only applied when `daemon` is `true`; set this to start the daemon from a known cwd.
    // TODO: other OS path options should likely be `PathBuf` as well.
    pub working_directory: Option<PathBuf>,
    /// How many threads **each** service should get. The threads are not shared across services.
    pub threads: usize,
    /// Number of listener tasks to use per fd. This allows for parallel accepts.
    pub listener_tasks_per_fd: usize,
    /// Allow work stealing between threads of the same service. Default `true`.
    pub work_stealing: bool,
    /// Enable Tokio's experimental alternative timer on work-stealing service runtimes.
    ///
    /// Requires building with `--cfg tokio_unstable`. Ignored when
    /// [`Self::work_stealing`] is disabled.
    pub runtime_enable_alt_timer: bool,
    /// The path to CA file the SSL library should use. If empty, the default trust store location
    /// defined by the SSL library will be used.
    pub ca_file: Option<String>,
    /// The maximum number of unique s2n configs to cache. Creating a new s2n config is an
    /// expensive operation, so we cache and re-use config objects with identical configurations.
    /// A value of 0 disables the cache.
    ///
    /// WARNING: Disabling the s2n config cache can result in poor performance
    #[cfg(feature = "s2n")]
    pub s2n_config_cache_size: Option<usize>,
    /// Grace period in seconds before starting the final step of the graceful shutdown after signaling shutdown.
    pub grace_period_seconds: Option<u64>,
    /// Timeout in seconds of the final step for the graceful shutdown.
    pub graceful_shutdown_timeout_seconds: Option<u64>,
    // These options don't belong here as they are specific to certain services
    /// IPv4 addresses for a client connector to bind to. See
    /// [`ConnectorOptions`](crate::connectors::ConnectorOptions).
    /// Note: this is an _unstable_ field that may be renamed or removed in the future.
    pub client_bind_to_ipv4: Vec<String>,
    /// IPv6 addresses for a client connector to bind to. See
    /// [`ConnectorOptions`](crate::connectors::ConnectorOptions).
    /// Note: this is an _unstable_ field that may be renamed or removed in the future.
    pub client_bind_to_ipv6: Vec<String>,
    /// Keepalive pool size for client connections to upstream. See
    /// [`ConnectorOptions`](crate::connectors::ConnectorOptions).
    /// Note: this is an _unstable_ field that may be renamed or removed in the future.
    pub upstream_keepalive_pool_size: usize,
    /// Number of dedicated thread pools to use for upstream connection establishment.
    /// See [`ConnectorOptions`](crate::connectors::ConnectorOptions).
    /// Note: this is an _unstable_ field that may be renamed or removed in the future.
    pub upstream_connect_offload_threadpools: Option<usize>,
    /// Number of threads per dedicated upstream connection establishment pool.
    /// See [`ConnectorOptions`](crate::connectors::ConnectorOptions).
    /// Note: this is an _unstable_ field that may be renamed or removed in the future.
    pub upstream_connect_offload_thread_per_pool: Option<usize>,
    /// When enabled allows TLS keys to be written to a file specified by the SSLKEYLOG
    /// env variable. This can be used by tools like Wireshark to decrypt upstream traffic
    /// for debugging purposes.
    /// Note: this is an _unstable_ field that may be renamed or removed in the future.
    pub upstream_debug_ssl_keylog: bool,
    /// The maximum number of retries that will be attempted when an error is
    /// retry-able (`e.retry() == true`) when proxying to upstream.
    ///
    /// This setting is a fail-safe and defaults to 16.
    pub max_retries: usize,
    /// Maximum number of retries for upgrade socket connect and accept operations.
    /// This controls how many times send_fds_to will retry connecting and how many times
    /// get_fds_from will retry accepting during graceful upgrades.
    /// The retry interval is 1 second between attempts.
    /// If not set, defaults to 5 retries.
    pub upgrade_sock_connect_accept_max_retries: Option<usize>,
    /// The maximum number of threads in each runtime's blocking thread pool.
    ///
    /// The blocking pool handles [`tokio::task::spawn_blocking`] tasks.
    /// When not set, the tokio default (512) is used.
    pub max_blocking_threads: Option<usize>,
    /// How long, in seconds, idle blocking threads are kept alive before being shut down.
    ///
    /// When not set, the tokio default (10 seconds) is used.
    pub blocking_threads_ttl_seconds: Option<u64>,
    /// Timeout durations greater than this threshold use Tokio's native timeout instead of
    /// Pingora's fast timeout.
    ///
    /// This avoids retaining long-duration cancelled timers in Pingora's shared timer map until
    /// their original deadline. When not set, defaults to 900 seconds (15 minutes). Set to `null`
    /// to disable the Tokio fallback.
    pub fast_timeout_to_tokio_threshold_seconds: Option<u64>,
    /// Enable Tokio's poll-time histogram on runtimes created by this server.
    ///
    /// This adds two timestamp reads to every task poll, so it should be
    /// enabled deliberately when investigating runtime latency. Requires
    /// building with `--cfg tokio_unstable`.
    pub runtime_metrics_poll_time_histogram: bool,
    /// Bucket scale for Tokio's poll-time histogram.
    ///
    /// Ignored unless [`Self::runtime_metrics_poll_time_histogram`] is enabled.
    pub runtime_metrics_poll_time_histogram_scale: Option<RuntimeMetricsPollTimeHistogramScale>,
    /// Width of the first Tokio poll-time histogram bucket in microseconds.
    ///
    /// Ignored unless [`Self::runtime_metrics_poll_time_histogram`] is enabled.
    pub runtime_metrics_poll_time_histogram_resolution_micros: Option<u64>,
    /// Number of Tokio poll-time histogram buckets.
    ///
    /// Ignored unless [`Self::runtime_metrics_poll_time_histogram`] is enabled. Memory usage
    /// scales with runtimes × workers × buckets, so values above 1024 are rejected.
    pub runtime_metrics_poll_time_histogram_buckets: Option<usize>,
    /// When `daemon` is `true`, controls whether the parent process of the daemon fork waits for
    /// the child to signal readiness before exiting.
    ///
    /// When `false` (default), the parent exits immediately after the daemon fork, matching the
    /// traditional daemonization behavior. Systemd will consider the service started as soon as
    /// the parent exits, which may be before the child has finished bootstrapping.
    ///
    /// When `true`, the parent waits (up to [`Self::daemon_ready_timeout_seconds`]) for the child
    /// to send `SIGUSR1` after bootstrap completes. This causes systemd to delay any subsequent
    /// steps (such as sending `SIGQUIT` to the old process) until the new instance is fully ready
    /// to serve traffic. If the child does not signal in time, the parent exits with a non-zero
    /// exit code, causing systemd to abort the reload.
    pub daemon_wait_for_ready: bool,
    /// Timeout in seconds for the parent process to wait for the child to signal readiness during
    /// daemonization when [`Self::daemon_wait_for_ready`] is `true`.
    ///
    /// If the child does not send `SIGUSR1` within this timeout, the parent exits with a non-zero
    /// exit code.
    ///
    /// Defaults to 600 seconds (10 minutes).
    pub daemon_ready_timeout_seconds: Option<NonZeroU64>,
    /// How long the child process will keep retrying `SIGUSR1` to the parent when the signal
    /// fails with a permission error (`EPERM`) during daemonization.
    ///
    /// After the daemon fork, the parent always drops its credentials to the configured user and
    /// group (see [`Self::user`], [`Self::group`]). Because the privilege drop happens after the
    /// fork, there is a small window where the child may attempt to signal the parent before the
    /// parent has finished changing its credentials. During this window the kernel will reject the
    /// signal with `EPERM` because the child and parent are running as different users. The child
    /// retries every 100 ms until this timeout elapses.
    ///
    /// In practice this window is very small, so the default of 60 seconds is far more than
    /// enough to account for it.
    ///
    /// Only retries on `EPERM`; any other error (e.g. `ESRCH` — parent no longer exists) is
    /// treated as fatal and logged without retrying.
    ///
    /// Defaults to 60 seconds.
    pub daemon_notify_timeout_seconds: Option<NonZeroU64>,
}

impl Default for ServerConf {
    fn default() -> Self {
        ServerConf {
            version: 0,
            client_bind_to_ipv4: vec![],
            client_bind_to_ipv6: vec![],
            ca_file: None,
            #[cfg(feature = "s2n")]
            s2n_config_cache_size: None,
            daemon: false,
            error_log: None,
            upstream_debug_ssl_keylog: false,
            pid_file: "/tmp/pingora.pid".to_string(),
            upgrade_sock: "/tmp/pingora_upgrade.sock".to_string(),
            user: None,
            group: None,
            working_directory: None,
            threads: 1,
            listener_tasks_per_fd: 1,
            work_stealing: true,
            runtime_enable_alt_timer: false,
            upstream_keepalive_pool_size: 128,
            upstream_connect_offload_threadpools: None,
            upstream_connect_offload_thread_per_pool: None,
            grace_period_seconds: None,
            graceful_shutdown_timeout_seconds: None,
            max_retries: DEFAULT_MAX_RETRIES,
            upgrade_sock_connect_accept_max_retries: None,
            max_blocking_threads: None,
            blocking_threads_ttl_seconds: None,
            fast_timeout_to_tokio_threshold_seconds: Some(
                pingora_timeout::fast_timeout::DEFAULT_FAST_TIMEOUT_TO_TOKIO_THRESHOLD.as_secs(),
            ),
            runtime_metrics_poll_time_histogram: false,
            runtime_metrics_poll_time_histogram_scale: None,
            runtime_metrics_poll_time_histogram_resolution_micros: None,
            runtime_metrics_poll_time_histogram_buckets: None,
            daemon_ready_timeout_seconds: None,
            daemon_wait_for_ready: false,
            daemon_notify_timeout_seconds: None,
        }
    }
}

/// Command-line options
///
/// Call `Opt::parse_args()` to build this object from the process's command line arguments.
#[derive(Parser, Debug, Default)]
#[clap(name = "basic", long_about = None)]
pub struct Opt {
    /// Whether this server should try to upgrade from a running old server
    #[clap(
        short,
        long,
        help = "This is the base set of command line arguments for a pingora-based service",
        long_help = None
    )]
    pub upgrade: bool,

    /// Whether this server should run in the background
    #[clap(short, long)]
    pub daemon: bool,

    /// Not actually used. This flag is there so that the server is not upset seeing this flag
    /// passed from `cargo test` sometimes
    #[clap(long, hide = true)]
    pub nocapture: bool,

    /// Test the configuration and exit
    ///
    /// When this flag is set, calling `server.bootstrap()` will exit the process without errors
    ///
    /// This flag is useful for upgrading service where the user wants to make sure the new
    /// service can start before shutting down the old server process.
    #[clap(
        short,
        long,
        help = "This flag is useful for upgrading service where the user wants \
                to make sure the new service can start before shutting down \
                the old server process.",
        long_help = None
    )]
    pub test: bool,

    /// The path to the configuration file.
    ///
    /// See [`ServerConf`] for more details of the configuration file.
    #[clap(short, long, help = "The path to the configuration file.", long_help = None)]
    pub conf: Option<String>,
}

impl ServerConf {
    // Does not has to be async until we want runtime reload
    pub fn load_from_yaml<P>(path: P) -> Result<Self>
    where
        P: AsRef<std::path::Path> + std::fmt::Display,
    {
        let conf_str = fs::read_to_string(&path).or_err_with(ReadError, || {
            format!("Unable to read conf file from {path}")
        })?;
        debug!("Conf file read from {path}");
        Self::from_yaml(&conf_str)
    }

    pub fn load_yaml_with_opt_override(opt: &Opt) -> Result<Self> {
        if let Some(path) = &opt.conf {
            let mut conf = Self::load_from_yaml(path)?;
            conf.merge_with_opt(opt);
            Ok(conf)
        } else {
            Error::e_explain(ReadError, "No path specified")
        }
    }

    pub fn new() -> Option<Self> {
        Self::from_yaml("---\nversion: 1").ok()
    }

    pub fn new_with_opt_override(opt: &Opt) -> Option<Self> {
        let conf = Self::new();
        match conf {
            Some(mut c) => {
                c.merge_with_opt(opt);
                Some(c)
            }
            None => None,
        }
    }

    pub fn from_yaml(conf_str: &str) -> Result<Self> {
        trace!("Read conf file: {conf_str}");
        let conf: ServerConf = serde_yaml::from_str(conf_str).or_err_with(ReadError, || {
            format!("Unable to parse yaml conf {conf_str}")
        })?;

        trace!("Loaded conf: {conf:?}");
        conf.validate()
    }

    pub fn to_yaml(&self) -> String {
        serde_yaml::to_string(self).unwrap()
    }

    pub fn validate(self) -> Result<Self> {
        if self.max_blocking_threads == Some(0) {
            return Error::e_explain(ReadError, "max_blocking_threads must be greater than zero");
        }
        if self.runtime_metrics_poll_time_histogram_resolution_micros == Some(0) {
            return Error::e_explain(
                ReadError,
                "runtime_metrics_poll_time_histogram_resolution_micros must be greater than zero",
            );
        }
        if self.runtime_metrics_poll_time_histogram_buckets == Some(0) {
            return Error::e_explain(
                ReadError,
                "runtime_metrics_poll_time_histogram_buckets must be greater than zero",
            );
        }
        if self
            .runtime_metrics_poll_time_histogram_buckets
            .is_some_and(|buckets| buckets > MAX_RUNTIME_METRICS_POLL_TIME_HISTOGRAM_BUCKETS)
        {
            return Error::e_explain(
                ReadError,
                format!(
                    "runtime_metrics_poll_time_histogram_buckets must be at most {MAX_RUNTIME_METRICS_POLL_TIME_HISTOGRAM_BUCKETS}"
                ),
            );
        }
        Ok(self)
    }

    /// Build the default runtime options derived from this server configuration.
    pub fn runtime_opts(&self) -> RuntimeOpts {
        RuntimeOpts {
            metrics: RuntimeMetricsOpts {
                poll_time_histogram: self.runtime_metrics_poll_time_histogram,
                poll_time_histogram_scale: self.runtime_metrics_poll_time_histogram_scale,
                poll_time_histogram_resolution: self
                    .runtime_metrics_poll_time_histogram_resolution_micros
                    .map(Duration::from_micros),
                poll_time_histogram_buckets: self.runtime_metrics_poll_time_histogram_buckets,
            },
            enable_alt_timer: self.runtime_enable_alt_timer,
            #[cfg(feature = "dial9")]
            dial9: None,
        }
    }

    pub fn merge_with_opt(&mut self, opt: &Opt) {
        if opt.daemon {
            self.daemon = true;
        }
    }
}

/// Create an instance of Opt by parsing the current command-line args.
/// This is equivalent to running `Opt::parse` but does not require the
/// caller to have included the `clap::Parser`
impl Opt {
    pub fn parse_args() -> Self {
        Opt::parse()
    }

    pub fn parse_from_args<I, T>(args: I) -> Self
    where
        I: IntoIterator<Item = T>,
        T: Into<OsString> + Clone,
    {
        Opt::parse_from(args)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn init_log() {
        let _ = env_logger::builder().is_test(true).try_init();
    }

    #[test]
    fn not_a_test_i_cannot_write_yaml_by_hand() {
        init_log();
        let conf = ServerConf {
            version: 1,
            client_bind_to_ipv4: vec!["1.2.3.4".to_string(), "5.6.7.8".to_string()],
            client_bind_to_ipv6: vec![],
            ca_file: None,
            #[cfg(feature = "s2n")]
            s2n_config_cache_size: None,
            daemon: false,
            error_log: None,
            upstream_debug_ssl_keylog: false,
            pid_file: "".to_string(),
            upgrade_sock: "".to_string(),
            user: None,
            group: None,
            working_directory: None,
            threads: 1,
            listener_tasks_per_fd: 1,
            work_stealing: true,
            runtime_enable_alt_timer: false,
            upstream_keepalive_pool_size: 4,
            upstream_connect_offload_threadpools: None,
            upstream_connect_offload_thread_per_pool: None,
            grace_period_seconds: None,
            graceful_shutdown_timeout_seconds: None,
            max_retries: 1,
            upgrade_sock_connect_accept_max_retries: None,
            max_blocking_threads: None,
            blocking_threads_ttl_seconds: None,
            fast_timeout_to_tokio_threshold_seconds: Some(
                pingora_timeout::fast_timeout::DEFAULT_FAST_TIMEOUT_TO_TOKIO_THRESHOLD.as_secs(),
            ),
            runtime_metrics_poll_time_histogram: false,
            runtime_metrics_poll_time_histogram_scale: None,
            runtime_metrics_poll_time_histogram_resolution_micros: None,
            runtime_metrics_poll_time_histogram_buckets: None,
            daemon_ready_timeout_seconds: None,
            daemon_wait_for_ready: false,
            daemon_notify_timeout_seconds: None,
        };
        // cargo test -- --nocapture not_a_test_i_cannot_write_yaml_by_hand
        println!("{}", conf.to_yaml());
    }

    #[test]
    fn test_load_file() {
        init_log();
        let conf_str = r#"
---
version: 1
client_bind_to_ipv4:
    - 1.2.3.4
    - 5.6.7.8
client_bind_to_ipv6: []
        "#
        .to_string();
        let conf = ServerConf::from_yaml(&conf_str).unwrap();
        assert_eq!(2, conf.client_bind_to_ipv4.len());
        assert_eq!(0, conf.client_bind_to_ipv6.len());
        assert_eq!(1, conf.version);
    }

    #[test]
    fn test_default() {
        init_log();
        let conf_str = r#"
---
version: 1
        "#
        .to_string();
        let conf = ServerConf::from_yaml(&conf_str).unwrap();
        assert_eq!(0, conf.client_bind_to_ipv4.len());
        assert_eq!(0, conf.client_bind_to_ipv6.len());
        assert_eq!(1, conf.version);
        assert_eq!(DEFAULT_MAX_RETRIES, conf.max_retries);
        assert_eq!("/tmp/pingora.pid", conf.pid_file);
        assert_eq!(
            Some(pingora_timeout::fast_timeout::DEFAULT_FAST_TIMEOUT_TO_TOKIO_THRESHOLD.as_secs()),
            conf.fast_timeout_to_tokio_threshold_seconds
        );
    }

    #[test]
    fn test_runtime_enable_alt_timer_config() {
        init_log();
        let conf_str = r#"
---
version: 1
runtime_enable_alt_timer: true
        "#;

        let conf = ServerConf::from_yaml(conf_str).unwrap();
        assert!(conf.runtime_enable_alt_timer);
    }

    #[test]
    fn test_runtime_opts_from_config() {
        init_log();
        let conf_str = r#"
---
version: 1
runtime_enable_alt_timer: true
runtime_metrics_poll_time_histogram: true
runtime_metrics_poll_time_histogram_scale: log
runtime_metrics_poll_time_histogram_resolution_micros: 20
runtime_metrics_poll_time_histogram_buckets: 16
        "#;

        let conf = ServerConf::from_yaml(conf_str).unwrap();
        let opts = conf.runtime_opts();
        assert!(opts.enable_alt_timer);
        assert!(opts.metrics.poll_time_histogram);
        assert_eq!(
            Some(RuntimeMetricsPollTimeHistogramScale::Log),
            opts.metrics.poll_time_histogram_scale
        );
        assert_eq!(
            Some(Duration::from_micros(20)),
            opts.metrics.poll_time_histogram_resolution
        );
        assert_eq!(Some(16), opts.metrics.poll_time_histogram_buckets);
        #[cfg(feature = "dial9")]
        assert!(opts.dial9.is_none());
    }

    #[test]
    fn test_working_directory_deserializes_from_yaml_string() {
        init_log();
        let conf_str = r#"
---
version: 1
daemon: true
working_directory: /var/lib/pingora
        "#;

        let conf = ServerConf::from_yaml(conf_str).unwrap();
        assert_eq!(
            conf.working_directory.as_deref(),
            Some(std::path::Path::new("/var/lib/pingora"))
        );

        let yaml = serde_yaml::to_value(&conf).unwrap();
        assert_eq!(
            yaml.get("working_directory"),
            Some(&serde_yaml::Value::String("/var/lib/pingora".to_string()))
        );
    }

    #[test]
    fn test_zero_max_blocking_threads_is_rejected() {
        init_log();
        let conf_str = r#"
---
version: 1
max_blocking_threads: 0
        "#;
        let result = ServerConf::from_yaml(conf_str);
        assert!(
            result.is_err(),
            "max_blocking_threads: 0 should fail validation"
        );
    }

    #[test]
    fn test_valid_max_blocking_threads() {
        init_log();
        let conf_str = r#"
---
version: 1
max_blocking_threads: 64
blocking_threads_ttl_seconds: 30
        "#;
        let conf = ServerConf::from_yaml(conf_str).unwrap();
        assert_eq!(Some(64), conf.max_blocking_threads);
        assert_eq!(Some(30), conf.blocking_threads_ttl_seconds);
    }

    #[test]
    fn test_fast_timeout_to_tokio_threshold_config() {
        init_log();
        let conf_str = r#"
---
version: 1
fast_timeout_to_tokio_threshold_seconds: 120
        "#;
        let conf = ServerConf::from_yaml(conf_str).unwrap();
        assert_eq!(Some(120), conf.fast_timeout_to_tokio_threshold_seconds);
    }

    #[test]
    fn test_fast_timeout_to_tokio_threshold_can_be_disabled() {
        init_log();
        let conf_str = r#"
---
version: 1
fast_timeout_to_tokio_threshold_seconds:
        "#;
        let conf = ServerConf::from_yaml(conf_str).unwrap();
        assert_eq!(None, conf.fast_timeout_to_tokio_threshold_seconds);
    }

    #[test]
    fn test_runtime_poll_time_histogram_config() {
        init_log();
        let conf_str = r#"
---
version: 1
runtime_metrics_poll_time_histogram: true
runtime_metrics_poll_time_histogram_scale: log
runtime_metrics_poll_time_histogram_resolution_micros: 20
runtime_metrics_poll_time_histogram_buckets: 16
        "#;

        let conf = ServerConf::from_yaml(conf_str).unwrap();
        assert!(conf.runtime_metrics_poll_time_histogram);
        assert_eq!(
            Some(RuntimeMetricsPollTimeHistogramScale::Log),
            conf.runtime_metrics_poll_time_histogram_scale
        );
        assert_eq!(
            Some(20),
            conf.runtime_metrics_poll_time_histogram_resolution_micros
        );
        assert_eq!(Some(16), conf.runtime_metrics_poll_time_histogram_buckets);
    }

    #[test]
    fn test_runtime_poll_time_histogram_bucket_limit() {
        init_log();
        let conf_str = format!(
            r#"
---
version: 1
runtime_metrics_poll_time_histogram: true
runtime_metrics_poll_time_histogram_buckets: {}
        "#,
            MAX_RUNTIME_METRICS_POLL_TIME_HISTOGRAM_BUCKETS + 1
        );

        let result = ServerConf::from_yaml(&conf_str);
        assert!(
            result.is_err(),
            "excessive runtime_metrics_poll_time_histogram_buckets should fail validation"
        );
    }
}
