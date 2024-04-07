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

//! Utility functions to help process HTTP headers for caching

use super::*;
use crate::cache_control::{CacheControl, Cacheable, InterpretCacheControl};
use crate::RespCacheable::*;

use http::{header, HeaderValue};
use httpdate::HttpDate;
use log::warn;
use pingora_http::RequestHeader;

/// Decide if the request can be cacheable
pub fn request_cacheable(req_header: &ReqHeader) -> bool {
    // TODO: the check is incomplete
    matches!(req_header.method, Method::GET | Method::HEAD)
}

/// Decide if the response is cacheable.
///
/// `cache_control` is the parsed [CacheControl] from the response header. It is a standalone
/// argument so that caller has the flexibility to choose to use, change or ignore it.
pub fn resp_cacheable(
    cache_control: Option<&CacheControl>,
    resp_header: &ResponseHeader,
    authorization_present: bool,
    defaults: &CacheMetaDefaults,
) -> RespCacheable {
    let now = SystemTime::now();
    let expire_time = calculate_fresh_until(
        now,
        cache_control,
        resp_header,
        authorization_present,
        defaults,
    );
    if let Some(fresh_until) = expire_time {
        let (stale_while_revalidate_sec, stale_if_error_sec) =
            calculate_serve_stale_sec(cache_control, defaults);

        let mut cloned_header = resp_header.clone();
        if let Some(cc) = cache_control {
            cc.strip_private_headers(&mut cloned_header);
        }
        return Cacheable(CacheMeta::new(
            fresh_until,
            now,
            stale_while_revalidate_sec,
            stale_if_error_sec,
            cloned_header,
        ));
    }
    Uncacheable(NoCacheReason::OriginNotCache)
}

/// Calculate the [SystemTime] at which the asset expires
///
/// Return None when not cacheable.
pub fn calculate_fresh_until(
    now: SystemTime,
    cache_control: Option<&CacheControl>,
    resp_header: &RespHeader,
    authorization_present: bool,
    defaults: &CacheMetaDefaults,
) -> Option<SystemTime> {
    fn freshness_ttl_to_time(now: SystemTime, fresh_sec: u32) -> Option<SystemTime> {
        if fresh_sec == 0 {
            // ensure that the response is treated as stale
            now.checked_sub(Duration::from_secs(1))
        } else {
            now.checked_add(Duration::from_secs(fresh_sec.into()))
        }
    }

    // A request with Authorization is normally not cacheable, unless Cache-Control allows it
    if authorization_present {
        let uncacheable = cache_control
            .as_ref()
            .map_or(true, |cc| !cc.allow_caching_authorized_req());
        if uncacheable {
            return None;
        }
    }

    let uncacheable = cache_control
        .as_ref()
        .map_or(false, |cc| cc.is_cacheable() == Cacheable::No);
    if uncacheable {
        return None;
    }

    // For TTL check cache-control first, then expires header, then defaults
    cache_control
        .and_then(|cc| {
            cc.fresh_sec()
                .and_then(|ttl| freshness_ttl_to_time(now, ttl))
        })
        .or_else(|| calculate_expires_header_time(resp_header))
        .or_else(|| {
            defaults
                .fresh_sec(resp_header.status)
                .and_then(|ttl| freshness_ttl_to_time(now, ttl))
        })
}

/// Calculate the expire time from the `Expires` header only
pub fn calculate_expires_header_time(resp_header: &RespHeader) -> Option<SystemTime> {
    // according to RFC 7234:
    // https://datatracker.ietf.org/doc/html/rfc7234#section-4.2.1
    // - treat multiple expires headers as invalid
    // https://datatracker.ietf.org/doc/html/rfc7234#section-5.3
    // - "MUST interpret invalid date formats... as representing a time in the past"
    fn parse_expires_value(expires_value: &HeaderValue) -> Option<SystemTime> {
        let expires = expires_value.to_str().ok()?;
        Some(SystemTime::from(
            expires
                .parse::<HttpDate>()
                .map_err(|e| warn!("Invalid HttpDate in Expires: {}, error: {}", expires, e))
                .ok()?,
        ))
    }

    let mut expires_iter = resp_header.headers.get_all("expires").iter();
    let expires_header = expires_iter.next();
    if expires_header.is_none() || expires_iter.next().is_some() {
        return None;
    }
    parse_expires_value(expires_header.unwrap()).or(Some(SystemTime::UNIX_EPOCH))
}

