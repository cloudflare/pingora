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

use once_cell::sync::Lazy;
use std::path::Path;
use std::process;
use std::{thread, time};

pub static MOCK_ORIGIN: Lazy<bool> = Lazy::new(init);

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
        std::fs::copy(Path::new(&src_cert_path), Path::new(&dst_cert_path));
        dst_cert_path = format!(
            "{}/tests/utils/conf/keys/server.crt",
            env!("CARGO_MANIFEST_DIR")
        );
        std::fs::copy(Path::new(&src_cert_path), Path::new(&dst_cert_path));
    }

    // TODO: figure out a way to kill openresty when exiting
    process::Command::new("pkill")
        .args(["-F", "/tmp/pingora_mock_origin.pid"])
        .spawn()
        .unwrap()
        .wait();
    let _origin = thread::spawn(|| {
        process::Command::new("openresty")
            .args(["-p", &format!("{}/origin", super::conf_dir())])
            .output()
            .unwrap();
    });
    // Wait until openresty is accepting connections, then give it a moment
    // to finish worker initialization.
    let deadline = time::Instant::now() + time::Duration::from_secs(10);
    while time::Instant::now() < deadline {
        if std::net::TcpStream::connect_timeout(
            &"127.0.0.1:8000".parse().unwrap(),
            time::Duration::from_millis(100),
        )
        .is_ok()
        {
            // Port is listening; allow a brief window for workers to finish
            // initializing before tests start sending real requests.
            thread::sleep(time::Duration::from_millis(500));
            return true;
        }
        thread::sleep(time::Duration::from_millis(50));
    }
    panic!("mock origin (openresty) failed to start within 10s");
}
