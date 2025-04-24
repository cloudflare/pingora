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

mod utils;

use utils::server_utils::init;
use utils::websocket::WS_ECHO;

use futures::{SinkExt, StreamExt};
use reqwest::header::{HeaderName, HeaderValue};
use reqwest::StatusCode;
use std::time::Duration;
use tokio_tungstenite::tungstenite::{client::IntoClientRequest, Message};

#[tokio::test]
async fn test_ip_binding() {
    init();
    let res = reqwest::get("http://127.0.0.1:6147/client_ip")
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let headers = res.headers();
    assert_eq!(headers["x-client-ip"], "127.0.0.2");
}

#[tokio::test]
async fn test_duplex() {
    init();
    // NOTE: this doesn't really verify that we are in full duplex mode as reqwest
    // won't allow us control when req body is sent
    let client = reqwest::Client::new();
    let res = client
        .post("http://127.0.0.1:6147/duplex/")
        .body("b".repeat(1024 * 1024)) // 1 MB upload
        .timeout(Duration::from_secs(5))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body = res.text().await.unwrap();
    assert_eq!(body.len(), 64 * 5);
}

#[tokio::test]
async fn test_connection_die() {
    init();
    let res = reqwest::get("http://127.0.0.1:6147/connection_die")
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body = res.text().await;
    // reqwest doesn't allow us to inspect the partial body
    assert!(body.is_err());
}

#[tokio::test]
async fn test_upload_connection_die() {
    init();
    let client = reqwest::Client::new();
    let res = client
        .post("http://127.0.0.1:6147/upload_connection_die/")
        .body("b".repeat(15 * 1024 * 1024)) // 15 MB upload
        .timeout(Duration::from_secs(5))
        .send()
        .await
        .unwrap();
    // should get 200 status before connection dies
    assert_eq!(res.status(), StatusCode::OK);
    let _ = res.text().await;

    // try h2
    let client = reqwest::Client::new();
    let res = client
        .post("http://127.0.0.1:6147/upload_connection_die/")
        .body("b".repeat(15 * 1024 * 1024)) // 15 MB upload
        .timeout(Duration::from_secs(5))
        .header("x-h2", "true")
        .send()
        .await
        .unwrap();
    // should get 200 status before connection dies
    assert_eq!(res.status(), StatusCode::OK);
    let _ = res.text().await;
}

#[tokio::test]
async fn test_upload() {
    init();
    let client = reqwest::Client::new();
    let res = client
        .post("http://127.0.0.1:6147/upload/")
        .body("b".repeat(15 * 1024 * 1024)) // 15 MB upload
        .timeout(Duration::from_secs(5))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body = res.text().await.unwrap();
    assert_eq!(body.len(), 64 * 5);
}

#[tokio::test]
async fn test_ws_server_ends_conn() {
    init();
    let _ = *WS_ECHO;

    // server gracefully closes connection

    let mut req = "ws://127.0.0.1:6147".into_client_request().unwrap();
    req.headers_mut()
        .insert("x-port", HeaderValue::from_static("9283"));

    let (mut ws_stream, _) = tokio_tungstenite::connect_async(req).await.unwrap();
    // gracefully close connection
    ws_stream.send("test".into()).await.unwrap();
    ws_stream.next().await.unwrap().unwrap();
    ws_stream.send("graceful".into()).await.unwrap();
    let msg = ws_stream.next().await.unwrap().unwrap();
    // assert graceful close
    assert!(matches!(msg, Message::Close(None)));
    // test may hang here if downstream doesn't close when upstream does
    assert!(ws_stream.next().await.is_none());

    // server abruptly closes connection

    let mut req = "ws://127.0.0.1:6147".into_client_request().unwrap();
    req.headers_mut()
        .insert("x-port", HeaderValue::from_static("9283"));

    let (mut ws_stream, _) = tokio_tungstenite::connect_async(req).await.unwrap();
    // abrupt close connection
    ws_stream.send("close".into()).await.unwrap();
    // test will hang here if downstream doesn't close when upstream does
    assert!(ws_stream.next().await.unwrap().is_err());

    // client gracefully closes connection

    let mut req = "ws://127.0.0.1:6147".into_client_request().unwrap();
    req.headers_mut()
        .insert("x-port", HeaderValue::from_static("9283"));

    let (mut ws_stream, _) = tokio_tungstenite::connect_async(req).await.unwrap();
    ws_stream.send("test".into()).await.unwrap();
    // sender initiates close
    ws_stream.close(None).await.unwrap();
    let msg = ws_stream.next().await.unwrap().unwrap();
    // assert echo
    assert_eq!("test", msg.into_text().unwrap());
    let msg = ws_stream.next().await.unwrap().unwrap();
    // assert graceful close
    assert!(matches!(msg, Message::Close(None)));
    assert!(ws_stream.next().await.is_none());
}

#[tokio::test]
async fn test_download_timeout() {
    init();
    use hyper::body::HttpBody;
    use tokio::time::sleep;

    let client = hyper::Client::new();
    let uri: hyper::Uri = "http://127.0.0.1:6147/download/".parse().unwrap();
    let req = hyper::Request::builder()
        .uri(uri)
        .header("x-write-timeout", "1")
        .body(hyper::Body::empty())
        .unwrap();
    let mut res = client.request(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);

    let mut err = false;
    sleep(Duration::from_secs(2)).await;
    while let Some(chunk) = res.body_mut().data().await {
        if chunk.is_err() {
            err = true;
        }
    }
    assert!(err);
}

#[tokio::test]
async fn test_download_timeout_min_rate() {
    init();
    use hyper::body::HttpBody;
    use tokio::time::sleep;

    let client = hyper::Client::new();
    let uri: hyper::Uri = "http://127.0.0.1:6147/download/".parse().unwrap();
    let req = hyper::Request::builder()
        .uri(uri)
        .header("x-write-timeout", "1")
        .header("x-min-rate", "10000")
        .body(hyper::Body::empty())
        .unwrap();
    let mut res = client.request(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);

    let mut err = false;
    sleep(Duration::from_secs(2)).await;
    while let Some(chunk) = res.body_mut().data().await {
        if chunk.is_err() {
            err = true;
        }
    }
    // no error as write timeout is overridden by min rate
    assert!(!err);
}

mod test_cache {
    use super::*;
    use std::str::FromStr;
    use tokio::time::sleep;

    #[tokio::test]
    async fn test_basic_caching() {
        init();
        let url = "http://127.0.0.1:6148/unique/test_basic_caching/now";

        let res = reqwest::get(url).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let headers = res.headers();
        let cache_miss_epoch = headers["x-epoch"].to_str().unwrap().parse::<f64>().unwrap();
        assert_eq!(headers["x-cache-status"], "miss");
        assert_eq!(res.text().await.unwrap(), "hello world");

        let res = reqwest::get(url).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let headers = res.headers();
        let cache_hit_epoch = headers["x-epoch"].to_str().unwrap().parse::<f64>().unwrap();
        assert_eq!(headers["x-cache-status"], "hit");
        assert_eq!(res.text().await.unwrap(), "hello world");

        assert_eq!(cache_miss_epoch, cache_hit_epoch);

        sleep(Duration::from_millis(1100)).await; // ttl is 1

        let res = reqwest::get(url).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let headers = res.headers();
        let cache_expired_epoch = headers["x-epoch"].to_str().unwrap().parse::<f64>().unwrap();
        assert_eq!(headers["x-cache-status"], "expired");
        assert_eq!(res.text().await.unwrap(), "hello world");

        assert!(cache_expired_epoch > cache_hit_epoch);
    }

    #[tokio::test]
    async fn test_purge() {
        init();
        let res = reqwest::get("http://127.0.0.1:6148/unique/test_purge/test2")
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let headers = res.headers();
        assert_eq!(headers["x-cache-status"], "miss");
        assert_eq!(res.text().await.unwrap(), "hello world");

        let res = reqwest::get("http://127.0.0.1:6148/unique/test_purge/test2")
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let headers = res.headers();
        assert_eq!(headers["x-cache-status"], "hit");
        assert_eq!(res.text().await.unwrap(), "hello world");

        let res = reqwest::Client::builder()
            .build()
            .unwrap()
            .request(
                reqwest::Method::from_bytes(b"PURGE").unwrap(),
                "http://127.0.0.1:6148/unique/test_purge/test2",
            )
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(res.text().await.unwrap(), "");

        let res = reqwest::Client::builder()
            .build()
            .unwrap()
            .request(
                reqwest::Method::from_bytes(b"PURGE").unwrap(),
                "http://127.0.0.1:6148/unique/test_purge/test2",
            )
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
        assert_eq!(res.text().await.unwrap(), "");

        let res = reqwest::get("http://127.0.0.1:6148/unique/test_purge/test2")
            .await
            .unwrap();
        let headers = res.headers();
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(headers["x-cache-status"], "miss");
        assert_eq!(res.text().await.unwrap(), "hello world");
    }

