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

//! Cache key

use blake2::{Blake2b, Digest};
use http::Extensions;
use serde::{Deserialize, Serialize};
use std::fmt::{Display, Formatter, Result as FmtResult};

// 16-byte / 128-bit key: large enough to avoid collision
const KEY_SIZE: usize = 16;

/// An 128 bit hash binary
pub type HashBinary = [u8; KEY_SIZE];

fn hex2str(hex: &[u8]) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(KEY_SIZE * 2);
    for c in hex {
        write!(s, "{:02x}", c).unwrap(); // safe, just dump hex to string
    }
    s
}

/// Decode the hex str into [HashBinary].
///
/// Return `None` when the decode fails or the input is not exact 32 (to decode to 16 bytes).
pub fn str2hex(s: &str) -> Option<HashBinary> {
    if s.len() != KEY_SIZE * 2 {
        return None;
    }
    let mut output = [0; KEY_SIZE];
    // no need to bubble the error, it should be obvious why the decode fails
    hex::decode_to_slice(s.as_bytes(), &mut output).ok()?;
    Some(output)
}

/// The trait for cache key
pub trait CacheHashKey {
    /// Return the hash of the cache key
    fn primary_bin(&self) -> HashBinary;

    /// Return the variance hash of the cache key.
    ///
    /// `None` if no variance.
    fn variance_bin(&self) -> Option<HashBinary>;

    /// Return the hash including both primary and variance keys
    fn combined_bin(&self) -> HashBinary {
        let key = self.primary_bin();
        if let Some(v) = self.variance_bin() {
            let mut hasher = Blake2b128::new();
            hasher.update(key);
            hasher.update(v);
            hasher.finalize().into()
        } else {
            // if there is no variance, combined_bin should return the same as primary_bin
            key
        }
    }

    /// An extra tag for identifying users
    ///
    /// For example, if the storage backend implements per user quota, this tag can be used.
    fn user_tag(&self) -> &str;

    /// The hex string of [Self::primary_bin()]
    fn primary(&self) -> String {
        hex2str(&self.primary_bin())
    }

    /// The hex string of [Self::variance_bin()]
    fn variance(&self) -> Option<String> {
        self.variance_bin().as_ref().map(|b| hex2str(&b[..]))
    }

    /// The hex string of [Self::combined_bin()]
    fn combined(&self) -> String {
        hex2str(&self.combined_bin())
    }
}

/// General purpose cache key.
///
/// The primary hash is computed over the exact bytes supplied to [`CacheKey::new`].
/// Callers that combine multiple logical components, such as a namespace and URL,
/// must encode their boundaries unambiguously before constructing the key.
///
/// # Migration
///
/// The former `namespace` argument has been removed. Concatenating the old namespace
/// and primary bytes preserves the legacy hash, but also preserves its ambiguous
/// component boundaries. Switching to an unambiguous encoding changes hashes for
/// keys with a non-empty namespace, so callers should expect a cold cache.
#[derive(Debug, Clone)]
pub struct CacheKey {
    // Primary is essentially a string, except it allows invalid UTF-8 sequences.
    // This field should be able to be hashed.
    primary: Vec<u8>,
    primary_bin_override: Option<HashBinary>,
    variance: Option<HashBinary>,
    /// An extra tag for identifying users
    ///
    /// For example, if the storage backend implements per user quota, this tag can be used.
    pub user_tag: String,

    /// Grab-bag for user-defined extensions. These will not be persisted to disk.
    pub extensions: Extensions,
}

impl CacheKey {
    /// Set the value of the variance hash
    pub fn set_variance_key(&mut self, key: HashBinary) {
        self.variance = Some(key)
    }

    /// Get the value of the variance hash
    pub fn get_variance_key(&self) -> Option<&HashBinary> {
        self.variance.as_ref()
    }

    /// Removes the variance from this cache key
    pub fn remove_variance_key(&mut self) {
        self.variance = None
    }

    /// Override the primary key hash
    pub fn set_primary_bin_override(&mut self, key: HashBinary) {
        self.primary_bin_override = Some(key)
    }

    /// Try to get primary key as UTF-8 str, if valid
    pub fn primary_key_str(&self) -> Option<&str> {
        std::str::from_utf8(&self.primary).ok()
    }
}

/// Storage optimized cache key to keep in memory or in storage
// 16 bytes + 8 bytes (+16 * u8) + user_tag.len() + 16 Bytes (Box<str>)
#[derive(Debug, Default, Deserialize, Serialize, Clone, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub struct CompactCacheKey {
    pub primary: HashBinary,
    // save 8 bytes for non-variance but waste 8 bytes for variance vs, store flat 16 bytes
    pub variance: Option<Box<HashBinary>>,
    pub user_tag: Box<str>, // the len should be small to keep memory usage bounded
}

