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

//! Extra information about the connection

use std::sync::Arc;
use std::time::SystemTime;

use super::raw_connect::ProxyDigest;
use super::ssl::digest::SslDigest;

/// The information can be extracted from a connection
#[derive(Clone, Debug)]
pub struct Digest {
    /// Information regarding the TLS of this connection if any
    pub ssl_digest: Option<Arc<SslDigest>>,
    /// Timing information
    pub timing_digest: Vec<Option<TimingDigest>>,
    /// information regarding the CONNECT proxy this connection uses.
    pub proxy_digest: Option<Arc<ProxyDigest>>,
}

/// The interface to return protocol related information
pub trait ProtoDigest {
    fn get_digest(&self) -> Option<&Digest> {
        None
    }
}

/// The timing information of the connection
#[derive(Clone, Debug)]
pub struct TimingDigest {
    /// When this connection was established
    pub established_ts: SystemTime,
}

impl Default for TimingDigest {
    fn default() -> Self {
        TimingDigest {
            established_ts: SystemTime::UNIX_EPOCH,
        }
    }
}

/// The interface to return timing information
pub trait GetTimingDigest {
    /// Return the timing for each layer from the lowest layer to upper
    fn get_timing_digest(&self) -> Vec<Option<TimingDigest>>;
}

/// The interface to set or return proxy information
pub trait GetProxyDigest {
    fn get_proxy_digest(&self) -> Option<Arc<ProxyDigest>>;
    fn set_proxy_digest(&mut self, _digest: ProxyDigest) {}
}