    #[tokio::test]
    async fn test_cache_miss_convert() {
        init();

        // test if-* header is stripped
        let client = reqwest::Client::new();
        let res = client
            .get("http://127.0.0.1:6148/unique/test_cache_miss_convert/no_if_headers")
            .header("if-modified-since", "Wed, 19 Jan 2022 18:39:12 GMT")
            .send()
            .await
            .unwrap();
        // 200 because last-modified not returned from upstream
        assert_eq!(res.status(), StatusCode::OK);
        let headers = res.headers();
        assert_eq!(headers["x-cache-status"], "miss");
        assert_eq!(res.text().await.unwrap(), "no if headers detected\n");

        // test range header is stripped
        let client = reqwest::Client::new();
        let res = client
            .get("http://127.0.0.1:6148/unique/test_cache_miss_convert2/no_if_headers")
            .header("Range", "bytes=0-1")
            .send()
            .await
            .unwrap();
        // we have not implemented downstream range yet, it should be 206 once we have it
        assert_eq!(res.status(), StatusCode::OK);
        let headers = res.headers();
        assert_eq!(headers["x-cache-status"], "miss");
        assert_eq!(res.text().await.unwrap(), "no if headers detected\n");
    }

    #[tokio::test]
    async fn test_network_error_mid_response() {
        init();
        let url = "http://127.0.0.1:6148/sleep/test_network_error_mid_response.txt";

        let res = reqwest::Client::new()
            .get(url)
            .header("x-set-sleep", "0") // no need to sleep
            .header("x-set-body-sleep", "0.1") // pause the body a bit before abort
            .header("x-abort-body", "true") // this will tell origin to kill the conn right away
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
        // sleep just a little to make sure the req above gets the cache lock
        sleep(Duration::from_millis(50)).await;
        let headers = res.headers();
        assert_eq!(headers["x-cache-status"], "miss");
        // the connection dies
        assert!(res.text().await.is_err());

        let res = reqwest::Client::new()
            .get(url)
            .header("x-set-sleep", "0") // no need to sleep
            .header("x-set-body-sleep", "0.1") // pause the body a bit before abort
            .header("x-abort-body", "true") // this will tell origin to kill the conn right away
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
        // sleep just a little to make sure the req above gets the cache lock
        sleep(Duration::from_millis(50)).await;
        let headers = res.headers();
        assert_eq!(headers["x-cache-status"], "miss");
        // the connection dies
        assert!(res.text().await.is_err());
    }

    #[tokio::test]
    async fn test_cache_upstream_revalidation() {
        init();
        let url = "http://127.0.0.1:6148/unique/test_upstream_revalidation/revalidate_now";

        let res = reqwest::get(url).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let headers = res.headers();
        let cache_miss_epoch = headers["x-epoch"].to_str().unwrap().parse::<f64>().unwrap();
        assert_eq!(headers["x-cache-status"], "miss");
        assert_eq!(headers["x-upstream-status"], "200");
        assert_eq!(res.text().await.unwrap(), "hello world");

        let res = reqwest::get(url).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let headers = res.headers();
        let cache_hit_epoch = headers["x-epoch"].to_str().unwrap().parse::<f64>().unwrap();
        assert_eq!(headers["x-cache-status"], "hit");
        assert!(headers.get("x-upstream-status").is_none());
        assert_eq!(res.text().await.unwrap(), "hello world");

        assert_eq!(cache_miss_epoch, cache_hit_epoch);

        sleep(Duration::from_millis(1100)).await; // ttl is 1

        let res = reqwest::get(url).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let headers = res.headers();
        let cache_expired_epoch = headers["x-epoch"].to_str().unwrap().parse::<f64>().unwrap();
        assert_eq!(headers["x-cache-status"], "revalidated");
        assert_eq!(headers["x-upstream-status"], "304");
        assert_eq!(res.text().await.unwrap(), "hello world");

        // still the old object
        assert_eq!(cache_expired_epoch, cache_hit_epoch);
    }

    #[tokio::test]
    async fn test_cache_upstream_revalidation_appends_headers() {
        init();
        let url = "http://127.0.0.1:6148/unique/test_cache_upstream_revalidation_appends_headers/cache_control";

        let res = reqwest::Client::new()
            .get(url)
            .header("set-cache-control", "public, max-age=1")
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let headers = res.headers();
        assert_eq!(headers["x-cache-status"], "miss");
        assert_eq!(headers["x-upstream-status"], "200");
        assert_eq!(headers["cache-control"], "public, max-age=1");
        assert_eq!(headers.get_all("cache-control").into_iter().count(), 1);
        assert_eq!(res.text().await.unwrap(), "hello world");

        let res = reqwest::Client::new()
            .get(url)
            .header("set-cache-control", "public, max-age=1")
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let headers = res.headers();
        assert_eq!(headers["x-cache-status"], "hit");
        assert!(headers.get("x-upstream-status").is_none());
        assert_eq!(headers.get_all("cache-control").into_iter().count(), 1);
        assert_eq!(res.text().await.unwrap(), "hello world");

        sleep(Duration::from_millis(1100)).await; // ttl is 1

        let res = reqwest::Client::new()
            .get(url)
            .header("set-cache-control", "public, max-age=1")
            .header("set-cache-control", "stale-while-revalidate=86400")
            .header("set-revalidated", "1")
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let headers = res.headers();
        assert_eq!(headers["x-cache-status"], "revalidated");
        assert_eq!(headers["x-upstream-status"], "304");
        let mut cc = headers.get_all("cache-control").into_iter();
        assert_eq!(cc.next().unwrap(), "public, max-age=1");
        assert_eq!(cc.next().unwrap(), "stale-while-revalidate=86400");
        assert!(cc.next().is_none());
        assert_eq!(res.text().await.unwrap(), "hello world");
    }

    #[tokio::test]
    async fn test_force_miss() {
        init();
        let url = "http://127.0.0.1:6148/unique/test_froce_miss/revalidate_now";

        let res = reqwest::get(url).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let headers = res.headers();
        let cache_miss_epoch = headers["x-epoch"].to_str().unwrap().parse::<f64>().unwrap();
        assert_eq!(headers["x-cache-status"], "miss");
        assert_eq!(headers["x-upstream-status"], "200");
        assert_eq!(res.text().await.unwrap(), "hello world");

        let res = reqwest::get(url).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let headers = res.headers();
        let cache_hit_epoch = headers["x-epoch"].to_str().unwrap().parse::<f64>().unwrap();
        assert_eq!(headers["x-cache-status"], "hit");
        assert!(headers.get("x-upstream-status").is_none());
        assert_eq!(res.text().await.unwrap(), "hello world");

        assert_eq!(cache_miss_epoch, cache_hit_epoch);

        let res = reqwest::Client::new()
            .get(url)
            .header("x-force-miss", "1")
            .send()
            .await
            .unwrap();

        assert_eq!(res.status(), StatusCode::OK);
        let headers = res.headers();
        assert_eq!(headers["x-cache-status"], "miss");
        assert_eq!(headers["x-upstream-status"], "200");
        assert_eq!(res.text().await.unwrap(), "hello world");
    }

    #[tokio::test]
    async fn test_force_miss_stale() {
        init();
        let url = "http://127.0.0.1:6148/unique/test_froce_miss_stale/revalidate_now";

        let res = reqwest::get(url).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let headers = res.headers();
        let cache_miss_epoch = headers["x-epoch"].to_str().unwrap().parse::<f64>().unwrap();
        assert_eq!(headers["x-cache-status"], "miss");
        assert_eq!(headers["x-upstream-status"], "200");
        assert_eq!(res.text().await.unwrap(), "hello world");

        let res = reqwest::get(url).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let headers = res.headers();
        let cache_hit_epoch = headers["x-epoch"].to_str().unwrap().parse::<f64>().unwrap();
        assert_eq!(headers["x-cache-status"], "hit");
        assert!(headers.get("x-upstream-status").is_none());
        assert_eq!(res.text().await.unwrap(), "hello world");

        assert_eq!(cache_miss_epoch, cache_hit_epoch);

        sleep(Duration::from_millis(1100)).await; // ttl is 1

        // stale, but can be forced miss
        let res = reqwest::Client::new()
            .get(url)
            .header("x-force-miss", "1")
            .send()
            .await
            .unwrap();

        assert_eq!(res.status(), StatusCode::OK);
        let headers = res.headers();
        assert_eq!(headers["x-cache-status"], "miss");
        assert_eq!(headers["x-upstream-status"], "200");
        let cache_miss_epoch2 = headers["x-epoch"].to_str().unwrap().parse::<f64>().unwrap();
        assert!(cache_miss_epoch != cache_miss_epoch2);
        assert_eq!(res.text().await.unwrap(), "hello world");
    }

