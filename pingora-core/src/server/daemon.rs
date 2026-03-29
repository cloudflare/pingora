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

use daemonize::{Daemonize, Outcome, Stdio};
use log::{debug, error, info};
use pingora_error::{Error, ErrorType, OrErr, Result};
use std::ffi::CString;
use std::fs::{self, OpenOptions};
use std::os::unix::prelude::OpenOptionsExt;
use std::path::Path;
use std::process;
use std::thread;
use std::time::{Duration, Instant};

use crate::server::configuration::ServerConf;

/// Error returned by [`send_signal`].
#[derive(Debug)]
pub(crate) enum SignalError {
    /// The caller does not have permission to send the signal to the target process (`EPERM`).
    PermissionDenied,
    /// Any other error from `kill(2)`. Contains the raw `errno` value.
    OtherSignalError(i32),
}

impl std::fmt::Display for SignalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SignalError::PermissionDenied => write!(f, "permission denied (EPERM)"),
            SignalError::OtherSignalError(errno) => {
                write!(f, "kill failed with errno {errno}")
            }
        }
    }
}

/// Send `signal` to the process identified by `pid`.
///
/// Returns `Ok(())` on success. On failure, maps `errno` to [`SignalError`]:
/// - `EPERM` → [`SignalError::PermissionDenied`]
/// - anything else → [`SignalError::OtherSignalError`] containing the raw errno value.
fn send_signal(pid: libc::pid_t, signal: libc::c_int) -> Result<(), SignalError> {
    // SAFETY: `kill(2)` is safe to call with any pid/signal combination — invalid values
    // simply return an error via errno rather than causing undefined behavior.
    let ret = unsafe { libc::kill(pid, signal) };
    if ret == 0 {
        return Ok(());
    }
    let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(-1);
    if errno == libc::EPERM {
        Err(SignalError::PermissionDenied)
    } else {
        Err(SignalError::OtherSignalError(errno))
    }
}

// Utilities to daemonize a pingora server, i.e. run the process in the background, possibly
// under a different running user and/or group.

/// Default timeout for the parent to wait for the daemon child to signal readiness.
const DEFAULT_DAEMON_READY_TIMEOUT: Duration = Duration::from_secs(600);

/// How long to sleep between `SIGUSR1` send attempts when `EPERM` is returned.
const NOTIFY_RETRY_INTERVAL: Duration = Duration::from_millis(100);

/// How long to sleep between pid-file liveness checks in the async wait loop.
const LIVENESS_CHECK_INTERVAL: Duration = Duration::from_millis(100);

// XXX: this operation should have been done when the old service is exiting.
// Now the new pid file just kick the old one out of the way
fn move_old_pid(path: &str) {
    if !Path::new(path).exists() {
        debug!("Old pid file does not exist");
        return;
    }
    let new_path = format!("{path}.old");
    match fs::rename(path, &new_path) {
        Ok(()) => {
            debug!("Old pid file renamed");
        }
        Err(e) => {
            error!(
                "failed to rename pid file from {} to {}: {}",
                path, new_path, e
            );
        }
    }
}

/// # Safety
///
/// `name` must be a valid, null-terminated C string. The returned `gid_t` is read from the
/// `passwd` struct returned by `getpwnam(3)`, which points to a static buffer that may be
/// overwritten by subsequent calls to `getpwnam` or `getpwuid`. The caller must not hold the
/// pointer across such calls.
unsafe fn gid_for_username(name: &CString) -> Option<libc::gid_t> {
    // SAFETY: `name` is a valid CString; `getpwnam` returns a pointer to a static buffer
    // or null. We read `pw_gid` immediately and do not retain the pointer.
    let passwd = libc::getpwnam(name.as_ptr() as *const libc::c_char);
    if !passwd.is_null() {
        return Some((*passwd).pw_gid);
    }
    None
}

