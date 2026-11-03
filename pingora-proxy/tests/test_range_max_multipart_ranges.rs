// Regression coverage for https://github.com/cloudflare/pingora/issues/1063:
// a Range request whose range-spec count equals `max_multipart_ranges` must
// be served, and only requests with more range-specs are ignored. Exercises
// the public `range_header_filter` including the response framing it writes.

use http::header::{ACCEPT_RANGES, CONTENT_LENGTH, CONTENT_RANGE, CONTENT_TYPE, RANGE};
use pingora_http::{RequestHeader, ResponseHeader};
use pingora_proxy::{range_header_filter, RangeType};

fn run(range: &str, max_multipart_ranges: Option<usize>) -> (RangeType, ResponseHeader) {
    let mut req = RequestHeader::build("GET", b"/", None).unwrap();
    req.insert_header(RANGE, range).unwrap();
    let mut resp = ResponseHeader::build(200, None).unwrap();
    resp.insert_header(CONTENT_LENGTH, "1000").unwrap();
    let range_type = range_header_filter(&req, &mut resp, max_multipart_ranges);
    (range_type, resp)
}

fn ranges(count: usize) -> String {
    let parts: Vec<String> = (0..count)
        .map(|i| format!("{}-{}", i * 4, i * 4 + 1))
        .collect();
    format!("bytes={}", parts.join(","))
}

#[test]
fn single_range_within_max_one_is_served() {
    let (range_type, resp) = run("bytes=0-1", Some(1));
    assert_eq!(range_type, RangeType::Single(0..2));
    assert_eq!(resp.status, http::StatusCode::PARTIAL_CONTENT);
    assert_eq!(resp.headers.get(CONTENT_RANGE).unwrap(), "bytes 0-1/1000");
}

#[test]
fn range_count_at_max_is_served() {
    // one below, exactly at, and one above the configured limit
    for (count, served) in [(199, true), (200, true), (201, false)] {
        let (range_type, resp) = run(&ranges(count), Some(200));
        if served {
            let RangeType::Multi(multi) = range_type else {
                panic!("{count} ranges within the limit should be served, got {range_type:?}");
            };
            assert_eq!(multi.ranges.len(), count);
            assert_eq!(resp.status, http::StatusCode::PARTIAL_CONTENT);
            // multipart framing replaces the plain content headers
            let content_type = resp.headers.get(CONTENT_TYPE).unwrap().to_str().unwrap();
            assert!(
                content_type.starts_with("multipart/byteranges; boundary="),
                "unexpected content type {content_type}"
            );
        } else {
            assert_eq!(range_type, RangeType::None);
            // request declined: full response, advertising range support
            assert_eq!(resp.status, http::StatusCode::OK);
            assert_eq!(resp.headers.get(ACCEPT_RANGES).unwrap(), "bytes");
            assert_eq!(resp.headers.get(CONTENT_LENGTH).unwrap(), "1000");
        }
    }
}

#[test]
fn no_limit_serves_unbounded_count() {
    let (range_type, resp) = run(&ranges(250), None);
    let RangeType::Multi(multi) = range_type else {
        panic!("250 ranges without a configured limit should be served");
    };
    assert_eq!(multi.ranges.len(), 250);
    assert_eq!(resp.status, http::StatusCode::PARTIAL_CONTENT);
}
