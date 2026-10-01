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

use std::path::Path;
use std::process::{self, Stdio};
use std::sync::LazyLock;
use std::{thread, time};

pub(crate) fn install_certificate(source: &Path, destination: &Path) -> std::io::Result<()> {
    // Other integration test processes can load this certificate concurrently.
    let staged = tempfile::NamedTempFile::new_in(destination.parent().unwrap_or(Path::new(".")))?;
    std::fs::copy(source, staged.path())?;
    staged.persist(destination).map_err(|error| error.error)?;
    Ok(())
}

/// Written by the mock origin's nginx.conf (`pid`).
const PID_FILE: &str = "/tmp/pingora_mock_origin.pid";

pub static MOCK_ORIGIN: LazyLock<bool> = LazyLock::new(init);

fn init() -> bool {
    #[cfg(feature = "rustls")]
    let src_cert_path = format!(
        "{}/tests/utils/conf/keys/server_rustls.crt",
        env!("CARGO_MANIFEST_DIR")
    );
    #[cfg(feature = "openssl_derived")]
    let src_cert_path = format!(
        "{}/tests/utils/conf/keys/server_boringssl_openssl.crt",
        env!("CARGO_MANIFEST_DIR")
    );
    #[cfg(feature = "s2n")]
    let src_cert_path = format!(
        "{}/tests/utils/conf/keys/server_s2n.crt",
        env!("CARGO_MANIFEST_DIR")
    );

    #[cfg(feature = "any_tls")]
    {
        let mut dst_cert_path = format!("{}/tests/keys/server.crt", env!("CARGO_MANIFEST_DIR"));
        install_certificate(Path::new(&src_cert_path), Path::new(&dst_cert_path)).unwrap();
        dst_cert_path = format!(
            "{}/tests/utils/conf/keys/server.crt",
            env!("CARGO_MANIFEST_DIR")
        );
        install_certificate(Path::new(&src_cert_path), Path::new(&dst_cert_path)).unwrap();
    }

    // Every test process restarts openresty. The previous instance keeps its listen ports
    // until it has exited, so a new instance started before then fails to bind while the
    // readiness probe below is still answered by the old one, and requests later find no
    // listener at all. Stop it and wait for it to exit first.
    // TODO: figure out a way to kill openresty when exiting
    stop_previous_origin();
    let deadline = time::Instant::now() + time::Duration::from_secs(10);
    while time::Instant::now() < deadline {
        let mut origin = process::Command::new("openresty")
            .args(["-p", &format!("{}/origin", super::conf_dir())])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        // Wait until this instance listens. If it exits instead, a port was still bound:
        // start it again.
        loop {
            if matches!(origin.try_wait(), Ok(Some(_))) {
                break;
            }
            if std::net::TcpStream::connect_timeout(
                &"127.0.0.1:8000".parse().unwrap(),
                time::Duration::from_millis(100),
            )
            .is_ok()
            {
                // Port is listening; allow a brief window for workers to finish
                // initializing before tests start sending real requests. The instance
                // outlives this process, as before; reap it if it exits sooner.
                thread::sleep(time::Duration::from_millis(500));
                thread::spawn(move || origin.wait());
                return true;
            }
            if time::Instant::now() >= deadline {
                let _ = origin.kill();
                break;
            }
            thread::sleep(time::Duration::from_millis(50));
        }
        let _ = origin.wait();
        thread::sleep(time::Duration::from_millis(100));
    }
    panic!("mock origin (openresty) failed to start within 10s");
}

/// Stop the openresty a previous test process started, and wait until it has exited.
fn stop_previous_origin() {
    let Some(pid) = std::fs::read_to_string(PID_FILE)
        .ok()
        .and_then(|pid| pid.trim().parse::<u32>().ok())
    else {
        return;
    };
    let pid = pid.to_string();
    let signalled = |args: &[&str]| {
        process::Command::new("kill")
            .args(args)
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    };
    if !signalled(&[pid.as_str()]) {
        return;
    }
    let deadline = time::Instant::now() + time::Duration::from_secs(5);
    while time::Instant::now() < deadline && signalled(&["-0", pid.as_str()]) {
        thread::sleep(time::Duration::from_millis(20));
    }
}