/// Calculates stale-while-revalidate and stale-if-error seconds from Cache-Control or the [CacheMetaDefaults].
pub fn calculate_serve_stale_sec(
    cache_control: Option<&impl InterpretCacheControl>,
    defaults: &CacheMetaDefaults,
) -> (u32, u32) {
    let serve_stale_while_revalidate_sec = cache_control
        .and_then(|cc| cc.serve_stale_while_revalidate_sec())
        .unwrap_or_else(|| defaults.serve_stale_while_revalidate_sec());
    let serve_stale_if_error_sec = cache_control
        .and_then(|cc| cc.serve_stale_if_error_sec())
        .unwrap_or_else(|| defaults.serve_stale_if_error_sec());
    (serve_stale_while_revalidate_sec, serve_stale_if_error_sec)
}

/// Filters to run when sending requests to upstream
pub mod upstream {
    use super::*;

    /// Adjust the request header for cacheable requests
    ///
    /// This filter does the following in order to fetch the entire response to cache
    /// - Convert HEAD to GET
    /// - `If-*` headers are removed
    /// - `Range` header is removed
    ///
    /// When `meta` is set, this function will inject `If-modified-since` according to the `Last-Modified` header
    /// and inject `If-none-match` according to `Etag` header
    pub fn request_filter(req: &mut RequestHeader, meta: Option<&CacheMeta>) -> Result<()> {
        // change HEAD to GET, HEAD itself is not semantically cacheable
        if req.method == Method::HEAD {
            req.set_method(Method::GET);
        }

        // remove downstream precondition headers https://datatracker.ietf.org/doc/html/rfc7232#section-3
        // we'd like to cache the 200 not the 304
        req.remove_header(&header::IF_MATCH);
        req.remove_header(&header::IF_NONE_MATCH);
        req.remove_header(&header::IF_MODIFIED_SINCE);
        req.remove_header(&header::IF_UNMODIFIED_SINCE);
        // see below range header
        req.remove_header(&header::IF_RANGE);

        // remove downstream range header as we'd like to cache the entire response (this might change in the future)
        req.remove_header(&header::RANGE);

        // we have a presumably staled response already, add precondition headers for revalidation
        if let Some(m) = meta {
            // rfc7232: "SHOULD send both validators in cache validation" but
            // there have been weird cases that an origin has matching etag but not Last-Modified
            if let Some(since) = m.headers().get(&header::LAST_MODIFIED) {
                req.insert_header(header::IF_MODIFIED_SINCE, since).unwrap();
            }
            if let Some(etag) = m.headers().get(&header::ETAG) {
                req.insert_header(header::IF_NONE_MATCH, etag).unwrap();
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RespCacheable::Cacheable;
    use http::header::{HeaderName, CACHE_CONTROL, EXPIRES, SET_COOKIE};
    use http::StatusCode;
    use httpdate::fmt_http_date;

    fn init_log() {
        let _ = env_logger::builder().is_test(true).try_init();
    }

    const DEFAULTS: CacheMetaDefaults = CacheMetaDefaults::new(
        |status| match status {
            StatusCode::OK => Some(10),
            StatusCode::NOT_FOUND => Some(5),
            StatusCode::PARTIAL_CONTENT => None,
            _ => Some(1),
        },
        0,
        u32::MAX, /* "infinite" stale-if-error */
    );

    // Cache nothing, by default
    const BYPASS_CACHE_DEFAULTS: CacheMetaDefaults = CacheMetaDefaults::new(|_| None, 0, 0);

    fn build_response(status: u16, headers: &[(HeaderName, &str)]) -> ResponseHeader {
        let mut header = ResponseHeader::build(status, Some(headers.len())).unwrap();
        for (k, v) in headers {
            header.append_header(k.to_string(), *v).unwrap();
        }
        header
    }

    fn resp_cacheable_wrapper(
        resp: &ResponseHeader,
        defaults: &CacheMetaDefaults,
        authorization_present: bool,
    ) -> Option<CacheMeta> {
        if let Cacheable(meta) = resp_cacheable(
            CacheControl::from_resp_headers(resp).as_ref(),
            resp,
            authorization_present,
            defaults,
        ) {
            Some(meta)
        } else {
            None
        }
    }

    #[test]
    fn test_resp_cacheable() {
        let meta = resp_cacheable_wrapper(
            &build_response(200, &[(CACHE_CONTROL, "max-age=12345")]),
            &DEFAULTS,
            false,
        );

        let meta = meta.unwrap();
        assert!(meta.is_fresh(SystemTime::now()));
        assert!(meta.is_fresh(
            SystemTime::now()
                .checked_add(Duration::from_secs(12))
                .unwrap()
        ),);
        assert!(!meta.is_fresh(
            SystemTime::now()
                .checked_add(Duration::from_secs(12346))
                .unwrap()
        ));
    }

    #[test]
    fn test_resp_uncacheable_directives() {
        let meta = resp_cacheable_wrapper(
            &build_response(200, &[(CACHE_CONTROL, "private, max-age=12345")]),
            &DEFAULTS,
            false,
        );
        assert!(meta.is_none());

        let meta = resp_cacheable_wrapper(
            &build_response(200, &[(CACHE_CONTROL, "no-store, max-age=12345")]),
            &DEFAULTS,
            false,
        );
        assert!(meta.is_none());
    }

    #[test]
    fn test_resp_cache_authorization() {
        let meta = resp_cacheable_wrapper(&build_response(200, &[]), &DEFAULTS, true);
        assert!(meta.is_none());

        let meta = resp_cacheable_wrapper(
            &build_response(200, &[(CACHE_CONTROL, "max-age=10")]),
            &DEFAULTS,
            true,
        );
        assert!(meta.is_none());

        let meta = resp_cacheable_wrapper(
            &build_response(200, &[(CACHE_CONTROL, "s-maxage=10")]),
            &DEFAULTS,
            true,
        );
        assert!(meta.unwrap().is_fresh(SystemTime::now()));

        let meta = resp_cacheable_wrapper(
            &build_response(200, &[(CACHE_CONTROL, "public, max-age=10")]),
            &DEFAULTS,
            true,
        );
        assert!(meta.unwrap().is_fresh(SystemTime::now()));

        let meta = resp_cacheable_wrapper(
            &build_response(200, &[(CACHE_CONTROL, "must-revalidate")]),
            &DEFAULTS,
            true,
        );
        assert!(meta.unwrap().is_fresh(SystemTime::now()));
    }

    #[test]
    fn test_resp_zero_max_age() {
        let meta = resp_cacheable_wrapper(
            &build_response(200, &[(CACHE_CONTROL, "max-age=0, public")]),
            &DEFAULTS,
            false,
        );

        // cacheable, but needs revalidation
        assert!(!meta.unwrap().is_fresh(SystemTime::now()));
    }

    #[test]
    fn test_resp_expires() {
        let five_sec_time = SystemTime::now()
            .checked_add(Duration::from_secs(5))
            .unwrap();

        // future expires is cacheable
        let meta = resp_cacheable_wrapper(
            &build_response(200, &[(EXPIRES, &fmt_http_date(five_sec_time))]),
            &DEFAULTS,
            false,
        );

        let meta = meta.unwrap();
        assert!(meta.is_fresh(SystemTime::now()));
        assert!(!meta.is_fresh(
            SystemTime::now()
                .checked_add(Duration::from_secs(6))
                .unwrap()
        ));

        // even on default uncacheable statuses
        let meta = resp_cacheable_wrapper(
            &build_response(206, &[(EXPIRES, &fmt_http_date(five_sec_time))]),
            &DEFAULTS,
            false,
        );
        assert!(meta.is_some());
    }

    #[test]
    fn test_resp_past_expires() {
        // cacheable, but expired
        let meta = resp_cacheable_wrapper(
            &build_response(200, &[(EXPIRES, "Fri, 15 May 2015 15:34:21 GMT")]),
            &BYPASS_CACHE_DEFAULTS,
            false,
        );
        assert!(!meta.unwrap().is_fresh(SystemTime::now()));
    }

    #[test]
    fn test_resp_nonstandard_expires() {
        // init log to allow inspecting warnings
        init_log();

        // invalid cases, according to parser
        // (but should be stale according to RFC)
        let meta = resp_cacheable_wrapper(
            &build_response(200, &[(EXPIRES, "Mon, 13 Feb 0002 12:00:00 GMT")]),
            &BYPASS_CACHE_DEFAULTS,
            false,
        );
        assert!(!meta.unwrap().is_fresh(SystemTime::now()));

        let meta = resp_cacheable_wrapper(
            &build_response(200, &[(EXPIRES, "Fri, 01 Dec 99999 16:00:00 GMT")]),
            &BYPASS_CACHE_DEFAULTS,
            false,
        );
        assert!(!meta.unwrap().is_fresh(SystemTime::now()));

        let meta = resp_cacheable_wrapper(
            &build_response(200, &[(EXPIRES, "0")]),
            &BYPASS_CACHE_DEFAULTS,
            false,
        );
        assert!(!meta.unwrap().is_fresh(SystemTime::now()));
    }

    #[test]
    fn test_resp_multiple_expires() {
        let five_sec_time = SystemTime::now()
            .checked_add(Duration::from_secs(5))
            .unwrap();
        let ten_sec_time = SystemTime::now()
            .checked_add(Duration::from_secs(10))
            .unwrap();

        // multiple expires = uncacheable
        let meta = resp_cacheable_wrapper(
            &build_response(
                200,
                &[
                    (EXPIRES, &fmt_http_date(five_sec_time)),
                    (EXPIRES, &fmt_http_date(ten_sec_time)),
                ],
            ),
            &BYPASS_CACHE_DEFAULTS,
            false,
        );
        assert!(meta.is_none());

        // unless the default is cacheable
        let meta = resp_cacheable_wrapper(
            &build_response(
                200,
                &[
                    (EXPIRES, &fmt_http_date(five_sec_time)),
                    (EXPIRES, &fmt_http_date(ten_sec_time)),
                ],
            ),
            &DEFAULTS,
            false,
        );
        assert!(meta.is_some());
    }

    #[test]
    fn test_resp_cache_control_with_expires() {
        let five_sec_time = SystemTime::now()
            .checked_add(Duration::from_secs(5))
            .unwrap();
        // cache-control takes precedence over expires
        let meta = resp_cacheable_wrapper(
            &build_response(
                200,
                &[
                    (EXPIRES, &fmt_http_date(five_sec_time)),
                    (CACHE_CONTROL, "max-age=0"),
                ],
            ),
            &DEFAULTS,
            false,
        );
        assert!(!meta.unwrap().is_fresh(SystemTime::now()));
    }

    #[test]
    fn test_resp_stale_while_revalidate() {
        // respect defaults
        let meta = resp_cacheable_wrapper(
            &build_response(200, &[(CACHE_CONTROL, "max-age=10")]),
            &DEFAULTS,
            false,
        );

        let meta = meta.unwrap();
        let eleven_sec_time = SystemTime::now()
            .checked_add(Duration::from_secs(11))
            .unwrap();
        assert!(!meta.is_fresh(eleven_sec_time));
        assert!(!meta.serve_stale_while_revalidate(SystemTime::now()));
        assert!(!meta.serve_stale_while_revalidate(eleven_sec_time));

        // override with stale-while-revalidate
        let meta = resp_cacheable_wrapper(
            &build_response(
                200,
                &[(CACHE_CONTROL, "max-age=10, stale-while-revalidate=5")],
            ),
            &DEFAULTS,
            false,
        );

        let meta = meta.unwrap();
        let eleven_sec_time = SystemTime::now()
            .checked_add(Duration::from_secs(11))
            .unwrap();
        let sixteen_sec_time = SystemTime::now()
            .checked_add(Duration::from_secs(16))
            .unwrap();
        assert!(!meta.is_fresh(eleven_sec_time));
        assert!(meta.serve_stale_while_revalidate(eleven_sec_time));
        assert!(!meta.serve_stale_while_revalidate(sixteen_sec_time));
    }

    #[test]
    fn test_resp_stale_if_error() {
        // respect defaults
        let meta = resp_cacheable_wrapper(
            &build_response(200, &[(CACHE_CONTROL, "max-age=10")]),
            &DEFAULTS,
            false,
        );

        let meta = meta.unwrap();
        let hundred_years_time = SystemTime::now()
            .checked_add(Duration::from_secs(86400 * 365 * 100))
            .unwrap();
        assert!(!meta.is_fresh(hundred_years_time));
        assert!(meta.serve_stale_if_error(hundred_years_time));

        // override with stale-if-error
        let meta = resp_cacheable_wrapper(
            &build_response(
                200,
                &[(
                    CACHE_CONTROL,
                    "max-age=10, stale-while-revalidate=5, stale-if-error=60",
                )],
            ),
            &DEFAULTS,
            false,
        );

        let meta = meta.unwrap();
        let eleven_sec_time = SystemTime::now()
            .checked_add(Duration::from_secs(11))
            .unwrap();
        let seventy_sec_time = SystemTime::now()
            .checked_add(Duration::from_secs(70))
            .unwrap();
        assert!(!meta.is_fresh(eleven_sec_time));
        assert!(meta.serve_stale_if_error(SystemTime::now()));
        assert!(meta.serve_stale_if_error(eleven_sec_time));
        assert!(!meta.serve_stale_if_error(seventy_sec_time));

        // never serve stale
        let meta = resp_cacheable_wrapper(
            &build_response(200, &[(CACHE_CONTROL, "max-age=10, stale-if-error=0")]),
            &DEFAULTS,
            false,
        );

        let meta = meta.unwrap();
        let eleven_sec_time = SystemTime::now()
            .checked_add(Duration::from_secs(11))
            .unwrap();
        assert!(!meta.is_fresh(eleven_sec_time));
        assert!(!meta.serve_stale_if_error(eleven_sec_time));
    }

    #[test]
    fn test_resp_status_cache_defaults() {
        // 200 response
        let meta = resp_cacheable_wrapper(&build_response(200, &[]), &DEFAULTS, false);
        assert!(meta.is_some());

        let meta = meta.unwrap();
        assert!(meta.is_fresh(
            SystemTime::now()
                .checked_add(Duration::from_secs(9))
                .unwrap()
        ));
        assert!(!meta.is_fresh(
            SystemTime::now()
                .checked_add(Duration::from_secs(11))
                .unwrap()
        ));

        // 404 response, different ttl
        let meta = resp_cacheable_wrapper(&build_response(404, &[]), &DEFAULTS, false);
        assert!(meta.is_some());

        let meta = meta.unwrap();
        assert!(meta.is_fresh(
            SystemTime::now()
                .checked_add(Duration::from_secs(4))
                .unwrap()
        ));
        assert!(!meta.is_fresh(
            SystemTime::now()
                .checked_add(Duration::from_secs(6))
                .unwrap()
        ));

        // 206 marked uncacheable (no cache TTL)
        let meta = resp_cacheable_wrapper(&build_response(206, &[]), &DEFAULTS, false);
        assert!(meta.is_none());

        // default uncacheable status with explicit Cache-Control is cacheable
        let meta = resp_cacheable_wrapper(
            &build_response(206, &[(CACHE_CONTROL, "public, max-age=10")]),
            &DEFAULTS,
            false,
        );
        assert!(meta.is_some());

        let meta = meta.unwrap();
        assert!(meta.is_fresh(
            SystemTime::now()
                .checked_add(Duration::from_secs(9))
                .unwrap()
        ));
        assert!(!meta.is_fresh(
            SystemTime::now()
                .checked_add(Duration::from_secs(11))
                .unwrap()
        ));

        // 416 matches any status
        let meta = resp_cacheable_wrapper(&build_response(416, &[]), &DEFAULTS, false);
        assert!(meta.is_some());

        let meta = meta.unwrap();
        assert!(meta.is_fresh(SystemTime::now()));
        assert!(!meta.is_fresh(
            SystemTime::now()
                .checked_add(Duration::from_secs(2))
                .unwrap()
        ));
    }

    #[test]
    fn test_resp_cache_no_cache_fields() {
        // check #field-names are stripped from the cache header
        let meta = resp_cacheable_wrapper(
            &build_response(
                200,
                &[
                    (SET_COOKIE, "my-cookie"),
                    (CACHE_CONTROL, "private=\"something\", max-age=10"),
                    (HeaderName::from_bytes(b"Something").unwrap(), "foo"),
                ],
            ),
            &DEFAULTS,
            false,
        );
        let meta = meta.unwrap();
        assert!(meta.headers().contains_key(SET_COOKIE));
        assert!(!meta.headers().contains_key("Something"));

        let meta = resp_cacheable_wrapper(
            &build_response(
                200,
                &[
                    (SET_COOKIE, "my-cookie"),
                    (
                        CACHE_CONTROL,
                        "max-age=0, no-cache=\"meta1, SeT-Cookie ,meta2\"",
                    ),
                    (HeaderName::from_bytes(b"meta1").unwrap(), "foo"),
                ],
            ),
            &DEFAULTS,
            false,
        );
        let meta = meta.unwrap();
        assert!(!meta.headers().contains_key(SET_COOKIE));
        assert!(!meta.headers().contains_key("meta1"));
    }
}
