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

use daemonize::Daemonize;
use log::{debug, error};
use std::ffi::CString;
use std::fs::{self, OpenOptions};
use std::os::unix::prelude::OpenOptionsExt;
use std::path::Path;

use crate::server::configuration::ServerConf;

// Utilities to daemonize a pingora server, i.e. run the process in the background, possibly
// under a different running user and/or group.

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

unsafe fn gid_for_username(name: &CString) -> Option<libc::gid_t> {
    let passwd = libc::getpwnam(name.as_ptr() as *const libc::c_char);
    if !passwd.is_null() {
        return Some((*passwd).pw_gid);
    }
    None
}

/// Start a server instance as a daemon.
#[cfg(unix)]
pub fn daemonize(conf: &ServerConf) {
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
        daemonize
    };

    let daemonize = match conf.user.as_ref() {
        Some(user) => {
            let user_cstr = CString::new(user.as_str()).unwrap();

            #[cfg(target_os = "macos")]
            let group_id = unsafe { gid_for_username(&user_cstr).map(|gid| gid as i32) };
            #[cfg(target_os = "freebsd")]
            let group_id = unsafe { gid_for_username(&user_cstr).map(|gid| gid as u32) };
            #[cfg(target_os = "linux")]
            let group_id = unsafe { gid_for_username(&user_cstr) };

            daemonize
                .privileged_action(move || {
                    if let Some(gid) = group_id {
                        // Set the supplemental group privileges for the child process.
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

    let daemonize = match conf.group.as_ref() {
        Some(group) => daemonize.group(group.as_str()),
        None => daemonize,
    };

    move_old_pid(&conf.pid_file);

    daemonize.start().unwrap(); // hard crash when fail
}
