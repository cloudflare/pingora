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

use std::hash::{Hash, Hasher};
use std::num::NonZero;
use std::sync::{Arc, Mutex};

use ahash::AHasher;
use lru::LruCache;
use pingora_error::{Error, Result};
use pingora_error::{ErrorType::*, OrErr};

use pingora_s2n::{
    load_pem_file, ClientAuthType, Config, IgnoreVerifyHostnameCallback,
    TlsConnector as S2NTlsConnector, DEFAULT_TLS13,
};

use crate::utils::tls::{CertKey, X509Pem};
use crate::{
    connectors::ConnectorOptions,
    listeners::ALPN,
    protocols::{
        tls::{client::handshake, S2NConnectionBuilder, TlsStream},
        IO,
    },
    upstreams::peer::Peer,
};

const DEFAULT_CONFIG_CACHE_SIZE: NonZero<usize> = NonZero::new(10).unwrap();

#[derive(Clone)]
pub struct Connector {
    pub ctx: TlsConnector,
}

impl Connector {
    /// Create a new connector based on the optional configurations. If no
    /// configurations are provided, no customized certificates or keys will be
    /// used
    pub fn new(options: Option<ConnectorOptions>) -> Self {
        Connector {
            ctx: TlsConnector::new(options),
        }
    }
}

/// Holds default options for configuring a TLS connection and an LRU cache for `s2n_config`.
///
/// In `s2n-tls`, each connection requires an associated `s2n_config`, which is expensive to create.
/// Although `s2n_config` objects can be cheaply cloned, they are immutable once built.
///
/// To avoid the overhead of constructing a new config for every connection, we maintain a cache
/// that stores previously built configs. Configs are retrieved from the cache based on the
/// configuration options used to create them.
#[derive(Clone)]
pub struct TlsConnector {
    config_cache: Option<Arc<Mutex<LruCache<u64, Config>>>>,
    options: Option<ConnectorOptions>,
}

impl TlsConnector {
    pub fn new(options: Option<ConnectorOptions>) -> Self {
        TlsConnector {
            config_cache: Self::create_config_cache(&options),
            options,
        }
    }

    /// Provided with a set of config options, either creates a new s2n config or
    /// fetches one from the LRU Cache.
    fn load_config(&self, config_options: S2NConfigOptions) -> Result<Config> {
        if self.config_cache.is_some() {
            let config_hash = config_options.config_hash();
            if let Some(config) = self.load_config_from_cache(config_hash) {
                return Ok(config);
            } else {
                let config = create_s2n_config(&self.options, config_options)?;
                self.put_config_in_cache(config_hash, config.clone());
                return Ok(config);
            }
        } else {
            create_s2n_config(&self.options, config_options)
        }
    }

    fn load_config_from_cache(&self, config_hash: u64) -> Option<Config> {
        if let Some(config_cache) = &self.config_cache {
            let mut cache = config_cache.lock().unwrap();
            cache.get(&config_hash).cloned()
        } else {
            None
        }
    }

    fn put_config_in_cache(&self, config_hash: u64, config: Config) {
        if let Some(config_cache) = &self.config_cache {
            let mut cache = config_cache.lock().unwrap();
            cache.put(config_hash, config);
        }
    }

    fn create_config_cache(
        options: &Option<ConnectorOptions>,
    ) -> Option<Arc<Mutex<LruCache<u64, Config>>>> {
        let mut cache_size = DEFAULT_CONFIG_CACHE_SIZE;
        if let Some(opts) = options {
            if let Some(cache_size_config) = opts.s2n_config_cache_size {
                if cache_size_config <= 0 {
                    return None;
                } else {
                    cache_size = NonZero::new(cache_size_config).unwrap();
                }
            }
        }
        return Some(Arc::new(Mutex::new(LruCache::new(cache_size))));
    }
}

pub(crate) async fn connect<T, P>(
    stream: T,
    peer: &P,
    alpn_override: Option<ALPN>,
    tls_ctx: &TlsConnector,
) -> Result<TlsStream<T>>
where
    T: IO,
    P: Peer + Send + Sync,
{
    // Default security policy with TLS 1.3 support
    // https://aws.github.io/s2n-tls/usage-guide/ch06-security-policies.html
    let security_policy = peer.get_s2n_security_policy().unwrap_or(&DEFAULT_TLS13);

    let config_options = S2NConfigOptions::from_peer(peer, alpn_override);
    let config = tls_ctx.load_config(config_options)?;

    let connection_builder = S2NConnectionBuilder {
        config: config,
        psk_config: peer.get_psk().cloned(),
        security_policy: Some(security_policy.clone()),
    };

    let domain = peer
        .alternative_cn()
        .map(|s| s.as_str())
        .unwrap_or(peer.sni());
    let connector = S2NTlsConnector::new(connection_builder);

    let connect_future = handshake(&connector, domain, stream);

    match peer.connection_timeout() {
        Some(t) => match pingora_timeout::timeout(t, connect_future).await {
            Ok(res) => res,
            Err(_) => Error::e_explain(
                ConnectTimedout,
                format!("connecting to server {}, timeout {:?}", peer, t),
            ),
        },
        None => connect_future.await,
    }
}

