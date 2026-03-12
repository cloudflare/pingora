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

#![cfg_attr(not(feature = "openssl"), allow(unused))]

use std::any::Any;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::Arc;

use async_trait::async_trait;
use clap::Parser;
use http::header::{CONTENT_LENGTH, CONTENT_TYPE};
use http::{Response, StatusCode};
use pingora_core::apps::http_app::ServeHttp;
use pingora_core::listeners::tls::TlsSettings;
use pingora_core::listeners::TlsAccept;
use pingora_core::protocols::http::ServerSession;
use pingora_core::protocols::tls::TlsRef;
use pingora_core::server::configuration::Opt;
use pingora_core::server::Server;
use pingora_core::services::listening::Service;
use pingora_core::Result;
#[cfg(feature = "openssl")]
use pingora_openssl::{
    nid::Nid,
    ssl::{NameType, SslFiletype, SslVerifyMode},
    x509::{GeneralName, X509Name},
};

// Custom structure to hold TLS information
struct MyTlsInfo {
    // SNI (Server Name Indication) from the TLS handshake
    sni: Option<String>,
    // SANs (Subject Alternative Names) from client certificate
    sans: Vec<String>,
    // Common Name (CN) from client certificate
    common_name: Option<String>,
}

struct MyApp;

#[async_trait]
impl ServeHttp for MyApp {
    async fn response(&self, session: &mut ServerSession) -> http::Response<Vec<u8>> {
        static EMPTY_VEC: Vec<String> = vec![];

        // Extract TLS info from the session's digest extensions
        let my_tls_info = session
            .digest()
            .and_then(|digest| digest.ssl_digest.as_ref())
            .and_then(|ssl_digest| ssl_digest.extension.get::<MyTlsInfo>());
        let sni = my_tls_info
            .and_then(|my_tls_info| my_tls_info.sni.as_deref())
            .unwrap_or("<none>");
        let sans = my_tls_info
            .map(|my_tls_info| &my_tls_info.sans)
            .unwrap_or(&EMPTY_VEC);
        let common_name = my_tls_info
            .and_then(|my_tls_info| my_tls_info.common_name.as_deref())
            .unwrap_or("<none>");

        // Create response message
        let mut message = String::new();
        message += &format!("Your SNI was: {sni}\n");
        message += &format!("Your SANs were: {sans:?}\n");
        message += &format!("Client Common Name (CN): {}\n", common_name);
        let message = message.into_bytes();

        Response::builder()
            .status(StatusCode::OK)
            .header(CONTENT_TYPE, "text/plain")
            .header(CONTENT_LENGTH, message.len())
            .body(message)
            .unwrap()
    }
}

struct MyTlsCallbacks;

#[async_trait]
impl TlsAccept for MyTlsCallbacks {
    #[cfg(feature = "openssl")]
    async fn handshake_complete_callback(
        &self,
        tls_ref: &TlsRef,
    ) -> Option<Arc<dyn Any + Send + Sync>> {
        // Here you can inspect the TLS connection and return an extension if needed.

        // Extract SNI (Server Name Indication)
        let sni = tls_ref
            .servername(NameType::HOST_NAME)
            .map(ToOwned::to_owned);

        // Extract SAN (Subject Alternative Names) from the client certificate
        let sans = tls_ref
            .peer_certificate()
            .and_then(|cert| cert.subject_alt_names())
            .map_or(vec![], |sans| {
                sans.into_iter()
                    .filter_map(|san| san_to_string(&san))
                    .collect::<Vec<_>>()
            });

        // Extract Common Name (CN) from the client certificate
        let common_name = tls_ref.peer_certificate().and_then(|cert| {
            let cn = cert.subject_name().entries_by_nid(Nid::COMMONNAME).next()?;
            Some(cn.data().as_utf8().ok()?.to_string())
        });

        let tls_info = MyTlsInfo {
            sni,
            sans,
            common_name,
        };
        Some(Arc::new(tls_info))
    }
}

