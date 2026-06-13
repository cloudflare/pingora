# Configuration

A Pingora configuration file is a list of Pingora settings in yaml format.

Example
```yaml
---
version: 1
threads: 2
pid_file: /run/pingora.pid
upgrade_sock: /tmp/pingora_upgrade.sock
user: nobody
group: webusers
```
## Settings
| Key      | meaning        | value type |
| ------------- |-------------| ----|
| version | the version of the conf, currently it is a constant `1` | number |
| pid_file | The path to the pid file | string |
| daemon | whether to run the server in the background | bool |
| error_log | the path to error log output file. STDERR is used if not set | string |
| upgrade_sock | the path to the upgrade socket. | string |
| threads | number of threads per service | number |
| user | the user the pingora server should be run under after daemonization | string |
| group | the group the pingora server should be run under after daemonization | string |
| working_directory | the working directory for the daemonized process | string |
| client_bind_to_ipv4 | source IPv4 addresses to bind to when connecting to server | list of string |
| client_bind_to_ipv6 | source IPv6 addresses to bind to when connecting to server| list of string |
| ca_file | The path to the root CA file | string |
| s2n_config_cache_size | The maximum number of unique s2n configs to cache. A value of 0 disables the cache. Default: 10 (s2n-tls only) | number |
| work_stealing | Enable work stealing runtime (default true). See Pingora runtime (WIP) section for more info | bool |
| runtime_enable_alt_timer | Enable Tokio's experimental alternative timer on work-stealing service runtimes. Requires building with `--cfg tokio_unstable`. Ignored when `work_stealing` is disabled. Default: `false` | bool |
| fast_timeout_to_tokio_threshold_seconds | Timeout durations greater than this value use Tokio's native timeout instead of Pingora's fast timeout. Default: `900`. Set to `null` to disable the Tokio fallback. | number |
| runtime_metrics_poll_time_histogram | Enable Tokio poll-time histograms on service runtimes. Requires building with `--cfg tokio_unstable`; adds two timestamp reads to every task poll. Default: `false` | bool |
| runtime_metrics_poll_time_histogram_scale | Bucket scale for Tokio poll-time histograms. Valid values: `linear`, `log`. Ignored unless `runtime_metrics_poll_time_histogram` is enabled. | string |
| runtime_metrics_poll_time_histogram_resolution_micros | Width of the first Tokio poll-time histogram bucket in microseconds. Must be greater than 0. Ignored unless `runtime_metrics_poll_time_histogram` is enabled. | number |
| runtime_metrics_poll_time_histogram_buckets | Number of Tokio poll-time histogram buckets. Must be greater than 0 and at most 1024. Memory usage scales with runtimes × workers × buckets. Ignored unless `runtime_metrics_poll_time_histogram` is enabled. | number |
| upstream_keepalive_pool_size | The number of idle upstream connections to keep per tokio worker. The pool's effective ceiling is `upstream_keepalive_pool_size × threads`. Eviction is globally consistent across workers. | number |
| daemon_wait_for_ready | When `true` and `daemon` is `true`, the parent process waits for the daemon to signal readiness (via `SIGUSR1`) before exiting. This causes systemd to delay sending `SIGQUIT` to the old process until the new instance is fully bootstrapped. Default: `false` | bool |
| daemon_ready_timeout_seconds | How long (in seconds) the parent waits for the daemon to signal readiness when `daemon_wait_for_ready` is `true`. If the daemon does not signal in time the parent exits with a non-zero code, causing systemd to abort the reload. Default: `600` | number |
| daemon_notify_timeout_seconds | How long (in seconds) the daemon retries sending `SIGUSR1` to the parent when the attempt fails with a permission error. This covers the brief window after the fork where the parent has not yet dropped its UID to match the daemon. Default: `60` | number |

## dial9

dial9 Tokio runtime telemetry is configured programmatically, not through
the YAML configuration file. This avoids applying experimental telemetry to
every service runtime and lets services provide non-serializable options such
as a pre-built S3 client.

dial9 is only available when Pingora is built with the `dial9` feature and
`--cfg tokio_unstable`. Services can override the global runtime options with
`runtime_opts_override()`:

```rust
use pingora::server::{Dial9RuntimeOpts, RuntimeOpts};
use pingora::services::Service;

struct MyService;

impl Service for MyService {
    fn name(&self) -> &str {
        "my-service"
    }

    fn runtime_opts_override(&self, global: &RuntimeOpts) -> Option<RuntimeOpts> {
        let mut opts = global.clone();
        opts.dial9 = Some(
            Dial9RuntimeOpts::new("/var/lib/pingora/dial9/my-service/trace.bin")
                .with_max_file_size(100 * 1024 * 1024)
                .with_max_total_size(512 * 1024 * 1024),
        );
        Some(opts)
    }
}
```

When built with the `dial9-worker-s3` feature, sealed trace segments can also
be uploaded to an S3-compatible bucket:

```rust
use pingora::server::{Dial9RuntimeOpts, Dial9S3UploadOpts, RuntimeOpts};
use pingora::services::Service;

struct MyService {
    s3_client: aws_sdk_s3::Client,
}

impl Service for MyService {
    fn name(&self) -> &str {
        "my-service"
    }

    fn runtime_opts_override(&self, global: &RuntimeOpts) -> Option<RuntimeOpts> {
        let mut opts = global.clone();
        opts.dial9 = Some(
            Dial9RuntimeOpts::new("/var/lib/pingora/dial9/my-service/trace.bin")
                .with_s3_upload(
                    Dial9S3UploadOpts::new("my-trace-bucket", "my-service")
                        .with_prefix("traces/my-service")
                        .with_region("us-east-1")
                        .with_client(self.s3_client.clone()),
                ),
        );
        Some(opts)
    }
}
```

The S3 client is optional. When omitted, dial9 uses the AWS SDK default
configuration chain and its bucket-region detection.

## Extension
Any unknown settings will be ignored. This allows extending the conf file to add and pass user defined settings. See User defined configuration section.
