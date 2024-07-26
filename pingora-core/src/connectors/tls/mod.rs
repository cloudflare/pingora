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

use std::any::Any;
use std::net::SocketAddr;
use std::sync::Arc;

use pingora_error::ErrorType::ConnectTimedout;
use pingora_error::{Error, Result};

use crate::connectors::l4::connect as l4_connect;
#[cfg(not(feature = "rustls"))]
use crate::connectors::tls::boringssl_openssl::connect as tls_connect;
#[cfg(not(feature = "rustls"))]
use crate::connectors::tls::boringssl_openssl::TlsConnectorCtx;
#[cfg(feature = "rustls")]
use crate::connectors::tls::rustls::connect as tls_connect;
#[cfg(feature = "rustls")]
use crate::connectors::tls::rustls::TlsConnectorCtx;
use crate::protocols::Stream;
use crate::upstreams::peer::{Peer, ALPN};

use super::ConnectorOptions;

#[cfg(not(feature = "rustls"))]
pub(crate) mod boringssl_openssl;
#[cfg(feature = "rustls")]
pub(crate) mod rustls;

#[derive(Clone)]
pub struct Connector {
    pub(crate) ctx: Arc<dyn TlsConnectorContext + Send + Sync>, // Arc to support clone
}

impl Connector {
    pub fn new(options: Option<ConnectorOptions>) -> Self {
        TlsConnectorCtx::build_connector(options)
    }
}

pub(crate) trait TlsConnectorContext {
    fn as_any(&self) -> &dyn Any;

    fn build_connector(options: Option<ConnectorOptions>) -> Connector
    where
        Self: Sized;
}

pub(super) async fn do_connect<P: Peer + Send + Sync>(
    peer: &P,
    bind_to: Option<SocketAddr>,
    alpn_override: Option<ALPN>,
    tls_ctx: &Arc<dyn TlsConnectorContext + Send + Sync>,
) -> Result<Stream> {
    // Create the future that does the connections, but don't evaluate it until
    // we decide if we need a timeout or not
    let connect_future = do_connect_inner(peer, bind_to, alpn_override, tls_ctx);

    match peer.total_connection_timeout() {
        Some(t) => match pingora_timeout::timeout(t, connect_future).await {
            Ok(res) => res,
            Err(_) => Error::e_explain(
                ConnectTimedout,
                format!("connecting to server {peer}, total-connection timeout {t:?}"),
            ),
        },
        None => connect_future.await,
    }
}

async fn do_connect_inner<P: Peer + Send + Sync>(
    peer: &P,
    bind_to: Option<SocketAddr>,
    alpn_override: Option<ALPN>,
    tls_ctx: &Arc<dyn TlsConnectorContext + Send + Sync>,
) -> Result<Stream> {
    let stream = l4_connect(peer, bind_to).await?;
    if peer.tls() {
        let tls_stream = tls_connect(stream, peer, alpn_override, tls_ctx).await?;
        Ok(Box::new(tls_stream))
    } else {
        Ok(Box::new(stream))
    }
}

/*
    OpenSSL considers underscores in hostnames non-compliant.
    We replace the underscore in the leftmost label as we must support these
    hostnames for wildcard matches and we have not patched OpenSSL.

    https://github.com/openssl/openssl/issues/12566

    > The labels must follow the rules for ARPANET host names. They must
    > start with a letter, end with a letter or digit, and have as interior
    > characters only letters, digits, and hyphen.  There are also some
    > restrictions on the length.  Labels must be 63 characters or less.
    - https://datatracker.ietf.org/doc/html/rfc1034#section-3.5
*/
pub(crate) fn replace_leftmost_underscore(sni: &str) -> Option<String> {
    // wildcard is only leftmost label
    if let Some((leftmost, rest)) = sni.split_once('.') {
        // if not a subdomain or leftmost does not contain underscore return
        if !rest.contains('.') || !leftmost.contains('_') {
            return None;
        }
        // we have a subdomain, replace underscores
        let leftmost = leftmost.replace('_', "-");
        return Some(format!("{leftmost}.{rest}"));
    }
    None
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_replace_leftmost_underscore() {
        let none_cases = [
            "",
            "some",
            "some.com",
            "1.1.1.1:5050",
            "dog.dot.com",
            "dog.d_t.com",
            "dog.dot.c_m",
            "d_g.com",
            "_",
            "dog.c_m",
        ];

        for case in none_cases {
            assert!(
                super::replace_leftmost_underscore(case).is_none(),
                "{}",
                case
            );
        }

        assert_eq!(
            Some("bb-b.some.com".to_string()),
            super::replace_leftmost_underscore("bb_b.some.com")
        );
        assert_eq!(
            Some("a-a-a.some.com".to_string()),
            super::replace_leftmost_underscore("a_a_a.some.com")
        );
        assert_eq!(
            Some("-.some.com".to_string()),
            super::replace_leftmost_underscore("_.some.com")
        );
    }
}
