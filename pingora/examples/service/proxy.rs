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

use crate::app::proxy::ProxyApp;
use pingora_core::listeners::Listeners;
use pingora_core::services::listening::Service;
use pingora_core::upstreams::peer::BasicPeer;

pub fn proxy_service(addr: &str, proxy_addr: &str) -> Service<ProxyApp> {
    let proxy_to = BasicPeer::new(proxy_addr);

    Service::with_listeners(
        "Proxy Service".to_string(),
        Listeners::tcp(addr),
        ProxyApp::new(proxy_to),
    )
}

pub fn proxy_service_tls(
    addr: &str,
    proxy_addr: &str,
    proxy_sni: &str,
    cert_path: &str,
    key_path: &str,
) -> Service<ProxyApp> {
    let mut proxy_to = BasicPeer::new(proxy_addr);
    // set SNI to enable TLS
    proxy_to.sni = proxy_sni.into();
    Service::with_listeners(
        "Proxy Service TLS".to_string(),
        Listeners::tls(addr, cert_path, key_path).unwrap(),
        ProxyApp::new(proxy_to),
    )
}
