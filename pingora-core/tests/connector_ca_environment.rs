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

#![cfg(any(feature = "openssl", feature = "boringssl"))]

use pingora_core::connectors::TransportConnector;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

#[test]
fn connector_initialization_does_not_modify_ca_environment() {
    let before_file = std::env::var_os("SSL_CERT_FILE");
    let before_dir = std::env::var_os("SSL_CERT_DIR");
    let running = Arc::new(AtomicBool::new(true));
    let reader_running = Arc::clone(&running);
    let reader = std::thread::spawn(move || {
        while reader_running.load(Ordering::Relaxed) {
            let _ = std::env::var_os("SSL_CERT_FILE");
            let _ = std::env::var_os("SSL_CERT_DIR");
        }
    });

    let _connector = TransportConnector::new(None);
    running.store(false, Ordering::Relaxed);
    reader.join().unwrap();

    assert_eq!(std::env::var_os("SSL_CERT_FILE"), before_file);
    assert_eq!(std::env::var_os("SSL_CERT_DIR"), before_dir);
}