fn create_s2n_config(
    connector_options: &Option<ConnectorOptions>,
    config_options: S2NConfigOptions,
) -> Result<Config> {
    let mut builder = Config::builder();

    if let Some(conf) = connector_options.as_ref() {
        if let Some(ca_file_path) = conf.ca_file.as_ref() {
            let ca_pem = load_pem_file(&ca_file_path)?;
            builder
                .trust_pem(&ca_pem)
                .or_err(InternalError, "failed to load ca cert")?;
        }

        if let Some((cert_file, key_file)) = conf.cert_key_file.as_ref() {
            let cert = load_pem_file(cert_file)?;
            let key = load_pem_file(key_file)?;
            builder
                .load_pem(&cert, &key)
                .or_err(InternalError, "failed to load client cert")?;
            builder
                .set_client_auth_type(ClientAuthType::Required)
                .or_err(InternalError, "failed to load client key")?;
        }
    }

    if let Some(max_blinding_delay) = config_options.max_blinding_delay {
        builder
            .set_max_blinding_delay(max_blinding_delay)
            .or_err(InternalError, "failed to set max blinding delay")?;
    }

    if let Some(ca) = config_options.ca {
        builder
            .trust_pem(&ca.raw_pem)
            .or_err(InternalError, "invalid peer ca cert")?;
    }

    if let Some(client_cert_key) = config_options.client_cert_key {
        builder
            .load_pem(&client_cert_key.raw_pem(), &client_cert_key.key())
            .or_err(InternalError, "invalid peer client cert or key")?;
    }

    if let Some(alpn) = config_options.alpn {
        builder
            .set_application_protocol_preference(alpn.to_wire_protocols())
            .or_err(InternalError, "failed to set peer alpn")?;
    }

    if !config_options.verify_cert {
        // Disabling x509 verification is considered unsafe
        unsafe {
            builder
                .disable_x509_verification()
                .or_err(InternalError, "failed to disable certificate verification")?;
        }
    }

    if !config_options.verify_hostname {
        // Set verify hostname callback that always returns success
        builder
            .set_verify_host_callback(IgnoreVerifyHostnameCallback::new())
            .or_err(InternalError, "failed to disable hostname verification")?;
    }

    if !config_options.use_system_certs {
        builder.with_system_certs(false).or_err(
            InternalError,
            "failed to disable system certificate loading",
        )?;
    }

    Ok(builder
        .build()
        .or_err(InternalError, "failed to build s2n config")?)
}

#[derive(Clone)]
struct S2NConfigOptions {
    max_blinding_delay: Option<u32>,
    alpn: Option<ALPN>,
    verify_cert: bool,
    verify_hostname: bool,
    use_system_certs: bool,
    ca: Option<Arc<X509Pem>>,
    client_cert_key: Option<Arc<CertKey>>,
}

impl S2NConfigOptions {
    fn from_peer<P>(peer: &P, alpn_override: Option<ALPN>) -> Self
    where
        P: Peer + Send + Sync,
    {
        S2NConfigOptions {
            max_blinding_delay: peer.get_max_blinding_delay(),
            alpn: alpn_override.or(peer.get_alpn().cloned()),
            verify_cert: peer.verify_cert(),
            verify_hostname: peer.verify_hostname(),
            use_system_certs: peer.use_system_certs(),
            ca: peer.get_ca().cloned(),
            client_cert_key: peer.get_client_cert_key().cloned(),
        }
    }

    fn config_hash(&self) -> u64 {
        let mut hasher = AHasher::default();
        self.hash(&mut hasher);
        hasher.finish()
    }
}

