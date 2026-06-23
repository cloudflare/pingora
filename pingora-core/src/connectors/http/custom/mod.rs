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

use async_trait::async_trait;
use std::time::Duration;

use pingora_error::Result;

use crate::{
    protocols::{http::custom::client::Session, Stream},
    upstreams::peer::Peer,
};

// Either returns a Custom Session or the Stream for creating a new H1 session as a fallback.
pub enum Connection<S: Session> {
    Session(S),
    Stream(Stream),
}
#[doc(hidden)]
#[async_trait]
pub trait Connector: Send + Sync + Unpin + 'static {
    type Session: Session;

    async fn get_http_session<P: Peer + Send + Sync + 'static>(
        &self,
        peer: &P,
    ) -> Result<(Connection<Self::Session>, bool)>;

    async fn reused_http_session<P: Peer + Send + Sync + 'static>(
        &self,
        peer: &P,
    ) -> Option<Self::Session>;

    async fn release_http_session<P: Peer + Send + Sync + 'static>(
        &self,
        mut session: Self::Session,
        peer: &P,
        idle_timeout: Option<Duration>,
    );
}

#[doc(hidden)]
#[async_trait]
impl Connector for () {
    type Session = ();

    async fn get_http_session<P: Peer + Send + Sync + 'static>(
        &self,
        _peer: &P,
    ) -> Result<(Connection<Self::Session>, bool)> {
        unreachable!("connector: get_http_session")
    }

    async fn reused_http_session<P: Peer + Send + Sync + 'static>(
        &self,
        _peer: &P,
    ) -> Option<Self::Session> {
        unreachable!("connector: reused_http_session")
    }

    async fn release_http_session<P: Peer + Send + Sync + 'static>(
        &self,
        _session: Self::Session,
        _peer: &P,
        _idle_timeout: Option<Duration>,
    ) {
        unreachable!("connector: release_http_session")
    }
}
