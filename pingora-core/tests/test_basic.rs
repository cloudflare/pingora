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

#[cfg(all(unix, feature = "any_tls"))]
use hyperlocal::{UnixClientExt, Uri};

#[tokio::test]
async fn test_http() {
    utils::init();
    let res = reqwest::get("http://127.0.0.1:6145").await.unwrap();
    assert_eq!(res.status(), reqwest::StatusCode::OK);
}

#[cfg(feature = "any_tls")]
#[tokio::test]
async fn test_https_http2() {
    utils::init();

    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()
        .unwrap();

    let res = client.get("https://127.0.0.1:6146").send().await.unwrap();
    assert_eq!(res.status(), reqwest::StatusCode::OK);
    assert_eq!(res.version(), reqwest::Version::HTTP_2);

    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .http1_only()
        .build()
        .unwrap();

    let res = client.get("https://127.0.0.1:6146").send().await.unwrap();
    assert_eq!(res.status(), reqwest::StatusCode::OK);
    assert_eq!(res.version(), reqwest::Version::HTTP_11);
}

#[cfg(unix)]
#[cfg(feature = "any_tls")]
#[tokio::test]
async fn test_uds() {
    utils::init();
    let url = Uri::new("/tmp/echo.sock", "/").into();
    let client = hyper::Client::unix();

    let res = client.get(url).await.unwrap();
    assert_eq!(res.status(), reqwest::StatusCode::OK);
}
