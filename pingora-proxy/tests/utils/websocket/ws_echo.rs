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

use std::{io::Error, thread, time::Duration};

use futures_util::{SinkExt, StreamExt};
use log::debug;
use std::sync::LazyLock;
use tokio::{
    net::{TcpListener, TcpStream},
    runtime::Builder,
};

pub static WS_ECHO: LazyLock<bool> = LazyLock::new(init);
pub const WS_ECHO_ORIGIN_PORT: u16 = 9283;

fn init() -> bool {
    thread::spawn(move || {
        let runtime = Builder::new_current_thread()
            .thread_name("websocket echo")
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async move {
            server(&format!("127.0.0.1:{WS_ECHO_ORIGIN_PORT}"))
                .await
                .unwrap();
        })
    });
    thread::sleep(Duration::from_millis(200));
    true
}

async fn server(addr: &str) -> Result<(), Error> {
    let listener = TcpListener::bind(&addr).await.unwrap();
    while let Ok((stream, _)) = listener.accept().await {
        tokio::spawn(handle_connection(stream));
    }
    Ok(())
}

async fn handle_connection(stream: TcpStream) {
    let mut ws_stream = tokio_tungstenite::accept_async(stream).await.unwrap();

    while let Some(msg) = ws_stream.next().await {
        let msg = msg.unwrap();
        let echo = msg.clone();
        if msg.is_text() {
            let data = msg.into_text().unwrap();
            if data.contains("close") {
                // abruptly close the stream without WS close;
                debug!("abrupt close");
                return;
            } else if data.contains("graceful") {
                debug!("graceful close");
                ws_stream.close(None).await.unwrap();
                // close() only sends frame
                return;
            } else {
                ws_stream.send(echo).await.unwrap();
            }
        }
    }
}