// Convert GeneralName of SAN to String representation
#[cfg(feature = "openssl")]
fn san_to_string(san: &GeneralName) -> Option<String> {
    if let Some(dnsname) = san.dnsname() {
        return Some(dnsname.to_owned());
    }
    if let Some(uri) = san.uri() {
        return Some(uri.to_owned());
    }
    if let Some(email) = san.email() {
        return Some(email.to_owned());
    }
    if let Some(ip) = san.ipaddress() {
        return bytes_to_ip_addr(ip).map(|addr| addr.to_string());
    }
    None
}

// Convert byte slice to IpAddr
fn bytes_to_ip_addr(bytes: &[u8]) -> Option<IpAddr> {
    match bytes.len() {
        4 => {
            let addr = Ipv4Addr::new(bytes[0], bytes[1], bytes[2], bytes[3]);
            Some(IpAddr::V4(addr))
        }
        16 => {
            let mut octets = [0u8; 16];
            octets.copy_from_slice(bytes);
            let addr = Ipv6Addr::from(octets);
            Some(IpAddr::V6(addr))
        }
        _ => None,
    }
}

// This example demonstrates an HTTP server that requires client certificates.
// The server extracts the SNI (Server Name Indication) from the TLS handshake and
// SANs (Subject Alternative Names) from the client certificate, then returns them
// as part of the HTTP response.
//
// ## How to run
//
//   cargo run -F openssl --example client_cert
//
//   # In another terminal, run the following command to test the server:
//   cd pingora-core
//   curl -k -i \
//     --cert examples/keys/clients/cert-1.pem --key examples/keys/clients/key-1.pem \
//     --resolve myapp.example.com:6196:127.0.0.1 \
//     https://myapp.example.com:6196/
//   curl -k -i \
//     --cert examples/keys/clients/cert-2.pem --key examples/keys/clients/key-2.pem \
//     --resolve myapp.example.com:6196:127.0.0.1 \
//     https://myapp.example.com:6196/
//   curl -k -i \
//     --cert examples/keys/clients/invalid-cert.pem --key examples/keys/clients/invalid-key.pem \
//     --resolve myapp.example.com:6196:127.0.0.1 \
//     https://myapp.example.com:6196/
#[cfg(feature = "openssl")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::init();

    // read command line arguments
    let opt = Opt::parse();
    let mut my_server = Server::new(Some(opt))?;
    my_server.bootstrap();

    let mut my_app = Service::new("my app".to_owned(), MyApp);

    // Paths to server certificate, private key, and client CA certificate
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let server_cert_path = format!("{manifest_dir}/examples/keys/server/cert.pem");
    let server_key_path = format!("{manifest_dir}/examples/keys/server/key.pem");
    let client_ca_path = format!("{manifest_dir}/examples/keys/client-ca/cert.pem");

    // Create TLS settings with callbacks
    let callbacks = Box::new(MyTlsCallbacks);
    let mut tls_settings = TlsSettings::with_callbacks(callbacks)?;
    // Set server certificate and private key
    tls_settings.set_certificate_chain_file(&server_cert_path)?;
    tls_settings.set_private_key_file(server_key_path, SslFiletype::PEM)?;
    // Require client certificate
    tls_settings.set_verify(SslVerifyMode::PEER | SslVerifyMode::FAIL_IF_NO_PEER_CERT);
    // Set CA for client certificate verification
    tls_settings.set_ca_file(&client_ca_path)?;
    // Optionally, set the list of acceptable client CAs sent to the client
    tls_settings.set_client_ca_list(X509Name::load_client_ca_file(&client_ca_path)?);

    my_app.add_tls_with_settings("0.0.0.0:6196", None, tls_settings);
    my_server.add_service(my_app);

    my_server.run_forever();
}

#[cfg(not(feature = "openssl"))]
fn main() {
    eprintln!("This example requires the 'openssl' feature to be enabled.");
}
