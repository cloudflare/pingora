use std::{io::Error, thread, time::Duration};

use futures_util::{SinkExt, StreamExt};
use log::debug;
use once_cell::sync::Lazy;
use tokio::{
    net::{TcpListener, TcpStream},
    runtime::Builder,
};

pub static WS_ECHO: Lazy<bool> = Lazy::new(init);

fn init() -> bool {
    thread::spawn(move || {
        let runtime = Builder::new_current_thread()
            .thread_name("websocket echo")
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async move {
            server("127.0.0.1:9283").await.unwrap();
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
