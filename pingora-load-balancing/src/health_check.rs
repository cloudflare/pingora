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

//! Health Check interface and methods.

use crate::Backend;
use arc_swap::ArcSwap;
use async_trait::async_trait;
use pingora_core::connectors::{http::Connector as HttpConnector, TransportConnector};
use pingora_core::upstreams::peer::{BasicPeer, HttpPeer, Peer};
use pingora_error::{Error, ErrorType::CustomCode, Result};
use pingora_http::{RequestHeader, ResponseHeader};
use std::sync::Arc;
use std::time::Duration;

/// [HealthCheck] is the interface to implement health check for backends
#[async_trait]
pub trait HealthCheck {
    /// Check the given backend.
    ///
    /// `Ok(())`` if the check passes, otherwise the check fails.
    async fn check(&self, target: &Backend) -> Result<()>;
    /// This function defines how many *consecutive* checks should flip the health of a backend.
    ///
    /// For example: with `success``: `true`: this function should return the
    /// number of check need to flip from unhealthy to healthy.
    fn health_threshold(&self, success: bool) -> usize;
}

/// TCP health check
///
/// This health check checks if a TCP (or TLS) connection can be established to a given backend.
pub struct TcpHealthCheck {
    /// Number of successful checks to flip from unhealthy to healthy.
    pub consecutive_success: usize,
    /// Number of failed checks to flip from healthy to unhealthy.
    pub consecutive_failure: usize,
    /// How to connect to the backend.
    ///
    /// This field defines settings like the connect timeout and src IP to bind.
    /// The SocketAddr of `peer_template` is just a placeholder which will be replaced by the
    /// actual address of the backend when the health check runs.
    ///
    /// By default, this check will try to establish a TCP connection. When the `sni` field is
    /// set, it will also try to establish a TLS connection on top of the TCP connection.
    pub peer_template: BasicPeer,
    connector: TransportConnector,
}

impl Default for TcpHealthCheck {
    fn default() -> Self {
        let mut peer_template = BasicPeer::new("0.0.0.0:1");
        peer_template.options.connection_timeout = Some(Duration::from_secs(1));
        TcpHealthCheck {
            consecutive_success: 1,
            consecutive_failure: 1,
            peer_template,
            connector: TransportConnector::new(None),
        }
    }
}

impl TcpHealthCheck {
    /// Create a new [TcpHealthCheck] with the following default values
    /// * connect timeout: 1 second
    /// * consecutive_success: 1
    /// * consecutive_failure: 1
    pub fn new() -> Box<Self> {
        Box::<TcpHealthCheck>::default()
    }

    /// Create a new [TcpHealthCheck] that tries to establish a TLS connection.
    ///
    /// The default values are the same as [Self::new()].
    pub fn new_tls(sni: &str) -> Box<Self> {
        let mut new = Self::default();
        new.peer_template.sni = sni.into();
        Box::new(new)
    }

    /// Replace the internal tcp connector with the given [TransportConnector]
    pub fn set_connector(&mut self, connector: TransportConnector) {
        self.connector = connector;
    }
}

#[async_trait]
impl HealthCheck for TcpHealthCheck {
    fn health_threshold(&self, success: bool) -> usize {
        if success {
            self.consecutive_success
        } else {
            self.consecutive_failure
        }
    }

    async fn check(&self, target: &Backend) -> Result<()> {
        let mut peer = self.peer_template.clone();
        peer._address = target.addr.clone();
        self.connector.get_stream(&peer).await.map(|_| {})
    }
}

type Validator = Box<dyn Fn(&ResponseHeader) -> Result<()> + Send + Sync>;

