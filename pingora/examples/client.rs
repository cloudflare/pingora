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

use pingora::connectors::http::Connector;
use pingora::upstreams::peer::HttpPeer;
use pingora_http::RequestHeader;

#[tokio::main]
async fn main() {
    let connector = Connector::new(None);

    let mut peer = HttpPeer::new("1.1.1.1:443", true, "one.one.one.one".into());
    peer.options.set_http_version(2, 1);
    let (mut http, _reused) = connector.get_http_session(&peer).await.unwrap();

    let mut new_request = RequestHeader::build("GET", b"/", None).unwrap();
    new_request
        .insert_header("Host", "one.one.one.one")
        .unwrap();
    http.write_request_header(Box::new(new_request))
        .await
        .unwrap();
    // Servers usually don't respond until the full request body is read.
    http.finish_request_body().await.unwrap();
    http.read_response_header().await.unwrap();
    println!("{:#?}", http.response_header().unwrap());
    // TODO: continue reading the body
    // TODO: return the connection back to the `connector` (or discard it)
}