/// Drop the parent process's UID to the user specified in [`ServerConf::user`].
///
/// The kernel only permits a process to send a signal to another if they share the same UID (or
/// the sender is root). Since the daemon child sends `SIGUSR1` to the parent to signal readiness,
/// the parent must be running as the same UID as the child by the time that signal arrives —
/// otherwise the kernel will reject it with `EPERM`.
///
/// This function is called in the `Outcome::Parent` path immediately after `execute()` returns,
/// before the parent enters its readiness wait loop, so the parent's UID matches the child's as
/// quickly as possible after the fork.
///
/// Only the UID is changed; the GID is left as-is. Signal permission checks are based on UID,
/// so changing the GID is not necessary for this purpose.
///
/// Logs an error and continues if the user cannot be resolved or `setuid` fails — the parent
/// is short-lived and about to exit, so a failed privilege drop is non-fatal. The child's
/// `EPERM` retry window (see [`ServerConf::daemon_notify_timeout_seconds`]) exists precisely to
/// cover the small gap between the fork and the parent completing this UID change.
fn drop_privileges_in_parent(conf: &ServerConf) -> Result<()> {
    let Some(user) = conf.user.as_ref() else {
        return Ok(());
    };

    let user_cstr = CString::new(user.as_str()).or_err_with(ErrorType::Custom("Daemon"), || {
        format!("drop_privileges_in_parent: user '{user}' invalid")
    })?;

    // SAFETY: `user_cstr` is a valid CString. `getpwnam` returns a pointer to a static
    // buffer or null. We read `pw_uid` immediately and do not retain the pointer.
    let passwd = unsafe { libc::getpwnam(user_cstr.as_ptr() as *const libc::c_char) };
    if passwd.is_null() {
        return Error::e_explain(
            ErrorType::Custom("Daemon"),
            format!("drop_privileges_in_parent: user '{user}' not found"),
        );
    }

    // SAFETY: `passwd` was checked for null above. We dereference it once to read `pw_uid`.
    let uid = unsafe { (*passwd).pw_uid };
    // SAFETY: `setuid(2)` is safe to call with any uid — invalid values return an error.
    let ret = unsafe { libc::setuid(uid) };
    if ret == 0 {
        Ok(())
    } else {
        Error::e_explain(
            ErrorType::Custom("Daemon"),
            format!(
                "drop_privileges_in_parent: setuid({uid}) failed: {}",
                std::io::Error::last_os_error()
            ),
        )
    }
}

/// Outcome of calling [`daemonize`].
///
/// When [`ServerConf::daemon_wait_for_ready`] is `true`, the child process must call
/// [`notify_parent_ready_for_fds`] after bootstrap completes to unblock the parent's wait loop.
pub struct DaemonizeResult {
    /// The PID of the original parent process to notify via `SIGUSR1` after bootstrap completes.
    ///
    /// `Some` when [`ServerConf::daemon_wait_for_ready`] is `true`, `None` otherwise.
    pub notify_parent_pid: Option<u32>,
}

/// Start a server instance as a daemon.
///
/// Both code paths use [`daemonize::Daemonize::execute()`] rather than calling `fork()` directly.
/// `execute()` returns an [`Outcome`] to the caller in each process rather than having the parent
/// exit inside the crate, which gives us the opportunity to run additional logic in the parent
/// before it exits.
///
/// When [`ServerConf::daemon_wait_for_ready`] is `false` (the default), the parent exits
/// immediately — matching the behavior of `start()`.
///
/// When `daemon_wait_for_ready` is `true`, the parent registers a `SIGUSR1` handler before
/// forking, then waits (in a sleep loop polling the pid file and the signal flag) for up to
/// [`ServerConf::daemon_ready_timeout_seconds`] (default 600 s) for the grandchild to send
/// `SIGUSR1`. On success the parent exits with code 0. On timeout, or if the daemon process
/// exits before signaling, the parent exits with code 1, causing systemd to abort the reload.
///
/// Returns a [`DaemonizeResult`] that is only meaningful to the child process. The parent always
/// exits before returning.
pub fn daemonize(conf: &ServerConf) -> DaemonizeResult {
    // Capture the parent PID before forking so it can be passed to the grandchild. The
    // grandchild sends SIGUSR1 to this PID after bootstrap completes.
    let parent_pid = if conf.daemon_wait_for_ready {
        Some(process::id())
    } else {
        None
    };

    move_old_pid(&conf.pid_file);

    match build_daemonize(conf).execute() {
        Outcome::Parent(result) => {
            result.unwrap_or_else(|e| panic!("Daemonize failed: {e}"));
        }
        Outcome::Child(result) => {
            result.unwrap_or_else(|e| panic!("Daemonize child setup failed: {e}"));
            return DaemonizeResult {
                notify_parent_pid: parent_pid,
            };
        }
    }

    if conf.daemon_wait_for_ready {
        // Drop root privileges before waiting so the parent does not linger as root.
        if let Err(e) = drop_privileges_in_parent(conf) {
            error!("drop_privileges_in_parent failed: {e}");

            // Exiting the parent process should be fine because if downgrading
            // the user's privileges fails here, it will fail in the child and
            // the child will exit too
            process::exit(1);
        }

        let timeout = conf
            .daemon_ready_timeout_seconds
            .map(|n| Duration::from_secs(n.get()))
            .unwrap_or(DEFAULT_DAEMON_READY_TIMEOUT);

        info!(
            "Waiting up to {:?} for daemon to signal readiness via SIGUSR1",
            timeout
        );

        wait_for_ready_or_exit(&conf.pid_file, timeout);
    }

    process::exit(0);
}

