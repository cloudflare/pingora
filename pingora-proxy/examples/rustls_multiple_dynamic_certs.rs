#![cfg(feature = "rustls")]

use async_trait::async_trait;
use bytes::Bytes;
use log::info;
use std::{collections::HashMap, sync::Arc};

use arc_swap::ArcSwap;
use pingora_core::server::configuration::Opt;
use pingora_core::server::Server;
use pingora_core::upstreams::peer::HttpPeer;
use pingora_core::Result;
use pingora_proxy::{ProxyHttp, Session};
use pingora_rustls::{CertifiedKey, CryptoProvider};

struct MyProxyHttp;

#[async_trait]
impl ProxyHttp for MyProxyHttp {
    type CTX = ();
    fn new_ctx(&self) -> Self::CTX {}

    async fn request_filter(&self, session: &mut Session, _ctx: &mut Self::CTX) -> Result<bool> {
        let host = session.req_header().uri.host();

        let _ = session
            .respond_error_with_body(
                503,
                Bytes::from_owner(format!("req header host:{}", host.unwrap_or_default())),
            )
            .await;

        return Ok(true);
    }

    async fn upstream_peer(
        &self,
        _session: &mut Session,
        _ctx: &mut Self::CTX,
    ) -> Result<Box<HttpPeer>> {
        unreachable!()
    }
}

// RUST_LOG=INFO cargo run --example rustls_multiple_dynamic_certs --features rustls

fn main() {
    env_logger::init();

    let opt = Opt::parse_args();
    let mut my_server = Server::new(Some(opt)).unwrap();
    my_server.bootstrap();

    let mut my_proxy = pingora_proxy::http_proxy_service(&my_server.configuration, MyProxyHttp);

    pingora_rustls::install_default_crypto_provider();
    let crypto_provider = CryptoProvider::get_default().expect("Never");

    let storage = make_storage(&crypto_provider).unwrap();
    let storage = Arc::new(ArcSwap::new(Arc::new(storage)));

    {
        let storage = Arc::clone(&storage);
        // Sync or Async
        std::thread::spawn(move || loop {
            std::thread::sleep(std::time::Duration::from_secs(5));

            let new_storage = make_storage(&crypto_provider).unwrap();
            storage.store(Arc::new(new_storage));

            info!(
                "storage updated at {:?}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .expect("Never")
            );
        });
    }

    let mut tls_settings =
        pingora_core::listeners::tls::TlsSettings::intermediate_with_multiple(storage).unwrap();
    tls_settings.enable_h2();
    my_proxy.add_tls_with_settings("0.0.0.0:6197", None, tls_settings);

    info!("====================================");
    info!("Server starting with rustls_multiple_dynamic_certs");
    info!("====================================");
    info!("");
    info!("Test with:");
    info!("  curl --resolve openrusty.org:6197:127.0.0.1 https://openrusty.org:6197/ -v -k");
    info!(
        "  curl --resolve www.openrusty.org:6197:127.0.0.1 https://www.openrusty.org:6197/ -v -k"
    );
    info!("  curl --resolve example.com:6197:127.0.0.1 https://example.com:6197/ -v");
    info!("");

    my_server.add_service(my_proxy);
    my_server.run_forever();
}

fn make_storage(
    crypto_provider: &CryptoProvider,
) -> Result<HashMap<String, Arc<CertifiedKey>>, Box<dyn core::error::Error>> {
    use std::fs;

    use pingora_rustls::{CertificateDer, PemObject as _, PrivateKeyDer};

    let cert_path = format!("{}/tests/keys/server.crt", env!("CARGO_MANIFEST_DIR"));
    let key_path = format!("{}/tests/keys/key.pem", env!("CARGO_MANIFEST_DIR"));

    let cert_str = fs::read_to_string(&cert_path)?;
    let key_str = fs::read_to_string(&key_path)?;

    let mut storage: HashMap<String, Arc<CertifiedKey>> = HashMap::new();

    storage.insert(
        "openrusty.org".into(),
        Arc::new({
            CertifiedKey::from_der(
                vec![CertificateDer::from_pem_slice(cert_str.as_bytes()).unwrap()],
                PrivateKeyDer::from_pem_slice(key_str.as_bytes()).unwrap(),
                &crypto_provider,
            )?
        }),
    );

    storage.insert(
        "*.openrusty.org".into(),
        Arc::new({
            CertifiedKey::from_der(
                vec![CertificateDer::from_pem_slice(cert_str.as_bytes()).unwrap()],
                PrivateKeyDer::from_pem_slice(key_str.as_bytes()).unwrap(),
                &crypto_provider,
            )?
        }),
    );

    Ok(storage)
}