impl Hash for S2NConfigOptions {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.max_blinding_delay.hash(state);
        self.alpn.hash(state);
        self.verify_cert.hash(state);
        self.verify_hostname.hash(state);
        self.use_system_certs.hash(state);
        self.ca.hash(state);
        self.client_cert_key.hash(state);
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, sync::Arc};

    use crate::{
        connectors::tls::{s2n::S2NConfigOptions, TlsConnector},
        listeners::ALPN,
        utils::tls::{CertKey, X509Pem},
    };

    const CA_CERT_FILE: &str = "tests/certs/ca.crt";
    const ALT_CA_CERT_FILE: &str = "tests/certs/alt-ca.crt";

    const CERT_FILE: &str = "tests/certs/server.crt";
    const ALT_CERT_FILE: &str = "tests/certs/alt-server.crt";

    const KEY_FILE: &str = "tests/certs/server.key";

    fn read_file(file: &str) -> Vec<u8> {
        fs::read(file).unwrap()
    }

    fn load_pem_from_file(file: &str) -> X509Pem {
        X509Pem::new(read_file(file))
    }

    fn create_config_options() -> S2NConfigOptions {
        S2NConfigOptions {
            max_blinding_delay: Some(10),
            alpn: Some(ALPN::H1),
            verify_cert: true,
            verify_hostname: true,
            use_system_certs: true,
            ca: Some(Arc::new(load_pem_from_file(CA_CERT_FILE))),
            client_cert_key: Some(Arc::new(CertKey::new(
                read_file(CERT_FILE),
                read_file(KEY_FILE),
            ))),
        }
    }

    #[test]
    fn config_cache_hit_identical() {
        let connector = TlsConnector::new(None);
        let config_options = create_config_options();

        let config = connector.load_config(config_options.clone()).unwrap();
        let cached_config = connector.load_config_from_cache(config_options.config_hash());

        assert!(cached_config.is_some());
        assert_eq!(config, cached_config.unwrap());
    }

    #[test]
    fn config_cache_miss_max_blinding_delay_changed() {
        let connector = TlsConnector::new(None);
        let mut config_options = create_config_options();

        let _config = connector.load_config(config_options.clone()).unwrap();
        config_options.max_blinding_delay = Some(20);
        let cached_config = connector.load_config_from_cache(config_options.config_hash());

        assert!(cached_config.is_none());
    }

    #[test]
    fn config_cache_miss_alpn_changed() {
        let connector = TlsConnector::new(None);
        let mut config_options = create_config_options();

        let _config = connector.load_config(config_options.clone()).unwrap();
        config_options.alpn = Some(ALPN::H2H1);
        let cached_config = connector.load_config_from_cache(config_options.config_hash());

        assert!(cached_config.is_none());
    }

    #[test]
    fn config_cache_miss_verify_cert_changed() {
        let connector = TlsConnector::new(None);
        let mut config_options = create_config_options();

        let _config = connector.load_config(config_options.clone()).unwrap();
        config_options.verify_cert = false;
        let cached_config = connector.load_config_from_cache(config_options.config_hash());

        assert!(cached_config.is_none());
    }

    #[test]
    fn config_cache_miss_verify_hostname_changed() {
        let connector = TlsConnector::new(None);
        let mut config_options = create_config_options();

        let _config = connector.load_config(config_options.clone()).unwrap();
        config_options.verify_hostname = false;
        let cached_config = connector.load_config_from_cache(config_options.config_hash());

        assert!(cached_config.is_none());
    }

    #[test]
    fn config_cache_miss_use_system_certs_changed() {
        let connector = TlsConnector::new(None);
        let mut config_options = create_config_options();

        let _config = connector.load_config(config_options.clone()).unwrap();
        config_options.use_system_certs = false;
        let cached_config = connector.load_config_from_cache(config_options.config_hash());

        assert!(cached_config.is_none());
    }

    #[test]
    fn config_cache_miss_ca_changed() {
        let connector = TlsConnector::new(None);
        let mut config_options = create_config_options();

        let _config = connector.load_config(config_options.clone()).unwrap();
        config_options.ca = Some(Arc::new(load_pem_from_file(ALT_CA_CERT_FILE)));
        let cached_config = connector.load_config_from_cache(config_options.config_hash());

        assert!(cached_config.is_none());
    }

    #[test]
    fn config_cache_miss_client_cert_key_changed() {
        let connector = TlsConnector::new(None);
        let mut config_options = create_config_options();

        let _config = connector.load_config(config_options.clone()).unwrap();
        config_options.client_cert_key = Some(Arc::new(CertKey::new(
            read_file(ALT_CERT_FILE),
            read_file(KEY_FILE),
        )));
        let cached_config = connector.load_config_from_cache(config_options.config_hash());

        assert!(cached_config.is_none());
    }
}