/// Build a single-threaded tokio runtime for the parent's signal wait loop.
///
/// The parent process is short-lived and only needs to wait for a signal and check the pid file.
/// A current-thread runtime avoids spawning worker threads in a process that is about to exit.
fn build_parent_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("failed to build tokio runtime for parent signal wait")
}

/// Wait for the daemon grandchild to send `SIGUSR1`, up to `timeout`.
///
/// Uses a local tokio runtime with [`tokio::signal::unix`] to listen for `SIGUSR1` instead of
/// raw signal handlers and polling loops. The daemon's PID is checked periodically via the pid
/// file — if the process exits before signaling, the parent aborts.
///
/// Exits the process directly:
/// - exit code 0 if `SIGUSR1` is received (daemon is ready).
/// - exit code 1 if `timeout` elapses (daemon took too long).
/// - exit code 1 if the pid file exists and the process is no longer running.
fn wait_for_ready_or_exit(pid_file: &str, timeout: Duration) {
    let rt = build_parent_runtime();
    let pid_file = pid_file.to_owned();

    rt.block_on(async move {
        use tokio::signal::unix::{signal, SignalKind};
        use tokio::time::{interval, timeout as tokio_timeout};

        let mut sigusr1 =
            signal(SignalKind::user_defined1()).expect("failed to register SIGUSR1 listener");

        let mut liveness_check = interval(LIVENESS_CHECK_INTERVAL);
        let mut daemon_pid: Option<libc::pid_t> = None;

        let result = tokio_timeout(timeout, async {
            loop {
                tokio::select! {
                    _ = sigusr1.recv() => {
                        info!("Daemon signaled readiness, parent exiting");
                        return;
                    }
                    _ = liveness_check.tick() => {
                        if daemon_pid.is_none() {
                            daemon_pid = try_read_pid_file(&pid_file);
                        }
                        if let Some(pid) = daemon_pid {
                            if !process_is_running(pid) {
                                error!(
                                    "Daemon process (pid {pid}) is no longer running \
                                     before signaling readiness, aborting"
                                );
                                process::exit(1);
                            }
                        }
                    }
                }
            }
        })
        .await;

        if result.is_err() {
            error!("Daemon did not signal readiness within {timeout:?}, aborting");
            process::exit(1);
        }
    });
}