/// HTTP health check
///
/// This health check checks if it can receive the expected HTTP(s) response from the given backend.
pub struct HttpHealthCheck {
    /// Number of successful checks to flip from unhealthy to healthy.
    pub consecutive_success: usize,
    /// Number of failed checks to flip from healthy to unhealthy.
    pub consecutive_failure: usize,
    /// How to connect to the backend.
    ///
    /// This field defines settings like the connect timeout and src IP to bind.
    /// The SocketAddr of `peer_template` is just a placeholder which will be replaced by the
    /// actual address of the backend when the health check runs.
    ///
    /// Set the `scheme` field to use HTTPs.
    pub peer_template: HttpPeer,
    /// Whether the underlying TCP/TLS connection can be reused across checks.
    ///
    /// * `false` will make sure that every health check goes through TCP (and TLS) handshakes.
    ///   Established connections sometimes hide the issue of firewalls and L4 LB.
    /// * `true` will try to reuse connections across checks, this is the more efficient and fast way
    ///   to perform health checks.
    pub reuse_connection: bool,
    /// The request header to send to the backend
    pub req: RequestHeader,
    connector: HttpConnector,
    /// Optional field to define how to validate the response from the server.
    ///
    /// If not set, any response with a `200 OK` is considered a successful check.
    pub validator: Option<Validator>,
    /// Sometimes the health check endpoint lives one a different port than the actual backend.
    /// Setting this option allows the health check to perform on the given port of the backend IP.
    pub port_override: Option<u16>,
}

impl HttpHealthCheck {
    /// Create a new [HttpHealthCheck] with the following default settings
    /// * connect timeout: 1 second
    /// * read timeout: 1 second
    /// * req: a GET to the `/` of the given host name
    /// * consecutive_success: 1
    /// * consecutive_failure: 1
    /// * reuse_connection: false
    /// * validator: `None`, any 200 response is considered successful
    pub fn new(host: &str, tls: bool) -> Self {
        let mut req = RequestHeader::build("GET", b"/", None).unwrap();
        req.append_header("Host", host).unwrap();
        let sni = if tls { host.into() } else { String::new() };
        let mut peer_template = HttpPeer::new("0.0.0.0:1", tls, sni);
        peer_template.options.connection_timeout = Some(Duration::from_secs(1));
        peer_template.options.read_timeout = Some(Duration::from_secs(1));
        HttpHealthCheck {
            consecutive_success: 1,
            consecutive_failure: 1,
            peer_template,
            connector: HttpConnector::new(None),
            reuse_connection: false,
            req,
            validator: None,
            port_override: None,
        }
    }

    /// Replace the internal http connector with the given [HttpConnector]
    pub fn set_connector(&mut self, connector: HttpConnector) {
        self.connector = connector;
    }
}

#[async_trait]
impl HealthCheck for HttpHealthCheck {
    fn health_threshold(&self, success: bool) -> usize {
        if success {
            self.consecutive_success
        } else {
            self.consecutive_failure
        }
    }

    async fn check(&self, target: &Backend) -> Result<()> {
        let mut peer = self.peer_template.clone();
        peer._address = target.addr.clone();
        if let Some(port) = self.port_override {
            peer._address.set_port(port);
        }
        let session = self.connector.get_http_session(&peer).await?;

        let mut session = session.0;
        let req = Box::new(self.req.clone());
        session.write_request_header(req).await?;

        if let Some(read_timeout) = peer.options.read_timeout {
            session.set_read_timeout(read_timeout);
        }

        session.read_response_header().await?;

        let resp = session.response_header().expect("just read");

        if let Some(validator) = self.validator.as_ref() {
            validator(resp)?;
        } else if resp.status != 200 {
            return Error::e_explain(
                CustomCode("non 200 code", resp.status.as_u16()),
                "during http healthcheck",
            );
        };

        while session.read_response_body().await?.is_some() {
            // drain the body if any
        }

        if self.reuse_connection {
            let idle_timeout = peer.idle_timeout();
            self.connector
                .release_http_session(session, &peer, idle_timeout)
                .await;
        }

        Ok(())
    }
}

#[derive(Clone)]
struct HealthInner {
    /// Whether the endpoint is healthy to serve traffic
    healthy: bool,
    /// Whether the endpoint is allowed to serve traffic independent of its health
    enabled: bool,
    /// The counter for stateful transition between healthy and unhealthy.
    /// When [healthy] is true, this counts the number of consecutive health check failures
    /// so that the caller can flip the healthy when a certain threshold is met, and vise versa.
    consecutive_counter: usize,
}