    #[tokio::test]
    async fn test_cache_downstream_revalidation_etag() {
        init();
        let url = "http://127.0.0.1:6148/unique/test_downstream_revalidation_etag/revalidate_now";
        let client = reqwest::Client::new();

        // MISS + 304
        let res = client
            .get(url)
            .header("If-None-Match", "\"abcd\", \"foobar\"") // "abcd" is the fixed etag of this
            // endpoint
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_MODIFIED);
        let headers = res.headers();
        let cache_miss_epoch = headers["x-epoch"].to_str().unwrap().parse::<f64>().unwrap();
        assert_eq!(headers["x-cache-status"], "miss");
        assert_eq!(res.text().await.unwrap(), ""); // 304 no body

        // HIT + 304
        let res = client
            .get(url)
            .header("If-None-Match", "\"abcd\", \"foobar\"")
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_MODIFIED);
        let headers = res.headers();
        let cache_hit_epoch = headers["x-epoch"].to_str().unwrap().parse::<f64>().unwrap();
        assert_eq!(headers["x-cache-status"], "hit");
        assert_eq!(res.text().await.unwrap(), ""); // 304 no body

        assert_eq!(cache_miss_epoch, cache_hit_epoch);

        // HIT + 200 (condition passed)
        let res = client
            .get(url)
            .header("If-None-Match", "\"foobar\"")
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let headers = res.headers();
        let cache_hit_epoch = headers["x-epoch"].to_str().unwrap().parse::<f64>().unwrap();
        assert_eq!(headers["x-cache-status"], "hit");
        assert_eq!(res.text().await.unwrap(), "hello world");

        assert_eq!(cache_miss_epoch, cache_hit_epoch);

        sleep(Duration::from_millis(1100)).await; // ttl is 1

        // revalidated + 304
        let res = client
            .get(url)
            .header("If-None-Match", "\"abcd\", \"foobar\"")
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_MODIFIED);
        let headers = res.headers();
        let cache_expired_epoch = headers["x-epoch"].to_str().unwrap().parse::<f64>().unwrap();
        assert_eq!(headers["x-cache-status"], "revalidated");
        assert_eq!(res.text().await.unwrap(), ""); // 304 no body

        // still the old object
        assert_eq!(cache_expired_epoch, cache_hit_epoch);
    }

    #[tokio::test]
    async fn test_cache_downstream_revalidation_last_modified() {
        init();
        let url = "http://127.0.0.1:6148/unique/test_downstream_revalidation_last_modified/revalidate_now";
        let client = reqwest::Client::new();

        // MISS + 304
        let res = client
            .get(url)
            .header("If-Modified-Since", "Tue, 03 May 2022 01:04:39 GMT") // fixed last-modified of
            // the endpoint
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_MODIFIED);
        let headers = res.headers();
        let cache_miss_epoch = headers["x-epoch"].to_str().unwrap().parse::<f64>().unwrap();
        assert_eq!(headers["x-cache-status"], "miss");
        assert_eq!(res.text().await.unwrap(), ""); // 304 no body

        // HIT + 304
        let res = client
            .get(url)
            .header("If-Modified-Since", "Tue, 03 May 2022 01:11:39 GMT")
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_MODIFIED);
        let headers = res.headers();
        let cache_hit_epoch = headers["x-epoch"].to_str().unwrap().parse::<f64>().unwrap();
        assert_eq!(headers["x-cache-status"], "hit");
        assert_eq!(res.text().await.unwrap(), ""); // 304 no body

        assert_eq!(cache_miss_epoch, cache_hit_epoch);

        // HIT + 200 (condition passed)
        let res = client
            .get(url)
            .header("If-Modified-Since", "Tue, 03 May 2022 00:11:39 GMT")
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let headers = res.headers();
        let cache_hit_epoch = headers["x-epoch"].to_str().unwrap().parse::<f64>().unwrap();
        assert_eq!(headers["x-cache-status"], "hit");
        assert_eq!(res.text().await.unwrap(), "hello world");

        assert_eq!(cache_miss_epoch, cache_hit_epoch);

        sleep(Duration::from_millis(1100)).await; // ttl is 1

        // revalidated + 304
        let res = client
            .get(url)
            .header("If-Modified-Since", "Tue, 03 May 2022 01:11:39 GMT")
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_MODIFIED);
        let headers = res.headers();
        let cache_expired_epoch = headers["x-epoch"].to_str().unwrap().parse::<f64>().unwrap();
        assert_eq!(headers["x-cache-status"], "revalidated");
        assert_eq!(res.text().await.unwrap(), ""); // 304 no body

        // still the old object
        assert_eq!(cache_expired_epoch, cache_hit_epoch);
    }

    #[tokio::test]
    async fn test_cache_downstream_head() {
        init();
        let url = "http://127.0.0.1:6148/unique/test_downstream_head/revalidate_now";
        let client = reqwest::Client::new();

        // MISS + HEAD
        let res = client.head(url).send().await.unwrap();

        assert_eq!(res.status(), StatusCode::OK);
        let headers = res.headers();
        let cache_miss_epoch = headers["x-epoch"].to_str().unwrap().parse::<f64>().unwrap();
        assert_eq!(headers["x-cache-status"], "miss");
        assert_eq!(res.text().await.unwrap(), ""); // HEAD no body

        // HIT + HEAD
        let res = client.head(url).send().await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let headers = res.headers();
        let cache_hit_epoch = headers["x-epoch"].to_str().unwrap().parse::<f64>().unwrap();
        assert_eq!(headers["x-cache-status"], "hit");
        assert_eq!(res.text().await.unwrap(), ""); // HEAD no body

        assert_eq!(cache_miss_epoch, cache_hit_epoch);

        sleep(Duration::from_millis(1100)).await; // ttl is 1

        // revalidated + HEAD
        let res = client.head(url).send().await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let headers = res.headers();
        let cache_expired_epoch = headers["x-epoch"].to_str().unwrap().parse::<f64>().unwrap();
        assert_eq!(headers["x-cache-status"], "revalidated");
        assert_eq!(res.text().await.unwrap(), ""); // HEAD no body

        // still the old object
        assert_eq!(cache_expired_epoch, cache_hit_epoch);
    }

    #[tokio::test]
    async fn test_purge_reject() {
        init();

        let res = reqwest::Client::builder()
            .build()
            .unwrap()
            .request(
                reqwest::Method::from_bytes(b"PURGE").unwrap(),
                "http://127.0.0.1:6148/",
            )
            .header("x-bypass-cache", "1") // not to cache this one
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::METHOD_NOT_ALLOWED);
        assert_eq!(res.text().await.unwrap(), "");
    }

    #[tokio::test]
    async fn test_1xx_caching() {
        // 1xx shouldn't interfere with HTTP caching

        // set up a one-off mock server
        // (warp / hyper don't have custom 1xx sending capabilities yet)
        async fn mock_1xx_server(port: u16, cc_header: &str) {
            use tokio::io::AsyncWriteExt;

            let listener = tokio::net::TcpListener::bind(format!("127.0.0.1:{}", port))
                .await
                .unwrap();
            if let Ok((mut stream, _addr)) = listener.accept().await {
                stream.write_all(b"HTTP/1.1 103 Early Hints\r\nLink: <https://foo.bar>; rel=preconnect\r\n\r\n").await.unwrap();
                // wait a bit so that the client can read
                sleep(Duration::from_millis(100)).await;
                stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Length: 5\r\nCache-Control: {}\r\n\r\nhello", cc_header).as_bytes()).await.unwrap();
                sleep(Duration::from_millis(100)).await;
            }
        }

        init();

        let url = "http://127.0.0.1:6148/unique/test_1xx_caching";

        tokio::spawn(async {
            mock_1xx_server(6151, "max-age=5").await;
        });
        // wait for server to start
        sleep(Duration::from_millis(100)).await;

        let client = reqwest::Client::new();
        let res = client
            .get(url)
            .header("x-port", "6151")
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let headers = res.headers();
        assert_eq!(headers["x-cache-status"], "miss");
        assert_eq!(res.text().await.unwrap(), "hello");

        let res = client
            .get(url)
            .header("x-port", "6151")
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let headers = res.headers();
        assert_eq!(headers["x-cache-status"], "hit");
        assert_eq!(res.text().await.unwrap(), "hello");

        // 1xx shouldn't interfere with bypass
        let url = "http://127.0.0.1:6148/unique/test_1xx_bypass";

        tokio::spawn(async {
            mock_1xx_server(6152, "private, no-store").await;
        });
        // wait for server to start
        sleep(Duration::from_millis(100)).await;

        let res = client
            .get(url)
            .header("x-port", "6152")
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let headers = res.headers();
        assert_eq!(headers["x-cache-status"], "no-cache");
        assert_eq!(res.text().await.unwrap(), "hello");

        // restart the one-off server - still uncacheable
        sleep(Duration::from_millis(100)).await;
        tokio::spawn(async {
            mock_1xx_server(6152, "private, no-store").await;
        });
        // wait for server to start
        sleep(Duration::from_millis(100)).await;

        let res = client
            .get(url)
            .header("x-port", "6152")
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let headers = res.headers();
        assert_eq!(headers["x-cache-status"], "no-cache");
        assert_eq!(res.text().await.unwrap(), "hello");
    }

    #[tokio::test]
    async fn test_bypassed_became_cacheable() {
        init();

        let url = "http://127.0.0.1:6148/unique/test_bypassed/cache_control";

        let res = reqwest::Client::new()
            .get(url)
            .header("set-cache-control", "private, max-age=0")
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let headers = res.headers();
        let cc = headers.get("Cache-Control").unwrap();
        assert_eq!(cc, "private, max-age=0");
        assert_eq!(headers["x-cache-status"], "no-cache");
        assert_eq!(res.text().await.unwrap(), "hello world");

        // request should bypass cache, but became cacheable (cache fill)
        let res = reqwest::Client::new()
            .get(url)
            .header("set-cache-control", "public, max-age=10")
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let headers = res.headers();
        assert_eq!(headers["x-cache-status"], "miss");
        assert_eq!(res.text().await.unwrap(), "hello world");

        // HIT
        let res = reqwest::Client::new()
            .get(url)
            .header("set-cache-control", "public, max-age=10")
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let headers = res.headers();
        assert_eq!(headers["x-cache-status"], "hit");
        assert_eq!(res.text().await.unwrap(), "hello world");
    }

    #[tokio::test]
    async fn test_bypassed_304() {
        init();

        let url = "http://127.0.0.1:6148/unique/test_bypassed_304/cache_control";

        let res = reqwest::Client::new()
            .get(url)
            .header("set-cache-control", "private, max-age=0")
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let headers = res.headers();
        let cc = headers.get("Cache-Control").unwrap();
        assert_eq!(cc, "private, max-age=0");
        assert_eq!(headers["x-cache-status"], "no-cache");
        assert_eq!(res.text().await.unwrap(), "hello world");

        // cacheable without private cache-control
        // note this will be a 304 and not a 200, we will cache on _next_ request
        let res = reqwest::Client::new()
            .get(url)
            .header("set-cache-control", "public, max-age=10")
            .header("set-revalidated", "1")
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_MODIFIED);
        let headers = res.headers();
        assert_eq!(headers["x-cache-status"], "deferred");

        // should be cache fill
        let res = reqwest::Client::new()
            .get(url)
            .header("set-cache-control", "public, max-age=10")
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let headers = res.headers();
        assert_eq!(headers["x-cache-status"], "miss");
        assert_eq!(res.text().await.unwrap(), "hello world");

        // HIT
        let res = reqwest::Client::new()
            .get(url)
            .header("set-cache-control", "public, max-age=10")
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let headers = res.headers();
        assert_eq!(headers["x-cache-status"], "hit");
        assert_eq!(res.text().await.unwrap(), "hello world");
    }

    #[tokio::test]
    async fn test_bypassed_uncacheable_304() {
        init();

        let url = "http://127.0.0.1:6148/unique/test_bypassed_private_304/cache_control";

        // cache fill
        let res = reqwest::Client::new()
            .get(url)
            .header("set-cache-control", "public, max-age=0")
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let headers = res.headers();
        let cc = headers.get("Cache-Control").unwrap();
        assert_eq!(cc, "public, max-age=0");
        assert_eq!(headers["x-cache-status"], "miss");
        assert_eq!(res.text().await.unwrap(), "hello world");

        // cache stale
        // upstream returns 304, but response became uncacheable
        let res = reqwest::Client::new()
            .get(url)
            .header("set-cache-control", "private")
            .header("set-revalidated", "1")
            .send()
            .await
            .unwrap();
        // should see the response body because we didn't send conditional headers
        assert_eq!(res.status(), StatusCode::OK);
        let headers = res.headers();
        assert_eq!(headers["x-cache-status"], "revalidated");
        assert_eq!(res.text().await.unwrap(), "hello world");

        // we bypass cache for this next request
        let res = reqwest::Client::new()
            .get(url)
            .header("set-cache-control", "public, max-age=10")
            .header("set-revalidated", "1") // non-200 status to get bypass phase
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_MODIFIED);
        let headers = res.headers();
        assert_eq!(headers["x-cache-status"], "deferred");
    }

    #[tokio::test]
    async fn test_bypassed_head() {
        init();

        let url = "http://127.0.0.1:6148/unique/test_bypassed_head/cache_control";

        // uncacheable, should bypass
        let res = reqwest::Client::new()
            .get(url)
            .header("set-cache-control", "private, max-age=0")
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let headers = res.headers();
        assert_eq!(headers["x-cache-status"], "no-cache");
        assert_eq!(res.text().await.unwrap(), "hello world");

        // we bypass cache for this next request, becomes cacheable
        let res = reqwest::Client::new()
            .head(url)
            .header("set-cache-control", "public, max-age=10")
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let headers = res.headers();
        // should not cache the response
        assert_eq!(headers["x-cache-status"], "deferred");

        // MISS
        let res = reqwest::Client::new()
            .get(url)
            .header("set-cache-control", "public, max-age=10")
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let headers = res.headers();
        assert_eq!(headers["x-cache-status"], "miss");
        assert_eq!(res.text().await.unwrap(), "hello world");
    }

    #[tokio::test]
    async fn test_eviction() {
        init();
        let url = "http://127.0.0.1:6148/file_maker/test_eviction".to_owned();

        // admit asset 1
        let res = reqwest::Client::new()
            .get(url.clone() + "1")
            .header("x-set-size", "3000")
            .header("x-eviction", "1") // tell test proxy to use eviction manager
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let headers = res.headers();
        assert_eq!(headers["x-cache-status"], "miss");
        assert_eq!(res.text().await.unwrap().len(), 3000);

        // admit asset 2
        let res = reqwest::Client::new()
            .get(url.clone() + "2")
            .header("x-set-size", "3000")
            .header("x-eviction", "1") // tell test proxy to use eviction manager
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let headers = res.headers();
        assert_eq!(headers["x-cache-status"], "miss");
        assert_eq!(res.text().await.unwrap().len(), 3000);

        // touch asset 2
        let res = reqwest::Client::new()
            .get(url.clone() + "2")
            .header("x-set-size", "3000")
            .header("x-eviction", "1") // tell test proxy to use eviction manager
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let headers = res.headers();
        assert_eq!(headers["x-cache-status"], "hit");
        assert_eq!(res.text().await.unwrap().len(), 3000);

        // touch asset 1
        let res = reqwest::Client::new()
            .get(url.clone() + "1")
            .header("x-set-size", "3000")
            .header("x-eviction", "1") // tell test proxy to use eviction manager
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let headers = res.headers();
        assert_eq!(headers["x-cache-status"], "hit");
        assert_eq!(res.text().await.unwrap().len(), 3000);

        // admit asset 3
        let res = reqwest::Client::new()
            .get(url.clone() + "3")
            .header("x-set-size", "6000")
            .header("x-eviction", "1") // tell test proxy to use eviction manager
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let headers = res.headers();
        assert_eq!(headers["x-cache-status"], "miss");
        assert_eq!(res.text().await.unwrap().len(), 6000);

        // check asset 2, it should be evicted already because admitting asset 3 made it full
        let res = reqwest::Client::new()
            .get(url.clone() + "2")
            .header("x-set-size", "3000")
            .header("x-eviction", "1") // tell test proxy to use eviction manager
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let headers = res.headers();
        assert_eq!(headers["x-cache-status"], "miss"); // evicted
        assert_eq!(res.text().await.unwrap().len(), 3000);
    }

    #[tokio::test]
    async fn test_cache_lock_miss_hit() {
        init();
        let url = "http://127.0.0.1:6148/sleep/test_cache_lock_miss_hit.txt";

        // no lock, parallel fetches to a slow origin are all misses
        tokio::spawn(async move {
            let res = reqwest::Client::new().get(url).send().await.unwrap();
            assert_eq!(res.status(), StatusCode::OK);
            let headers = res.headers();
            assert_eq!(headers["x-cache-status"], "miss");
            assert_eq!(res.text().await.unwrap(), "hello world");
        });

        tokio::spawn(async move {
            let res = reqwest::Client::new().get(url).send().await.unwrap();
            assert_eq!(res.status(), StatusCode::OK);
            let headers = res.headers();
            assert_eq!(headers["x-cache-status"], "miss");
            assert_eq!(res.text().await.unwrap(), "hello world");
        });

        tokio::spawn(async move {
            let res = reqwest::Client::new().get(url).send().await.unwrap();
            assert_eq!(res.status(), StatusCode::OK);
            let headers = res.headers();
            assert_eq!(headers["x-cache-status"], "miss");
            assert_eq!(res.text().await.unwrap(), "hello world");
        })
        .await
        .unwrap(); // wait for at least one of them to finish

        let res = reqwest::Client::new().get(url).send().await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let headers = res.headers();
        assert_eq!(headers["x-cache-status"], "hit");
        assert_eq!(res.text().await.unwrap(), "hello world");

        // try with lock
        let url = "http://127.0.0.1:6148/sleep/test_cache_lock_miss_hit2.txt";
        let task1 = tokio::spawn(async move {
            let res = reqwest::Client::new()
                .get(url)
                .header("x-lock", "true")
                .send()
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::OK);
            let headers = res.headers();
            assert_eq!(headers["x-cache-status"], "miss");
            assert_eq!(res.text().await.unwrap(), "hello world");
        });
        // sleep just a little to make sure the req above gets the cache lock
        sleep(Duration::from_millis(50)).await;
        let task2 = tokio::spawn(async move {
            let res = reqwest::Client::new()
                .get(url)
                .header("x-lock", "true")
                .send()
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::OK);
            let headers = res.headers();
            assert_eq!(headers["x-cache-status"], "hit");
            let lock_time_ms: u32 = headers["x-cache-lock-time-ms"]
                .to_str()
                .unwrap()
                .parse()
                .unwrap();
            assert!(lock_time_ms > 900 && lock_time_ms < 1000);
            assert_eq!(res.text().await.unwrap(), "hello world");
        });
        let task3 = tokio::spawn(async move {
            let res = reqwest::Client::new()
                .get(url)
                .header("x-lock", "true")
                .send()
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::OK);
            let headers = res.headers();
            assert_eq!(headers["x-cache-status"], "hit");
            let lock_time_ms: u32 = headers["x-cache-lock-time-ms"]
                .to_str()
                .unwrap()
                .parse()
                .unwrap();
            assert!(lock_time_ms > 900 && lock_time_ms < 1000);
            assert_eq!(res.text().await.unwrap(), "hello world");
        });

        task1.await.unwrap();
        task2.await.unwrap();
        task3.await.unwrap();
    }

    #[tokio::test]
    async fn test_cache_lock_expired() {
        init();
        let url = "http://127.0.0.1:6148/sleep/test_cache_lock_expired.txt";

        // cache one
        let res = reqwest::Client::new()
            .get(url)
            .header("x-no-stale-revalidate", "true")
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let headers = res.headers();
        assert_eq!(headers["x-cache-status"], "miss");
        assert_eq!(res.text().await.unwrap(), "hello world");
        // let it stale
        sleep(Duration::from_secs(1)).await;

        let task1 = tokio::spawn(async move {
            let res = reqwest::Client::new()
                .get(url)
                .header("x-lock", "true")
                .header("x-no-stale-revalidate", "true")
                .send()
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::OK);
            let headers = res.headers();
            assert_eq!(headers["x-cache-status"], "expired");
            assert_eq!(res.text().await.unwrap(), "hello world");
        });
        // sleep just a little to make sure the req above gets the cache lock
        sleep(Duration::from_millis(50)).await;
        let task2 = tokio::spawn(async move {
            let res = reqwest::Client::new()
                .get(url)
                .header("x-lock", "true")
                .header("x-no-stale-revalidate", "true")
                .send()
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::OK);
            let headers = res.headers();
            assert_eq!(headers["x-cache-status"], "hit");
            assert_eq!(res.text().await.unwrap(), "hello world");
        });
        let task3 = tokio::spawn(async move {
            let res = reqwest::Client::new()
                .get(url)
                .header("x-lock", "true")
                .header("x-no-stale-revalidate", "true")
                .send()
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::OK);
            let headers = res.headers();
            assert_eq!(headers["x-cache-status"], "hit");
            assert_eq!(res.text().await.unwrap(), "hello world");
        });

        task1.await.unwrap();
        task2.await.unwrap();
        task3.await.unwrap();
    }

    #[tokio::test]
    async fn test_cache_lock_network_error() {
        init();
        let url = "http://127.0.0.1:6148/sleep/test_cache_lock_network_error.txt";

        let task1 = tokio::spawn(async move {
            let res = reqwest::Client::new()
                .get(url)
                .header("x-lock", "true")
                .header("x-set-sleep", "0.3") // sometimes we hit the retry logic which is x3 slow
                .header("x-abort", "true") // this will tell origin to kill the conn right away
                .send()
                .await
                .unwrap();
            assert_eq!(res.status(), 502); // error happened
        });
        // sleep just a little to make sure the req above gets the cache lock
        sleep(Duration::from_millis(50)).await;

        let task2 = tokio::spawn(async move {
            let res = reqwest::Client::new()
                .get(url)
                .header("x-lock", "true")
                .send()
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::OK);
            let headers = res.headers();
            let status = headers["x-cache-status"].to_owned();
            assert_eq!(res.text().await.unwrap(), "hello world");
            status
        });
        let task3 = tokio::spawn(async move {
            let res = reqwest::Client::new()
                .get(url)
                .header("x-lock", "true")
                .send()
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::OK);
            let headers = res.headers();
            let status = headers["x-cache-status"].to_owned();
            assert_eq!(res.text().await.unwrap(), "hello world");
            status
        });

        task1.await.unwrap();
        let status2 = task2.await.unwrap();
        let status3 = task3.await.unwrap();

        let mut count_miss = 0;
        if status2 == "miss" {
            count_miss += 1;
        }
        if status3 == "miss" {
            count_miss += 1;
        }
        assert_eq!(count_miss, 1);
    }

    #[tokio::test]
    async fn test_cache_lock_uncacheable() {
        init();
        let url = "http://127.0.0.1:6148/sleep/test_cache_lock_uncacheable.txt";

        let task1 = tokio::spawn(async move {
            let res = reqwest::Client::new()
                .get(url)
                .header("x-lock", "true")
                .header("x-no-store", "true") // tell origin to return CC: no-store
                .send()
                .await
                .unwrap();
            assert_eq!(res.status(), 200);
            let headers = res.headers();
            assert_eq!(headers["x-cache-status"], "no-cache");
            assert_eq!(res.text().await.unwrap(), "hello world");
        });
        // sleep just a little to make sure the req above gets the cache lock
        sleep(Duration::from_millis(50)).await;

        let task2 = tokio::spawn(async move {
            let res = reqwest::Client::new()
                .get(url)
                .header("x-lock", "true")
                .send()
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::OK);
            let headers = res.headers();
            assert_eq!(headers["x-cache-status"], "no-cache");
            assert_eq!(res.text().await.unwrap(), "hello world");
        });
        let task3 = tokio::spawn(async move {
            let res = reqwest::Client::new()
                .get(url)
                .header("x-lock", "true")
                .send()
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::OK);
            let headers = res.headers();
            assert_eq!(headers["x-cache-status"], "no-cache");
            assert_eq!(res.text().await.unwrap(), "hello world");
        });

        task1.await.unwrap();
        task2.await.unwrap();
        task3.await.unwrap();
    }

    #[tokio::test]
    async fn test_cache_lock_timeout() {
        init();
        let url = "http://127.0.0.1:6148/sleep/test_cache_lock_timeout.txt";

        let task1 = tokio::spawn(async move {
            let res = reqwest::Client::new()
                .get(url)
                .header("x-lock", "true")
                .header("x-set-sleep", "3") // we have a 2 second cache lock timeout
                .send()
                .await
                .unwrap();
            assert_eq!(res.status(), 200);
            let headers = res.headers();
            assert_eq!(headers["x-cache-status"], "miss");
            assert_eq!(res.text().await.unwrap(), "hello world");
        });
        // sleep just a little to make sure the req above gets the cache lock
        sleep(Duration::from_millis(50)).await;

        let task2 = tokio::spawn(async move {
            let res = reqwest::Client::new()
                .get(url)
                .header("x-lock", "true")
                .header("x-set-sleep", "0.1") // tell origin to return faster
                .send()
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::OK);
            let headers = res.headers();
            assert_eq!(headers["x-cache-status"], "no-cache");
            assert_eq!(res.text().await.unwrap(), "hello world");
        });

        // send the 3rd request after the 2 second cache lock timeout where the
        // first request still holds the lock (3s delay in origin)
        sleep(Duration::from_millis(2000)).await;
        let task3 = tokio::spawn(async move {
            let res = reqwest::Client::new()
                .get(url)
                .header("x-lock", "true")
                .header("x-set-sleep", "0.1")
                .send()
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::OK);
            let headers = res.headers();
            assert_eq!(headers["x-cache-status"], "no-cache");
            assert_eq!(res.text().await.unwrap(), "hello world");
        });

        task1.await.unwrap();
        task2.await.unwrap();
        task3.await.unwrap();

        let res = reqwest::Client::new()
            .get(url)
            .header("x-lock", "true")
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
        let headers = res.headers();
        assert_eq!(headers["x-cache-status"], "hit"); // the first request cached it
        assert_eq!(res.text().await.unwrap(), "hello world");
    }

    #[tokio::test]
    async fn test_cache_serve_stale_network_error() {
        init();
        let url = "http://127.0.0.1:6148/sleep/test_cache_serve_stale_network_error.txt";

        let res = reqwest::Client::new()
            .get(url)
            .header("x-set-sleep", "0") // no need to sleep we just reuse this endpoint
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
        let headers = res.headers();
        assert_eq!(headers["x-cache-status"], "miss");
        assert_eq!(res.text().await.unwrap(), "hello world");

        sleep(Duration::from_millis(1100)).await; // ttl is 1

        let res = reqwest::Client::new()
            .get(url)
            .header("x-set-sleep", "0") // no need to sleep we just reuse this endpoint
            .header("x-abort", "true") // this will tell origin to kill the conn right away
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
        let headers = res.headers();
        assert_eq!(headers["x-cache-status"], "stale");
        assert_eq!(res.text().await.unwrap(), "hello world");
    }

    #[tokio::test]
    async fn test_cache_serve_stale_network_error_mid_response() {
        init();
        let url =
            "http://127.0.0.1:6148/sleep/test_cache_serve_stale_network_error_mid_response.txt";

        let res = reqwest::Client::new()
            .get(url)
            .header("x-set-sleep", "0") // no need to sleep we just reuse this endpoint
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
        let headers = res.headers();
        assert_eq!(headers["x-cache-status"], "miss");
        assert_eq!(res.text().await.unwrap(), "hello world");

        sleep(Duration::from_millis(1100)).await; // ttl is 1

        let res = reqwest::Client::new()
            .get(url)
            .header("x-set-sleep", "0") // no need to sleep we just reuse this endpoint
            .header("x-set-body-sleep", "0.1") // pause the body a bit before abort
            .header("x-abort-body", "true") // this will tell origin to kill the conn right away
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
        let headers = res.headers();
        assert_eq!(headers["x-cache-status"], "expired");
        // the connection dies
        assert!(res.text().await.is_err());
    }

    #[tokio::test]
    async fn test_cache_serve_stale_on_500() {
        init();
        let url = "http://127.0.0.1:6148/sleep/test_cache_serve_stale_on_500.txt";

        let res = reqwest::Client::new()
            .get(url)
            .header("x-set-sleep", "0") // no need to sleep we just reuse this endpoint
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
        let headers = res.headers();
        assert_eq!(headers["x-cache-status"], "miss");
        assert_eq!(res.text().await.unwrap(), "hello world");

        sleep(Duration::from_millis(1100)).await; // ttl is 1

        let res = reqwest::Client::new()
            .get(url)
            .header("x-set-sleep", "0") // no need to sleep we just reuse this endpoint
            .header("x-error-header", "true") // this will tell origin to return 500
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
        let headers = res.headers();
        assert_eq!(headers["x-cache-status"], "stale");
        assert_eq!(res.text().await.unwrap(), "hello world");
    }

    #[tokio::test]
    async fn test_stale_while_revalidate_many_readers() {
        init();
        let url = "http://127.0.0.1:6148/sleep/test_stale_while_revalidate_many_readers.txt";

        // cache one
        let res = reqwest::Client::new().get(url).send().await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let headers = res.headers();
        assert_eq!(headers["x-cache-status"], "miss");
        assert_eq!(res.text().await.unwrap(), "hello world");
        // let it stale
        sleep(Duration::from_secs(1)).await;

        let task1 = tokio::spawn(async move {
            let res = reqwest::Client::new()
                .get(url)
                .header("x-lock", "true")
                .send()
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::OK);
            let headers = res.headers();
            assert_eq!(headers["x-cache-status"], "stale-updating");
            assert_eq!(res.text().await.unwrap(), "hello world");
        });
        // sleep just a little to make sure the req above gets the cache lock
        sleep(Duration::from_millis(50)).await;
        let task2 = tokio::spawn(async move {
            let res = reqwest::Client::new()
                .get(url)
                .header("x-lock", "true")
                .send()
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::OK);
            let headers = res.headers();
            assert_eq!(headers["x-cache-status"], "stale-updating");
            assert_eq!(res.text().await.unwrap(), "hello world");
        });
        let task3 = tokio::spawn(async move {
            let res = reqwest::Client::new()
                .get(url)
                .header("x-lock", "true")
                .send()
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::OK);
            let headers = res.headers();
            assert_eq!(headers["x-cache-status"], "stale-updating");
            assert_eq!(res.text().await.unwrap(), "hello world");
        });

        task1.await.unwrap();
        task2.await.unwrap();
        task3.await.unwrap();
    }

    #[tokio::test]
    async fn test_stale_while_revalidate_single_request() {
        init();
        let url = "http://127.0.0.1:6148/sleep/test_stale_while_revalidate_single_request.txt";

        // cache one
        let res = reqwest::Client::new()
            .get(url)
            .header("x-set-sleep", "0")
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let headers = res.headers();
        assert_eq!(headers["x-cache-status"], "miss");
        assert_eq!(res.text().await.unwrap(), "hello world");
        // let it stale
        sleep(Duration::from_secs(1)).await;

        let res = reqwest::Client::new()
            .get(url)
            .header("x-lock", "true")
            .header("x-set-sleep", "0") // by default /sleep endpoint will sleep 1s
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let headers = res.headers();
        assert_eq!(headers["x-cache-status"], "stale-updating");
        assert_eq!(res.text().await.unwrap(), "hello world");

        // wait for the background request to finish
        sleep(Duration::from_millis(100)).await;

        let res = reqwest::Client::new()
            .get(url)
            .header("x-lock", "true")
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let headers = res.headers();
        assert_eq!(headers["x-cache-status"], "hit"); // fresh
        assert_eq!(res.text().await.unwrap(), "hello world");
    }

    #[tokio::test]
    async fn test_cache_streaming_partial_body() {
        init();
        let url = "http://127.0.0.1:6148/slow_body/test_cache_streaming_partial_body.txt";
        let task1 = tokio::spawn(async move {
            let res = reqwest::Client::new()
                .get(url)
                .header("x-lock", "true")
                .send()
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::OK);
            let headers = res.headers();
            assert_eq!(headers["x-cache-status"], "miss");
            assert_eq!(res.text().await.unwrap(), "hello world!");
        });
        // sleep just a little to make sure the req above gets the cache lock
        sleep(Duration::from_millis(50)).await;

        let task2 = tokio::spawn(async move {
            let res = reqwest::Client::new()
                .get(url)
                .header("x-lock", "true")
                .send()
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::OK);
            let headers = res.headers();
            assert_eq!(headers["x-cache-status"], "hit");
            let lock_time_ms: u32 = headers["x-cache-lock-time-ms"]
                .to_str()
                .unwrap()
                .parse()
                .unwrap();
            // the entire body should need 2 extra seconds, here the test shows that
            // only the header is under cache lock and the body should be streamed
            assert!(lock_time_ms > 900 && lock_time_ms < 1000);
            assert_eq!(res.text().await.unwrap(), "hello world!");
        });
        let task3 = tokio::spawn(async move {
            let res = reqwest::Client::new()
                .get(url)
                .header("x-lock", "true")
                .send()
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::OK);
            let headers = res.headers();
            assert_eq!(headers["x-cache-status"], "hit");
            let lock_time_ms: u32 = headers["x-cache-lock-time-ms"]
                .to_str()
                .unwrap()
                .parse()
                .unwrap();
            // the entire body should need 2 extra seconds, here the test shows that
            // only the header is under cache lock and the body should be streamed
            assert!(lock_time_ms > 900 && lock_time_ms < 1000);
            assert_eq!(res.text().await.unwrap(), "hello world!");
        });

        task1.await.unwrap();
        task2.await.unwrap();
        task3.await.unwrap();
    }

    #[tokio::test]
    async fn test_cache_streaming_multiple_writers() {
        // multiple streaming writers don't conflict
        init();
        let url = "http://127.0.0.1:6148/slow_body/test_cache_streaming_multiple_writers.txt";
        let task1 = tokio::spawn(async move {
            let res = reqwest::Client::new()
                .get(url)
                .header("x-set-hello", "everyone")
                .send()
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::OK);
            let headers = res.headers();
            assert_eq!(headers["x-cache-status"], "miss");
            assert_eq!(res.text().await.unwrap(), "hello everyone!");
        });

        let task2 = tokio::spawn(async move {
            let res = reqwest::Client::new()
                .get(url)
                // don't allow using the other streaming write's result
                .header("x-force-expire", "1")
                .header("x-set-hello", "todo el mundo")
                .send()
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::OK);
            let headers = res.headers();
            assert_eq!(headers["x-cache-status"], "miss");
            assert_eq!(res.text().await.unwrap(), "hello todo el mundo!");
        });

        task1.await.unwrap();
        task2.await.unwrap();
    }

    #[tokio::test]
    async fn test_range_request() {
        init();
        let url = "http://127.0.0.1:6148/unique/test_range_request/now";

        let res = reqwest::Client::new()
            .get(url)
            .header("Range", "bytes=0-1")
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::PARTIAL_CONTENT);
        let headers = res.headers();
        let cache_miss_epoch = headers["x-epoch"].to_str().unwrap().parse::<f64>().unwrap();
        assert_eq!(headers["x-cache-status"], "miss");
        assert_eq!(res.text().await.unwrap(), "he");

        // full body is cached
        let res = reqwest::get(url).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let headers = res.headers();
        let cache_hit_epoch = headers["x-epoch"].to_str().unwrap().parse::<f64>().unwrap();
        assert_eq!(headers["x-cache-status"], "hit");
        assert_eq!(res.text().await.unwrap(), "hello world");

        assert_eq!(cache_miss_epoch, cache_hit_epoch);

        let res = reqwest::Client::new()
            .get(url)
            .header("Range", "bytes=0-1")
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::PARTIAL_CONTENT);
        let headers = res.headers();
        assert_eq!(headers["x-cache-status"], "hit");
        assert_eq!(res.text().await.unwrap(), "he");

        let res = reqwest::Client::new()
            .get(url)
            .header("Range", "bytes=1-0")
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::RANGE_NOT_SATISFIABLE);
        let headers = res.headers();
        assert_eq!(headers["x-cache-status"], "hit");
        assert_eq!(res.text().await.unwrap(), "");

        let res = reqwest::Client::new()
            .head(url)
            .header("Range", "bytes=0-1")
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::PARTIAL_CONTENT);
        let headers = res.headers();
        assert_eq!(headers["x-cache-status"], "hit");
        assert_eq!(res.text().await.unwrap(), "");

        sleep(Duration::from_millis(1100)).await; // ttl is 1

        let res = reqwest::Client::new()
            .get(url)
            .header("Range", "bytes=0-1")
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::PARTIAL_CONTENT);
        let headers = res.headers();
        assert_eq!(headers["x-cache-status"], "expired");
        assert_eq!(res.text().await.unwrap(), "he");
    }

    #[tokio::test]
    async fn test_caching_when_downstream_bails() {
        init();
        let url = "http://127.0.0.1:6148/slow_body/test_caching_when_downstream_bails/";

        tokio::spawn(async move {
            let res = reqwest::Client::new()
                .get(url)
                .header("x-lock", "true")
                .send()
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::OK);
            let headers = res.headers();
            assert_eq!(headers["x-cache-status"], "miss");
            // exit without res.text().await so that we bail early
        });
        // sleep just a little to make sure the req above gets the cache lock
        sleep(Duration::from_millis(50)).await;

        let res = reqwest::Client::new()
            .get(url)
            .header("x-lock", "true")
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let headers = res.headers();
        assert_eq!(headers["x-cache-status"], "hit");
        let lock_time_ms: u32 = headers["x-cache-lock-time-ms"]
            .to_str()
            .unwrap()
            .parse()
            .unwrap();
        // the entire body should need 2 extra seconds, here the test shows that
        // only the header is under cache lock and the body should be streamed
        assert!(lock_time_ms > 900 && lock_time_ms < 1000);
        assert_eq!(res.text().await.unwrap(), "hello world!");
    }

    async fn send_vary_req_with_headers_with_dups(
        url: &str,
        vary_field: &str,
        headers: Vec<(&str, &str)>,
        dup_headers: Vec<(&str, &str)>,
    ) -> reqwest::Response {
        let req_headers = headers
            .iter()
            .map(|(name, value)| {
                (
                    HeaderName::from_str(name).unwrap(),
                    HeaderValue::from_str(value).unwrap(),
                )
            })
            .collect();
        let mut req_builder = reqwest::Client::new()
            .get(url)
            .headers(req_headers)
            .header("set-vary", vary_field);

        // Apply any duplicate headers
        for (key, value) in dup_headers {
            req_builder = req_builder.header(key, value);
        }

        req_builder.send().await.unwrap()
    }

    async fn send_vary_req_with_headers(
        url: &str,
        vary_field: &str,
        headers: Vec<(&str, &str)>,
    ) -> reqwest::Response {
        send_vary_req_with_headers_with_dups(url, vary_field, headers, vec![]).await
    }

    async fn send_vary_req(url: &str, vary_field: &str, value: &str) -> reqwest::Response {
        send_vary_req_with_headers(url, vary_field, vec![(vary_field, value)]).await
    }

    #[tokio::test]
    async fn test_vary_caching() {
        init();
        let url = "http://127.0.0.1:6148/unique/test_vary_caching/vary";
        let vary_field = "Accept";

        let res = send_vary_req(url, vary_field, "image/png").await;
        assert_eq!(res.status(), StatusCode::OK);
        let headers = res.headers();
        let cache_a_miss_epoch = headers["x-epoch"].to_str().unwrap().parse::<f64>().unwrap();
        assert_eq!(headers["x-cache-status"], "miss");
        assert_eq!(res.text().await.unwrap(), "hello world");

        let res = send_vary_req(url, vary_field, "image/png").await;
        assert_eq!(res.status(), StatusCode::OK);
        let headers = res.headers();
        let cache_hit_epoch = headers["x-epoch"].to_str().unwrap().parse::<f64>().unwrap();
        assert_eq!(headers["x-cache-status"], "hit");
        assert_eq!(res.text().await.unwrap(), "hello world");

        assert_eq!(cache_a_miss_epoch, cache_hit_epoch);

        let res = send_vary_req(url, vary_field, "image/jpeg").await;
        assert_eq!(res.status(), StatusCode::OK);
        let headers = res.headers();
        let cache_b_miss_epoch = headers["x-epoch"].to_str().unwrap().parse::<f64>().unwrap();
        assert_eq!(headers["x-cache-status"], "miss");
        assert_eq!(res.text().await.unwrap(), "hello world");

        let res = send_vary_req(url, vary_field, "image/jpeg").await;
        assert_eq!(res.status(), StatusCode::OK);
        let headers = res.headers();
        let cache_hit_epoch = headers["x-epoch"].to_str().unwrap().parse::<f64>().unwrap();
        assert_eq!(headers["x-cache-status"], "hit");
        assert_eq!(res.text().await.unwrap(), "hello world");

        assert_eq!(cache_b_miss_epoch, cache_hit_epoch);
        assert!(cache_a_miss_epoch != cache_b_miss_epoch);
    }

    #[tokio::test]
    async fn test_vary_caching_ignored_vary_header() {
        init();
        let url = "http://127.0.0.1:6148/unique/test_vary_caching_ignored_vary_header/vary";
        let vary_field = "Some-Ignored-Vary-Header";

        // Asset into cache (png)
        let res = send_vary_req(url, vary_field, "image/png").await;
        assert_eq!(res.status(), StatusCode::OK);
        let headers = res.headers();
        let epoch = headers["x-epoch"].to_str().unwrap().parse::<f64>().unwrap();
        assert_eq!(headers["x-cache-status"], "miss");
        assert_eq!(res.text().await.unwrap(), "hello world");

        // HIT on png
        let res = send_vary_req(url, vary_field, "image/png").await;
        assert_eq!(res.status(), StatusCode::OK);
        let headers = res.headers();
        assert_eq!(
            epoch,
            headers["x-epoch"].to_str().unwrap().parse::<f64>().unwrap()
        );
        assert_eq!(headers["x-cache-status"], "hit");
        assert_eq!(res.text().await.unwrap(), "hello world");

        // Vary header ignored -> get png, not jpeg
        let res = send_vary_req(url, vary_field, "image/jpeg").await;
        assert_eq!(res.status(), StatusCode::OK);
        let headers = res.headers();
        assert_eq!(
            epoch,
            headers["x-epoch"].to_str().unwrap().parse::<f64>().unwrap()
        );
        assert_eq!(headers["x-cache-status"], "hit");
    }

    #[tokio::test]
    async fn test_vary_some_ignored() {
        init();
        let url = "http://127.0.0.1:6148/unique/test_vary_some_ignored/vary";
        let vary_header = "Accept, SomeIgnoredVaryHeader";

        // Make a request where we vary on some headers, and provide values for those.
        let res = send_vary_req_with_headers(
            url,
            vary_header,
            vec![
                ("SomeIgnoredVaryHeader", "image/webp"),
                ("Accept", "image/webp"),
            ],
        )
        .await;
        assert_eq!(res.status(), StatusCode::OK);
        let headers = res.headers();
        assert_eq!(headers["x-cache-status"], "miss");
        let epoch1 = headers["x-epoch"].to_str().unwrap().parse::<f64>().unwrap();
        assert_eq!(res.text().await.unwrap(), "hello world");

        // Identical request should yield a HIT.
        let res = send_vary_req_with_headers(
            url,
            vary_header,
            vec![
                ("SomeIgnoredVaryHeader", "image/webp"),
                ("Accept", "image/webp"),
            ],
        )
        .await;
        assert_eq!(res.status(), StatusCode::OK);
        let headers = res.headers();
        assert_eq!(headers["x-cache-status"], "hit");
        assert_eq!(
            epoch1,
            headers["x-epoch"].to_str().unwrap().parse::<f64>().unwrap()
        );
        assert_eq!(res.text().await.unwrap(), "hello world");

        // Hit when changing a header we don't vary on.
        let res = send_vary_req_with_headers(
            url,
            vary_header,
            vec![
                ("SomeIgnoredVaryHeader", "definitely-not-webp"),
                ("Accept", "image/webp"),
            ],
        )
        .await;
        assert_eq!(res.status(), StatusCode::OK);
        let headers = res.headers();
        assert_eq!(headers["x-cache-status"], "hit");
        assert_eq!(
            epoch1,
            headers["x-epoch"].to_str().unwrap().parse::<f64>().unwrap()
        );
        assert_eq!(res.text().await.unwrap(), "hello world");

        // Get a secondary variant by changing Accept.
        let res = send_vary_req_with_headers(
            url,
            vary_header,
            vec![
                ("SomeIgnoredVaryHeader", "definitely-not-webp"),
                ("Accept", "image/jpeg"),
            ],
        )
        .await;
        assert_eq!(res.status(), StatusCode::OK);
        let headers = res.headers();
        assert_eq!(headers["x-cache-status"], "miss");
        let epoch2 = headers["x-epoch"].to_str().unwrap().parse::<f64>().unwrap();
        assert_ne!(epoch1, epoch2);
        assert_eq!(res.text().await.unwrap(), "hello world");

        // Cache hit on secondary variant.
        let res = send_vary_req_with_headers(
            url,
            vary_header,
            vec![
                ("SomeIgnoredVaryHeader", "definitely-not-webp"),
                ("Accept", "image/jpeg"),
            ],
        )
        .await;
        assert_eq!(res.status(), StatusCode::OK);
        let headers = res.headers();
        assert_eq!(headers["x-cache-status"], "hit");
        assert_eq!(
            epoch2,
            headers["x-epoch"].to_str().unwrap().parse::<f64>().unwrap()
        );

        // Cache hit on primary variant.
        let res = send_vary_req_with_headers(
            url,
            vary_header,
            vec![
                ("SomeIgnoredVaryHeader", "definitely-not-webp"),
                ("Accept", "image/webp"),
            ],
        )
        .await;
        assert_eq!(res.status(), StatusCode::OK);
        let headers = res.headers();
        assert_eq!(headers["x-cache-status"], "hit");
        assert_eq!(
            epoch1,
            headers["x-epoch"].to_str().unwrap().parse::<f64>().unwrap()
        );
    }

    #[tokio::test]
    async fn test_vary_dup_header_some_ignored() {
        init();
        let url = "http://127.0.0.1:6148/unique/test_vary_dup_header_some_ignored/vary";
        let first_vary_header = "SomeIgnoredVaryHeader";
        let dup_vary_header = "Accept";

        // Make a request where we vary on some headers, and provide values for those.
        let res = send_vary_req_with_headers_with_dups(
            url,
            first_vary_header,
            vec![
                ("SomeIgnoredVaryHeader", "image/webp"),
                ("Accept", "image/webp"),
            ],
            vec![("set-vary", dup_vary_header)],
        )
        .await;
        assert_eq!(res.status(), StatusCode::OK);
        let headers = res.headers();
        assert_eq!(headers["x-cache-status"], "miss");
        let epoch1 = headers["x-epoch"].to_str().unwrap().parse::<f64>().unwrap();
        assert_eq!(res.text().await.unwrap(), "hello world");

        // Identical request should yield a HIT.
        let res = send_vary_req_with_headers_with_dups(
            url,
            first_vary_header,
            vec![
                ("SomeIgnoredVaryHeader", "image/webp"),
                ("Accept", "image/webp"),
            ],
            vec![("set-vary", dup_vary_header)],
        )
        .await;
        assert_eq!(res.status(), StatusCode::OK);
        let headers = res.headers();
        assert_eq!(headers["x-cache-status"], "hit");
        assert_eq!(
            epoch1,
            headers["x-epoch"].to_str().unwrap().parse::<f64>().unwrap()
        );
        assert_eq!(res.text().await.unwrap(), "hello world");

        // Get a secondary variant by changing Accept.
        let res = send_vary_req_with_headers_with_dups(
            url,
            first_vary_header,
            vec![
                ("SomeIgnoredVaryHeader", "image/webp"),
                ("Accept", "image/jpeg"),
            ],
            vec![("set-vary", dup_vary_header)],
        )
        .await;
        assert_eq!(res.status(), StatusCode::OK);
        let headers = res.headers();
        assert_eq!(headers["x-cache-status"], "miss");
        let epoch2 = headers["x-epoch"].to_str().unwrap().parse::<f64>().unwrap();
        assert_ne!(epoch1, epoch2);
        assert_eq!(res.text().await.unwrap(), "hello world");
    }

    #[tokio::test]
    async fn test_vary_purge() {
        init();
        let url = "http://127.0.0.1:6148/unique/test_vary_purge/vary";
        let vary_field = "Accept";
        let opt1 = "image/png";
        let opt2 = "image/jpeg";

        send_vary_req(url, vary_field, opt1).await;
        let res = send_vary_req(url, vary_field, opt1).await;
        assert_eq!(res.status(), StatusCode::OK);
        let headers = res.headers();
        assert_eq!(headers["x-cache-status"], "hit");

        send_vary_req(url, vary_field, opt2).await;
        let res = send_vary_req(url, vary_field, opt2).await;
        assert_eq!(res.status(), StatusCode::OK);
        let headers = res.headers();
        assert_eq!(headers["x-cache-status"], "hit");

        //both variances are cached

        let res = reqwest::Client::builder()
            .build()
            .unwrap()
            .request(reqwest::Method::from_bytes(b"PURGE").unwrap(), url)
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(res.text().await.unwrap(), "");

        //both should be miss

        let res = send_vary_req(url, vary_field, opt1).await;
        assert_eq!(res.status(), StatusCode::OK);
        let headers = res.headers();
        assert_eq!(headers["x-cache-status"], "miss");

        let res = send_vary_req(url, vary_field, opt2).await;
        assert_eq!(res.status(), StatusCode::OK);
        let headers = res.headers();
        assert_eq!(headers["x-cache-status"], "miss");
    }

    async fn send_max_file_size_req(url: &str, max_file_size_bytes: usize) -> reqwest::Response {
        reqwest::Client::new()
            .get(url)
            .header(
                "x-cache-max-file-size-bytes",
                max_file_size_bytes.to_string(),
            )
            .send()
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn test_cache_max_file_size() {
        init();
        let url = "http://127.0.0.1:6148/unique/test_cache_max_file_size_100/now";

        let res = send_max_file_size_req(url, 100).await;
        assert_eq!(res.status(), StatusCode::OK);
        let headers = res.headers();
        assert_eq!(headers["x-cache-status"], "miss");
        assert_eq!(res.text().await.unwrap(), "hello world");

        let res = send_max_file_size_req(url, 100).await;
        assert_eq!(res.status(), StatusCode::OK);
        let headers = res.headers();
        assert_eq!(headers["x-cache-status"], "hit");
        assert_eq!(res.text().await.unwrap(), "hello world");

        let url = "http://127.0.0.1:6148/unique/test_cache_max_file_size_1/now";
        let res = send_max_file_size_req(url, 1).await;
        assert_eq!(res.status(), StatusCode::OK);
        let headers = res.headers();
        assert_eq!(headers["x-cache-status"], "no-cache");
        assert_eq!(res.text().await.unwrap(), "hello world");

        let res = send_max_file_size_req(url, 1).await;
        assert_eq!(res.status(), StatusCode::OK);
        let headers = res.headers();
        assert_eq!(headers["x-cache-status"], "no-cache");
        assert_eq!(res.text().await.unwrap(), "hello world");
    }

    #[tokio::test]
    async fn test_cache_h2_premature_end() {
        init();
        let url = "http://127.0.0.1:6148/set_content_length/test_cache_h2_premature_end.txt";
        // try to fill cache
        reqwest::Client::new()
            .get(url)
            .header("x-lock", "true")
            .header("x-h2", "true")
            .header("x-set-content-length", "13") // 2 more than "hello world"
            .send()
            .await
            .unwrap();
        // h2 protocol error with content length mismatch

        // did not get saved into cache, next request will be cache miss
        let res = reqwest::Client::new()
            .get(url)
            .header("x-lock", "true")
            .header("x-h2", "true")
            .header("x-set-content-length", "11")
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let headers = res.headers();
        assert_eq!(headers["x-cache-status"], "miss");
        assert_eq!(res.text().await.unwrap(), "hello world");
    }
}