impl Display for CompactCacheKey {
    fn fmt(&self, f: &mut Formatter) -> FmtResult {
        write!(f, "{}", hex2str(&self.primary))?;
        if let Some(var) = &self.variance {
            write!(f, ", variance: {}", hex2str(var.as_ref()))?;
        }
        write!(f, ", user_tag: {}", self.user_tag)
    }
}

impl CacheHashKey for CompactCacheKey {
    fn primary_bin(&self) -> HashBinary {
        self.primary
    }

    fn variance_bin(&self) -> Option<HashBinary> {
        self.variance.as_ref().map(|s| *s.as_ref())
    }

    fn user_tag(&self) -> &str {
        &self.user_tag
    }
}

/*
 * We use blake2 hashing, which is faster and more secure, to replace md5.
 * We have not given too much thought on whether non-crypto hash can be safely
 * use because hashing performance is not critical.
 * Note: we should avoid hashes like ahash which does not have consistent output
 * across machines because it is designed purely for in memory hashtable
*/

// hash output: we use 128 bits (16 bytes) hash which will map to 32 bytes hex string
pub(crate) type Blake2b128 = Blake2b<blake2::digest::consts::U16>;

/// helper function: hash str to u8
pub fn hash_u8(key: &str) -> u8 {
    let mut hasher = Blake2b128::new();
    hasher.update(key);
    let raw = hasher.finalize();
    raw[0]
}

/// helper function: hash key (String or Bytes) to [HashBinary]
pub fn hash_key<K: AsRef<[u8]>>(key: K) -> HashBinary {
    let mut hasher = Blake2b128::new();
    hasher.update(key.as_ref());
    let raw = hasher.finalize();
    raw.into()
}

impl CacheKey {
    fn primary_hasher(&self) -> Blake2b128 {
        let mut hasher = Blake2b128::new();
        hasher.update(&self.primary);
        hasher
    }

    /// Create a new [CacheKey] from the given `primary` key and `user_tag`.
    ///
    /// Only the `primary` key will be hashed to produce the primary cache hash.
    /// If the primary contains multiple logical components, callers must frame
    /// them unambiguously, for example by length-prefixing each component.
    pub fn new<B, S>(primary: B, user_tag: S) -> Self
    where
        B: Into<Vec<u8>>,
        S: Into<String>,
    {
        CacheKey {
            primary: primary.into(),
            primary_bin_override: None,
            variance: None,
            user_tag: user_tag.into(),
            extensions: Extensions::new(),
        }
    }

    /// Return the primary key of this key
    pub fn primary_key(&self) -> &[u8] {
        &self.primary[..]
    }

    /// Convert this key to [CompactCacheKey].
    pub fn to_compact(&self) -> CompactCacheKey {
        let primary = self.primary_bin();
        CompactCacheKey {
            primary,
            variance: self.variance_bin().map(Box::new),
            user_tag: self.user_tag.clone().into_boxed_str(),
        }
    }
}

impl CacheHashKey for CacheKey {
    fn primary_bin(&self) -> HashBinary {
        if let Some(primary_bin_override) = self.primary_bin_override {
            primary_bin_override
        } else {
            self.primary_hasher().finalize().into()
        }
    }

    fn variance_bin(&self) -> Option<HashBinary> {
        self.variance
    }