/// Health of backends that can be updated atomically
pub(crate) struct Health(ArcSwap<HealthInner>);

impl Default for Health {
    fn default() -> Self {
        Health(ArcSwap::new(Arc::new(HealthInner {
            healthy: true, // TODO: allow to start with unhealthy
            enabled: true,
            consecutive_counter: 0,
        })))
    }
}

impl Clone for Health {
    fn clone(&self) -> Self {
        let inner = self.0.load_full();
        Health(ArcSwap::new(inner))
    }
}

impl Health {
    pub fn ready(&self) -> bool {
        let h = self.0.load();
        h.healthy && h.enabled
    }

    pub fn enable(&self, enabled: bool) {
        let h = self.0.load();
        if h.enabled != enabled {
            // clone the inner
            let mut new_health = (**h).clone();
            new_health.enabled = enabled;
            self.0.store(Arc::new(new_health));
        };
    }

    // return true when the health is flipped
    pub fn observe_health(&self, health: bool, flip_threshold: usize) -> bool {
        let h = self.0.load();
        let mut flipped = false;
        if h.healthy != health {
            // opposite health observed, ready to increase the counter
            // clone the inner
            let mut new_health = (**h).clone();
            new_health.consecutive_counter += 1;
            if new_health.consecutive_counter >= flip_threshold {
                new_health.healthy = health;
                new_health.consecutive_counter = 0;
                flipped = true;
            }
            self.0.store(Arc::new(new_health));
        } else if h.consecutive_counter > 0 {
            // observing the same health as the current state.
            // reset the counter, if it is non-zero, because it is no longer consecutive
            let mut new_health = (**h).clone();
            new_health.consecutive_counter = 0;
            self.0.store(Arc::new(new_health));
        }
        flipped
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::SocketAddr;
    use http::Extensions;

    #[tokio::test]
    async fn test_tcp_check() {
        let tcp_check = TcpHealthCheck::default();

        let backend = Backend {
            addr: SocketAddr::Inet("1.1.1.1:80".parse().unwrap()),
            weight: 1,
            ext: Extensions::new(),
        };

        assert!(tcp_check.check(&backend).await.is_ok());

        let backend = Backend {
            addr: SocketAddr::Inet("1.1.1.1:79".parse().unwrap()),
            weight: 1,
            ext: Extensions::new(),
        };

        assert!(tcp_check.check(&backend).await.is_err());
    }

    #[tokio::test]
    async fn test_tls_check() {
        let tls_check = TcpHealthCheck::new_tls("one.one.one.one");
        let backend = Backend {
            addr: SocketAddr::Inet("1.1.1.1:443".parse().unwrap()),
            weight: 1,
            ext: Extensions::new(),
        };

        assert!(tls_check.check(&backend).await.is_ok());
    }

    #[tokio::test]
    async fn test_https_check() {
        let https_check = HttpHealthCheck::new("one.one.one.one", true);

        let backend = Backend {
            addr: SocketAddr::Inet("1.1.1.1:443".parse().unwrap()),
            weight: 1,
            ext: Extensions::new(),
        };

        assert!(https_check.check(&backend).await.is_ok());
    }

    #[tokio::test]
    async fn test_http_custom_check() {
        let mut http_check = HttpHealthCheck::new("one.one.one.one", false);
        http_check.validator = Some(Box::new(|resp: &ResponseHeader| {
            if resp.status == 301 {
                Ok(())
            } else {
                Error::e_explain(
                    CustomCode("non 301 code", resp.status.as_u16()),
                    "during http healthcheck",
                )
            }
        }));

        let backend = Backend {
            addr: SocketAddr::Inet("1.1.1.1:80".parse().unwrap()),
            weight: 1,
            ext: Extensions::new(),
        };

        http_check.check(&backend).await.unwrap();

        assert!(http_check.check(&backend).await.is_ok());
    }
}
