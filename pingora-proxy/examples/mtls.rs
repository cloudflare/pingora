// Copyright 2025 Cloudflare, Inc.
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

use std::sync::Arc;

use boring::{
    pkey::PKey,
    ssl::{SslAlert, SslVerifyError, SslVerifyMode},
    x509::X509,
};
use log::{debug, info};
use pingora_core::{
    listeners::{tls::TlsSettings, TlsAccept},
    prelude::{HttpPeer, Opt},
    protocols::tls::TlsRef,
    server::Server as PingoraServer,
    upstreams::peer::PeerOptions,
    utils::tls::CertKey,
    Result,
};
use pingora_proxy::{ProxyHttp, Session};

// This is the client that will connect to the server using mTLS. It forwards all requests to the server
// after the mTLS handshake is successful.
struct Client;

#[async_trait::async_trait]
impl ProxyHttp for Client {
    type CTX = ();
    fn new_ctx(&self) -> Self::CTX {}

    async fn upstream_peer(
        &self,
        _session: &mut pingora_proxy::Session,
        _ctx: &mut Self::CTX,
    ) -> Result<Box<HttpPeer>> {
        let mut peer = HttpPeer::new("localhost:8081", true, "globeandcitizen.com".to_string()); // the SNI should point to the target cert's host value provided

        // We need to present the client certificate to the server for the mTLS handshake.
        {
            let mut peer_options = PeerOptions::new();

            let ca_cert = X509::from_pem(cert::CA_CERT).unwrap();

            peer_options.verify_cert = true; // Verify the server's certificate
            peer_options.verify_hostname = true; // Whether to check if upstream server cert's Host matches
            peer_options.ca = Some(Arc::new(Box::new([ca_cert]))); // CA cert to verify server's certificate
            peer.options = peer_options;

            let client_credentials = {
                let client_cert = X509::from_pem(cert::CLIENT_CERT).unwrap();
                let key = boring::pkey::PKey::private_key_from_pem(cert::CLIENT_KEY).unwrap();
                CertKey::new(vec![client_cert], key)
            };

            peer.client_cert_key = Some(Arc::new(client_credentials));
        }

        Ok(Box::new(peer))
    }

    async fn request_filter(&self, _session: &mut Session, _ctx: &mut Self::CTX) -> Result<bool>
    where
        Self::CTX: Send + Sync,
    {
        Ok(false)
    }
}

struct Server;

#[async_trait::async_trait]
impl ProxyHttp for Server {
    type CTX = ();
    fn new_ctx(&self) -> Self::CTX {}

    async fn upstream_peer(
        &self,
        _session: &mut pingora_proxy::Session,
        _ctx: &mut Self::CTX,
    ) -> Result<Box<HttpPeer>> {
        Ok(Box::new(HttpPeer::new(
            "localhost:8080",
            true,
            "globeandcitizen.com".to_string(),
        )))
    }
}

// This will provide the server-side TLS configuration and handle the mTLS handshake.
struct ServerTls;

impl ServerTls {
    pub fn verify_client_cert(ssl: &mut TlsRef) -> Result<(), SslVerifyError> {
        if ssl.verify_mode() != SslVerifyMode::PEER {
            log::error!("SSL verify mode is not set to PEER, cannot verify client certificate");
            return Err(SslVerifyError::Invalid(SslAlert::INTERNAL_ERROR));
        }

        let client_cert = match ssl.peer_certificate() {
            Some(val) => val,
            None => {
                log::error!("Failed to get client certificate");
                return Err(SslVerifyError::Invalid(SslAlert::NO_CERTIFICATE));
            }
        };

        debug!("Client certificate: {:?}", client_cert.subject_name());

        let ca_cert = X509::from_pem(cert::CA_CERT)
            .map_err(|e| SslVerifyError::Invalid(SslAlert::INTERNAL_ERROR))?
            .public_key()
            .unwrap();

        if !client_cert.verify(&ca_cert).unwrap() {
            log::error!("Client certificate verification failed");
            return Err(SslVerifyError::Invalid(SslAlert::BAD_CERTIFICATE));
        }

        info!("Client certificate verified successfully");
        Ok(())
    }
}

#[async_trait::async_trait]
impl TlsAccept for ServerTls {
    async fn certificate_callback(&self, ssl: &mut TlsRef) {
        // set the hostname for the SSL context
        ssl.set_hostname("globeandcitizen.com").unwrap();

        // provide the server private key to the SSL context
        let server_key = PKey::private_key_from_pem(cert::SERVER_KEY).unwrap();
        ssl.set_private_key(&server_key).unwrap();

        // provide the server certificate to the SSL context
        let server_cert = X509::from_pem(cert::SERVER_CERT).unwrap();
        ssl.set_certificate(&server_cert).unwrap();

        // set the custom callback to verify the client certificate
        ssl.set_custom_verify_callback(SslVerifyMode::PEER, Self::verify_client_cert);
    }
}

#[tokio::main]
async fn main() {
    // mTLS Steps:
    // 1. Client connects to server
    // 2. Server presents its TLS certificate
    // 3. Client verifies the server's certificate
    // 4. Client presents its TLS certificate
    // 5. Server verifies the client's certificate
    // 6. Server grants access
    // 7. Client and server exchange information over encrypted TLS connection

    // setting up the client; localhost:8080 is the server address

    let client = {
        let mut client = PingoraServer::new(Some(Opt {
            conf: Some(format!("./mtls_assets/conf.yml")),
            ..Default::default()
        }))
        .unwrap();

        client.bootstrap();
        let mut client_proxy =
            pingora_proxy::http_proxy_service_with_name(&client.configuration, Client, "client");

        client_proxy
            .add_tls(
                "localhost:8080",
                "./mtls_assets/client.pem",
                "./mtls_assets/client.key",
            )
            .unwrap();

        client.add_service(client_proxy);
        client
    };

    // set up the server: localhost:8081 is the upstream server address
    let server = {
        let mut server = PingoraServer::new(Some(Opt {
            conf: Some(format!("./mtls_assets/conf.yml")),
            ..Default::default()
        }))
        .unwrap();

        server.bootstrap();
        let mut server_proxy =
            pingora_proxy::http_proxy_service_with_name(&server.configuration, Server, "server");

        server_proxy.add_tls_with_settings(
            "localhost:8081",
            None,
            TlsSettings::with_callbacks(Box::new(ServerTls)).unwrap(), // Use the custom TLS acceptor for mTLS
        );

        server.add_service(server_proxy);
        server
    };

    // let (client_tx, client)
}

mod cert {
    // This is the ca cert that has been used to sign the client and server certs.
    pub const CA_CERT: &[u8] = include_bytes!("./mtls_assets/ca.pem");

    // This is the client cert that has been signed by the CA cert.
    pub const CLIENT_CERT: &[u8] = include_bytes!("./mtls_assets/client.pem");

    // This is the private key for the client cert. Would normally be kept secret.
    pub const CLIENT_KEY: &[u8] = include_bytes!("./mtls_assets/client.key");

    // This is the server cert that has been signed by the CA cert.
    pub const SERVER_CERT: &[u8] = include_bytes!("./mtls_assets/server.pem");

    // This is the private key for the server cert. Would normally be kept secret.
    pub const SERVER_KEY: &[u8] = include_bytes!("./mtls_assets/server.key");
}
