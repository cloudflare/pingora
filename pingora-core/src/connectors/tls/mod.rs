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

#[cfg(feature = "openssl_derived")]
mod boringssl_openssl;

#[cfg(feature = "openssl_derived")]
pub use boringssl_openssl::*;

#[cfg(feature = "rustls")]
mod rustls;

#[cfg(feature = "rustls")]
pub use rustls::*;

///    OpenSSL considers underscores in hostnames non-compliant.
///    We replace the underscore in the leftmost label as we must support these
///    hostnames for wildcard matches and we have not patched OpenSSL.
///
///    https://github.com/openssl/openssl/issues/12566
///
///    > The labels must follow the rules for ARPANET host names. They must
///    > start with a letter, end with a letter or digit, and have as interior
///    > characters only letters, digits, and hyphen.  There are also some
///    > restrictions on the length.  Labels must be 63 characters or less.
///    - https://datatracker.ietf.org/doc/html/rfc1034#section-3.5
#[cfg(feature = "any_tls")]
pub fn replace_leftmost_underscore(sni: &str) -> Option<String> {
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

#[cfg(feature = "any_tls")]
#[cfg(test)]
mod tests {
    use super::*;

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
            assert!(replace_leftmost_underscore(case).is_none(), "{}", case);
        }

        assert_eq!(
            Some("bb-b.some.com".to_string()),
            replace_leftmost_underscore("bb_b.some.com")
        );
        assert_eq!(
            Some("a-a-a.some.com".to_string()),
            replace_leftmost_underscore("a_a_a.some.com")
        );
        assert_eq!(
            Some("-.some.com".to_string()),
            replace_leftmost_underscore("_.some.com")
        );
    }
}
