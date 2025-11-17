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

//! TLS information from the TLS connection

use std::any::{Any, TypeId};
use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::Arc;

/// The TLS connection information
#[derive(Clone)]
pub struct SslDigest {
    /// The cipher used
    pub cipher: Cow<'static, str>,
    /// The TLS version of this connection
    pub version: Cow<'static, str>,
    /// The organization of the peer's certificate
    pub organization: Option<String>,
    /// The serial number of the peer's certificate
    pub serial_number: Option<String>,
    /// The digest of the peer's certificate
    pub cert_digest: Vec<u8>,
    /// User-defined extensions
    extensions: HashMap<TypeId, Arc<dyn Any + Send + Sync>>,
}

impl std::fmt::Debug for SslDigest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SslDigest")
            .field("cipher", &self.cipher)
            .field("version", &self.version)
            .field("organization", &self.organization)
            .field("serial_number", &self.serial_number)
            .field("cert_digest", &self.cert_digest)
            .field("extensions_count", &self.extensions.len())
            .finish()
    }
}

impl SslDigest {
    /// Create a new SslDigest
    pub fn new<S>(
        cipher: S,
        version: S,
        organization: Option<String>,
        serial_number: Option<String>,
        cert_digest: Vec<u8>,
    ) -> Self
    where
        S: Into<Cow<'static, str>>,
    {
        SslDigest {
            cipher: cipher.into(),
            version: version.into(),
            organization,
            serial_number,
            cert_digest,
            extensions: HashMap::new(),
        }
    }

    /// Insert a user-defined value
    pub fn insert<T: Any + Send + Sync>(&mut self, value: T) {
        self.extensions.insert(TypeId::of::<T>(), Arc::new(value));
    }

    /// Get a user-defined value by type
    pub fn get<T: Any + Send + Sync>(&self) -> Option<&T> {
        self.extensions
            .get(&TypeId::of::<T>())
            .and_then(|v| v.downcast_ref::<T>())
    }
}
