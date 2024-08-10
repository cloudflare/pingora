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

//! Server configurations
//!
//! Server configurations define startup settings such as:
//! * User and group to run as after daemonization
//! * Number of threads per service
//! * Error log file path

use clap::Parser;
use log::{debug, trace};
use pingora_error::{Error, ErrorType::*, OrErr, Result};
use serde::{Deserialize, Serialize};
use std::fs;

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
    /// How many threads **each** service should get. The threads are not shared across services.
    pub threads: usize,
    /// Allow work stealing between threads of the same service. Default `true`.
    pub work_stealing: bool,
    /// The path to CA file the SSL library should use. If empty, the default trust store location
    /// defined by the SSL library will be used.
    pub ca_file: Option<String>,
    /// Grace period in seconds before starting the final step of the graceful shutdown after signaling shutdown.
    pub grace_period_seconds: Option<u64>,
    /// Timeout in seconds of the final step for the graceful shutdown.
    pub graceful_shutdown_timeout_seconds: Option<u64>,
    // These options don't belong here as they are specific to certain services
    /// IPv4 addresses for a client connector to bind to. See [`ConnectorOptions`].
    /// Note: this is an _unstable_ field that may be renamed or removed in the future.
    pub client_bind_to_ipv4: Vec<String>,
    /// IPv6 addresses for a client connector to bind to. See [`ConnectorOptions`].
    /// Note: this is an _unstable_ field that may be renamed or removed in the future.
    pub client_bind_to_ipv6: Vec<String>,
    /// Keepalive pool size for client connections to upstream. See [`ConnectorOptions`].
    /// Note: this is an _unstable_ field that may be renamed or removed in the future.
    pub upstream_keepalive_pool_size: usize,
    /// Number of dedicated thread pools to use for upstream connection establishment.
    /// See [`ConnectorOptions`].
    /// Note: this is an _unstable_ field that may be renamed or removed in the future.
    pub upstream_connect_offload_threadpools: Option<usize>,
    /// Number of threads per dedicated upstream connection establishment pool.
    /// See [`ConnectorOptions`].
    /// Note: this is an _unstable_ field that may be renamed or removed in the future.
    pub upstream_connect_offload_thread_per_pool: Option<usize>,
    /// When enabled allows TLS keys to be written to a file specified by the SSLKEYLOG
    /// env variable. This can be used by tools like Wireshark to decrypt upstream traffic
    /// for debugging purposes.
    /// Note: this is an _unstable_ field that may be renamed or removed in the future.
    pub upstream_debug_ssl_keylog: bool,
}

impl Default for ServerConf {
    fn default() -> Self {
        ServerConf {
            version: 0,
            client_bind_to_ipv4: vec![],
            client_bind_to_ipv6: vec![],
            ca_file: None,
            daemon: false,
            error_log: None,
            upstream_debug_ssl_keylog: false,
            pid_file: "/tmp/pingora.pid".to_string(),
            upgrade_sock: "/tmp/pingora_upgrade.sock".to_string(),
            user: None,
            group: None,
            threads: 1,
            work_stealing: true,
            upstream_keepalive_pool_size: 128,
            upstream_connect_offload_threadpools: None,
            upstream_connect_offload_thread_per_pool: None,
            grace_period_seconds: None,
            graceful_shutdown_timeout_seconds: None,
        }
    }
}

/// Command-line options
///
/// Call `Opt::from_args()` to build this object from the process's command line arguments.
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
    #[clap(long, hidden = true)]
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
        // TODO: do the validation
        Ok(self)
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
            daemon: false,
            error_log: None,
            upstream_debug_ssl_keylog: false,
            pid_file: "".to_string(),
            upgrade_sock: "".to_string(),
            user: None,
            group: None,
            threads: 1,
            work_stealing: true,
            upstream_keepalive_pool_size: 4,
            upstream_connect_offload_threadpools: None,
            upstream_connect_offload_thread_per_pool: None,
            grace_period_seconds: None,
            graceful_shutdown_timeout_seconds: None,
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
        assert_eq!("/tmp/pingora.pid", conf.pid_file);
    }
}
