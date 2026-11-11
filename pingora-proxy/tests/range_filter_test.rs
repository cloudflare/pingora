use pingora_http::{RequestHeader, ResponseHeader};
use pingora_proxy::{range_header_filter, RangeType};

fn run(range: &str, max: Option<usize>) -> RangeType {
    let mut req = RequestHeader::build("GET", b"/", None).unwrap();
    req.insert_header("Range", range).unwrap();
    let mut resp = ResponseHeader::build(200, None).unwrap();
    resp.insert_header("Content-Length", "1000").unwrap();
    range_header_filter(&req, &mut resp, max)
}

fn ranges(n: usize) -> String {
    let parts: Vec<String> = (0..n).map(|i| format!("{}-{}", i * 4, i * 4 + 1)).collect();
    format!("bytes={}", parts.join(","))
}

#[test]
fn max_multipart_ranges_boundary() {
    assert!(matches!(run("bytes=0-1", Some(1)), RangeType::Single(_)));
    assert!(matches!(run("bytes=0-1,4-5", Some(2)), RangeType::Multi(_)));
    assert!(matches!(run(&ranges(199), Some(200)), RangeType::Multi(_)));
    assert!(matches!(run(&ranges(200), Some(200)), RangeType::Multi(_)));
    assert_eq!(run(&ranges(201), Some(200)), RangeType::None);
}
