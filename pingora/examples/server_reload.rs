use log::{error, info};
use pingora::protocols::TcpKeepalive;
use pingora::server::configuration::Opt;
use pingora::server::Server;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::signal::unix;

mod app;
mod service;

pub fn main() {
    env_logger::init();

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap();

    let args_opt = Opt::parse_args();

    rt.block_on(async move {
        let mut reload_signal = unix::signal(unix::SignalKind::hangup()).unwrap();
        let upgrade = Arc::new(AtomicBool::new(args_opt.upgrade));
        let conf_filename = args_opt.conf;

        loop {
            let conf_filename = conf_filename.clone();
            let upgrade = upgrade.clone();
            let upgrade_for_store = upgrade.clone();
            let task = tokio::spawn(async move {
                let opt = Opt {
                    conf: conf_filename,
                    upgrade: upgrade.load(Ordering::SeqCst),
                    ..Opt::default()
                };
                let opt = Some(opt);
                let mut my_server = Server::new(opt).unwrap();
                my_server.try_bootstrap().unwrap();

                let mut echo_service_http = service::echo::echo_service_http();

                let mut options = pingora::listeners::TcpSocketOptions::default();
                options.tcp_fastopen = Some(10);
                options.tcp_keepalive = Some(TcpKeepalive {
                    idle: Duration::from_secs(60),
                    interval: Duration::from_secs(5),
                    count: 5,
                });

                echo_service_http.add_tcp_with_settings("0.0.0.0:6145", options);
                my_server.add_service(echo_service_http);

                let server_task =
                    tokio::task::spawn_blocking(move || match my_server.run_server(false) {
                        Ok(reload) => {
                            info!("Reload: {}", reload);
                        }
                        Err(e) => {
                            error!("Failed to run server: {}", e);
                        }
                    });
                server_task.await.unwrap();
            });

            tokio::select! {
                _ = reload_signal.recv() => {
                    #[cfg(target_os = "linux")]
                    {
                        upgrade_for_store.store(true, Ordering::SeqCst);
                    }
                    #[cfg(not(target_os = "linux"))]
                    {
                        info!("Upgrade is only supported on Linux");
                    }
                }
                _ = task => {
                    info!("Server task finished");
                    break;
                }
            }
        }
    });
    rt.shutdown_background();
}