    fn user_tag(&self) -> &str {
        &self.user_tag
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cache_key_hash() {
        let key = CacheKey::new("aa", "1");
        let hash = key.primary();
        assert_eq!(hash, "ac10f2aef117729f8dad056b3059eb7e");
        assert!(key.variance().is_none());
        assert_eq!(key.combined(), hash);
        let compact = key.to_compact();
        assert_eq!(compact.primary(), hash);
        assert!(compact.variance().is_none());
        assert_eq!(compact.combined(), hash);
    }

    #[test]
    fn test_caller_framed_primary_avoids_ambiguous_component_boundaries() {
        let left_components = [b"tenant_a".as_slice(), b"/path".as_slice()];
        let right_components = [b"tenant_".as_slice(), b"a/path".as_slice()];
        let legacy_primary = left_components.concat();
        assert_eq!(legacy_primary, right_components.concat());

        fn length_prefixed(components: &[&[u8]]) -> Vec<u8> {
            let mut primary = Vec::new();
            for component in components {
                primary.extend_from_slice(&(component.len() as u64).to_be_bytes());
                primary.extend_from_slice(component);
            }
            primary
        }

        let left = CacheKey::new(length_prefixed(&left_components), "1");
        let right = CacheKey::new(length_prefixed(&right_components), "1");
        assert_ne!(left.primary_bin(), right.primary_bin());
        assert_ne!(left.primary_bin(), hash_key(legacy_primary));
    }

    #[test]
    fn test_raw_concatenation_preserves_legacy_hash() {
        let mut primary = b"tenant_a".to_vec();
        primary.extend_from_slice(b"/path");

        let key = CacheKey::new(primary, "1");
        assert_eq!(key.primary(), "6c79e74e88bacb8eb370adb7617068c8");
    }

    #[test]
    fn test_cache_key_hash_override() {
        let mut key = CacheKey {
            primary: b"aa".to_vec(),
            primary_bin_override: str2hex("27c35e6e9373877f29e562464e46497e"),
            variance: None,
            user_tag: "1".into(),
            extensions: Extensions::new(),
        };
        let hash = key.primary();
        assert_eq!(hash, "27c35e6e9373877f29e562464e46497e");
        assert!(key.variance().is_none());
        assert_eq!(key.combined(), hash);
        let compact = key.to_compact();
        assert_eq!(compact.primary(), hash);
        assert!(compact.variance().is_none());
        assert_eq!(compact.combined(), hash);

        // make sure set_primary_bin_override overrides the primary key hash correctly
        key.set_primary_bin_override(str2hex("004174d3e75a811a5b44c46b3856f3ee").unwrap());
        let hash = key.primary();
        assert_eq!(hash, "004174d3e75a811a5b44c46b3856f3ee");
        assert!(key.variance().is_none());
        assert_eq!(key.combined(), hash);
        let compact = key.to_compact();
        assert_eq!(compact.primary(), hash);
        assert!(compact.variance().is_none());
        assert_eq!(compact.combined(), hash);
    }

    #[test]
    fn test_cache_key_vary_hash() {
        let key = CacheKey {
            primary: b"aa".to_vec(),
            primary_bin_override: None,
            variance: Some([0u8; 16]),
            user_tag: "1".into(),
            extensions: Extensions::new(),
        };
        let hash = key.primary();
        assert_eq!(hash, "ac10f2aef117729f8dad056b3059eb7e");
        assert_eq!(key.variance().unwrap(), "00000000000000000000000000000000");
        assert_eq!(key.combined(), "004174d3e75a811a5b44c46b3856f3ee");
        let compact = key.to_compact();
        assert_eq!(compact.primary(), "ac10f2aef117729f8dad056b3059eb7e");
        assert_eq!(
            compact.variance().unwrap(),
            "00000000000000000000000000000000"
        );
        assert_eq!(compact.combined(), "004174d3e75a811a5b44c46b3856f3ee");
    }

    #[test]
    fn test_cache_key_vary_hash_override() {
        let key = CacheKey {
            primary: b"saaaad".to_vec(),
            primary_bin_override: str2hex("ac10f2aef117729f8dad056b3059eb7e"),
            variance: Some([0u8; 16]),
            user_tag: "1".into(),
            extensions: Extensions::new(),
        };
        let hash = key.primary();
        assert_eq!(hash, "ac10f2aef117729f8dad056b3059eb7e");
        assert_eq!(key.variance().unwrap(), "00000000000000000000000000000000");
        assert_eq!(key.combined(), "004174d3e75a811a5b44c46b3856f3ee");
        let compact = key.to_compact();
        assert_eq!(compact.primary(), "ac10f2aef117729f8dad056b3059eb7e");
        assert_eq!(
            compact.variance().unwrap(),
            "00000000000000000000000000000000"
        );
        assert_eq!(compact.combined(), "004174d3e75a811a5b44c46b3856f3ee");
    }

    #[test]
    fn test_hex_str() {
        let mut key = [0; KEY_SIZE];
        for (i, v) in key.iter_mut().enumerate() {
            // key: [0, 1, 2, .., 15]
            *v = i as u8;
        }
        let hex_str = hex2str(&key);
        let key2 = str2hex(&hex_str).unwrap();
        for i in 0..KEY_SIZE {
            assert_eq!(key[i], key2[i]);
        }
    }
    #[test]
    fn test_primary_key_str_valid_utf8() {
        let valid_utf8_key = CacheKey {
            primary: b"/valid/path?query=1".to_vec(),
            primary_bin_override: None,
            variance: None,
            user_tag: "1".into(),
            extensions: Extensions::new(),
        };

        assert_eq!(
            valid_utf8_key.primary_key_str(),
            Some("/valid/path?query=1")
        )
    }

    #[test]
    fn test_primary_key_str_invalid_utf8() {
        let invalid_utf8_key = CacheKey {
            primary: vec![0x66, 0x6f, 0x6f, 0xff],
            primary_bin_override: None,
            variance: None,
            user_tag: "1".into(),
            extensions: Extensions::new(),
        };

        assert!(invalid_utf8_key.primary_key_str().is_none())
    }
}