/// Notify the parent process that the daemon is ready to serve traffic by sending `SIGUSR1`.
///
/// Should be called by the daemon process after bootstrap is complete when
/// [`ServerConf::daemon_wait_for_ready`] is `true`. `parent_pid` is the PID of the original
/// process captured before the fork and stored in [`DaemonizeResult::notify_parent_pid`].
///
/// `SIGUSR1` sets an atomic flag that the parent's wait loop checks, causing it to exit with
/// code 0 and allowing systemd to proceed with the next step of the service reload.
///
/// If `kill(2)` returns `EPERM` — which can happen transiently when the child's UID has just
/// been changed by `setuid` and the kernel hasn't yet updated the credential check — the
/// function sleeps for [`NOTIFY_RETRY_INTERVAL`] (100 ms) and retries until `notify_timeout`
/// elapses, at which point it logs an error and returns. Any other error (e.g. `ESRCH`,
/// meaning the parent no longer exists) is logged and the function returns immediately without
/// retrying.
pub fn notify_parent_ready_for_fds(parent_pid: u32, notify_timeout: Duration) {
    let parent_pid = parent_pid as libc::pid_t;
    info!(
        "Sending SIGUSR1 to parent process (pid {}) to signal daemon readiness",
        parent_pid
    );

    let start = Instant::now();

    while start.elapsed() < notify_timeout {
        match send_signal(parent_pid, libc::SIGUSR1) {
            Ok(()) => return,
            Err(SignalError::PermissionDenied) => {
                debug!(
                    "Permission denied sending SIGUSR1 to parent (pid {}), retrying in {:?}",
                    parent_pid, NOTIFY_RETRY_INTERVAL
                );
                thread::sleep(NOTIFY_RETRY_INTERVAL);
            }
            Err(SignalError::OtherSignalError(errno)) => {
                error!(
                    "Failed to send SIGUSR1 to parent (pid {}): errno {errno}",
                    parent_pid
                );
                return;
            }
        }
    }

    error!(
        "Permission denied sending SIGUSR1 to parent (pid {}), giving up after {:?}",
        parent_pid, notify_timeout
    );
}

/// Try to read a PID from `pid_file`. Returns `None` if the file does not exist or cannot be
/// parsed.
fn try_read_pid_file(pid_file: &str) -> Option<libc::pid_t> {
    fs::read_to_string(pid_file)
        .ok()
        .and_then(|c| c.trim().parse().ok())
}

/// Returns `true` if a process with `pid` is currently running.
fn process_is_running(pid: libc::pid_t) -> bool {
    // Signal 0 does not send a signal; it just checks whether the process exists and whether
    // we have permission to signal it. EPERM (no permission) is not possible here because
    // drop_privileges_in_parent guarantees the parent has already dropped to the same user as
    // the daemon child before this function is called.
    send_signal(pid, 0).is_ok()
}

/// Build a [`Daemonize`] instance configured from `conf`, without calling `start()` or
/// `execute()`. The caller is responsible for driving execution.
fn build_daemonize(conf: &ServerConf) -> Daemonize<()> {
    // TODO: customize working dir

    let daemonize = Daemonize::new()
        .umask(0o007) // allow same group to access files but not everyone else
        .pid_file(&conf.pid_file);

    let daemonize = if let Some(error_log) = conf.error_log.as_ref() {
        let err = OpenOptions::new()
            .append(true)
            .create(true)
            // open read() in case there are no readers
            // available otherwise we will panic with
            // an ENXIO since O_NONBLOCK is set
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(error_log)
            .unwrap();
        daemonize.stderr(err)
    } else {
        daemonize.stdout(Stdio::keep()).stderr(Stdio::keep())
    };

    let daemonize = match conf.user.as_ref() {
        Some(user) => {
            let user_cstr = CString::new(user.as_str()).unwrap();

            // SAFETY: `user_cstr` is a valid CString. See `gid_for_username` safety docs.
            #[cfg(target_os = "macos")]
            let group_id = unsafe { gid_for_username(&user_cstr).map(|gid| gid as i32) };
            #[cfg(target_os = "freebsd")]
            let group_id = unsafe { gid_for_username(&user_cstr).map(|gid| gid as u32) };
            #[cfg(target_os = "linux")]
            let group_id = unsafe { gid_for_username(&user_cstr) };

            daemonize
                .privileged_action(move || {
                    if let Some(gid) = group_id {
                        // SAFETY: `user_cstr` is a valid CString captured by the closure.
                        // `initgroups(3)` is safe to call with a valid username and gid.
                        unsafe {
                            libc::initgroups(user_cstr.as_ptr() as *const libc::c_char, gid);
                        }
                    }
                })
                .user(user.as_str())
                .chown_pid_file(true)
        }
        None => daemonize,
    };

    match conf.group.as_ref() {
        Some(group) => daemonize.group(group.as_str()),
        None => daemonize,
    }
}
