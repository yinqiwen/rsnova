// #![feature(map_try_insert)]

use anyhow::anyhow;
use clap::{Parser, ValueEnum};

use std::fs;
use std::net::{SocketAddr, UdpSocket};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::RwLock;
use tokio::time;

use url::Url;
use veil::Redact;

mod mux;
mod tunnel;
mod utils;

#[cfg(not(target_arch = "arm"))]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

// #[cfg(target_env = "msvc")]
// #[global_allocator]
// static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

// #[cfg(not(target_env = "msvc"))]
// use tikv_jemallocator::Jemalloc;

// #[cfg(not(target_env = "msvc"))]
// #[global_allocator]
// static GLOBAL: Jemalloc = Jemalloc;

// #[allow(non_upper_case_globals)]
// #[export_name = "malloc_conf"]
// pub static malloc_conf: &[u8] = b"prof:true,prof_active:true,lg_prof_sample:19\0";

#[derive(ValueEnum, Clone, Debug)]
enum Protocol {
    Tls,
    Quic,
}

#[derive(ValueEnum, Clone, Debug, PartialEq)]
enum Role {
    Client,
    Server,
}

#[derive(Parser, Redact)]
#[clap(author, version, about, long_about = None)]
struct Args {
    // #[clap(default_value = "", long, env)]
    // #[redact(partial)]
    // model_id: String,
    #[structopt(long = "listen", default_value = "127.0.0.1:48100")]
    listen: SocketAddr,

    #[clap(long, value_enum, default_value_t=Protocol::Tls)]
    protocol: Protocol,

    #[structopt(long = "remote")]
    remote: Option<Url>,

    #[clap(default_value = "127.0.0.1:48101", long, env)]
    admin: String,

    #[clap(long = "key", requires = "cert", default_value = "key.pem")]
    #[redact(partial)]
    key: Option<PathBuf>,
    /// TLS certificate in PEM format
    #[clap(long = "cert", default_value = "cert.pem")]
    cert: Option<PathBuf>,

    #[clap(long, value_enum, default_value_t=Role::Client)]
    role: Role,

    #[clap(default_value = "5", long)]
    concurrent: usize,

    #[clap(default_value = "2", long)]
    threads: usize,

    #[clap(default_value = "1048576", long)]
    thread_stack_size: usize,

    #[clap(default_value = "30", long)]
    idle_timeout_secs: usize,

    #[clap(default_value = "mydomain.io", long)]
    tls_host: String,

    #[clap(default_value = "false", long)]
    tproxy: bool,

    #[clap(default_value = "false", long)]
    rcgen: bool,

    #[clap(default_value = "false", long)]
    profile: bool,

    #[clap(default_value = "", long)]
    log: String,

    /// PAC file path to serve
    #[clap(long, conflicts_with = "autoproxy_url")]
    pac_file: Option<PathBuf>,

    /// PAC server listen address
    #[clap(long, default_value = "127.0.0.1:48102")]
    pac_listen: SocketAddr,

    /// AutoProxy list URL for auto-generating PAC (e.g., gfwlist)
    #[clap(long, conflicts_with = "pac_file")]
    autoproxy_url: Option<String>,

    /// AutoProxy list update interval in seconds (0 to disable auto-update)
    #[clap(long, default_value = "86400")]
    autoproxy_update_secs: u64,
}

/// 获取本机的出口 IP 地址
fn get_local_ip() -> Option<std::net::IpAddr> {
    // 通过连接外部地址来获取本机使用的出口 IP
    let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("8.8.8.8:80").ok()?;
    socket.local_addr().ok().map(|addr| addr.ip())
}

fn rcgen(tls_host: &String) -> anyhow::Result<()> {
    let cert_path = std::path::PathBuf::from(r"./cert.pem");
    let key_path = std::path::PathBuf::from(r"./key.pem");

    println!(
        "generating self-signed certificate at {:?}  & {:?} with host:{}",
        cert_path, key_path, tls_host,
    );
    let rcgen::CertifiedKey { cert, key_pair } =
        rcgen::generate_simple_self_signed(vec![tls_host.into()])
            .map_err(|e| anyhow!("generate cert failed: {}", e))?;
    let key = key_pair.serialize_pem();
    let cert = cert.pem();

    fs::write(&cert_path, cert).map_err(|e| anyhow!("write cert failed: {}", e))?;
    fs::write(&key_path, key).map_err(|e| anyhow!("write key failed: {}", e))?;
    println!("certificate generated successfully");
    Ok(())
}

async fn start_pac_server(listen: &SocketAddr, pac_content: Arc<RwLock<Vec<u8>>>) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(listen).await?;
    tracing::info!("PAC server listening on {}", listen);

    loop {
        let (mut stream, addr) = listener.accept().await?;
        let pac_content = pac_content.clone();
        tokio::spawn(async move {
            let mut buf = [0u8; 4096];
            // Read HTTP request (we don't need to parse it fully, just drain it)
            if let Err(e) = stream.read(&mut buf).await {
                tracing::warn!("PAC server read error from {}: {}", addr, e);
                return;
            }

            // Read PAC content with lock
            let content = pac_content.read().await;

            // Build HTTP response with PAC content
            let response = format!(
                "HTTP/1.1 200 OK\r\n\
                 Content-Type: application/x-ns-proxy-autoconfig\r\n\
                 Content-Length: {}\r\n\
                 Connection: close\r\n\
                 \r\n",
                content.len()
            );

            if let Err(e) = stream.write_all(response.as_bytes()).await {
                tracing::warn!("PAC server write header error to {}: {}", addr, e);
                return;
            }
            if let Err(e) = stream.write_all(&content).await {
                tracing::warn!("PAC server write body error to {}: {}", addr, e);
                return;
            }
            tracing::debug!("PAC served to {}", addr);
        });
    }
}

async fn watch_pac_file(pac_file: PathBuf, pac_content: Arc<RwLock<Vec<u8>>>) {
    let mut last_modified = fs::metadata(&pac_file)
        .and_then(|m| m.modified())
        .ok();

    let mut interval = time::interval(Duration::from_secs(5));
    loop {
        interval.tick().await;

        let current_modified = match fs::metadata(&pac_file).and_then(|m| m.modified()) {
            Ok(t) => Some(t),
            Err(e) => {
                tracing::warn!("Failed to get PAC file metadata: {}", e);
                continue;
            }
        };

        if current_modified != last_modified {
            match fs::read(&pac_file) {
                Ok(new_content) => {
                    let mut content = pac_content.write().await;
                    *content = new_content;
                    last_modified = current_modified;
                    tracing::info!("PAC file {:?} reloaded", pac_file);
                }
                Err(e) => {
                    tracing::warn!("Failed to reload PAC file {:?}: {}", pac_file, e);
                }
            }
        }
    }
}

async fn watch_autoproxy(
    autoproxy_url: String,
    pac_proxy: String,
    fetch_proxy: Option<String>,
    pac_content: Arc<RwLock<Vec<u8>>>,
    update_secs: u64,
) {
    let mut interval = time::interval(Duration::from_secs(update_secs));
    // Skip the first tick (already loaded at startup)
    interval.tick().await;

    loop {
        interval.tick().await;
        tracing::info!("Updating autoproxy list from {}", autoproxy_url);

        match utils::fetch_and_generate_pac(&autoproxy_url, &pac_proxy, fetch_proxy.as_deref()).await {
            Ok(new_content) => {
                let mut content = pac_content.write().await;
                *content = new_content;
                tracing::info!("AutoProxy PAC updated successfully");
            }
            Err(e) => {
                tracing::warn!("Failed to update autoproxy list: {}", e);
            }
        }
    }
}

async fn service_main(args: &Args) -> anyhow::Result<()> {
    if args.profile {
        tracing_subscriber::fmt::init();
        // console_subscriber::init();
        // tokio::spawn(async {
        //     let app = axum::Router::new()
        //         .route("/debug/pprof/heap", axum::routing::get(profile_get_heap));
        //     // run our app with hyper, listening globally on port 3000
        //     let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await.unwrap();
        //     axum::serve(listener, app).await.unwrap();
        // });
    } else {
        if args.log.is_empty() {
            tracing_subscriber::fmt::init();
        } else {
            let file_appender = tracing_appender::rolling::daily("./", args.log.as_str());
            //let (non_blocking_appender, _guard) = tracing_appender::non_blocking(file_appender);
            tracing_subscriber::fmt().with_writer(file_appender).init();
            tokio::spawn(utils::clean_rotate_logs(format!("./{}", args.log.as_str())));
        }
    }

    tracing::info!("{args:?}");

    let recorder = utils::MetricsLogRecorder::new(Duration::from_secs(10));
    if let Err(e) = metrics::set_boxed_recorder(Box::new(recorder)) {
        tracing::warn!("set metrics recorder failed: {}", e);
    }

    // Start PAC server from local file (no proxy needed)
    if let Some(pac_file) = &args.pac_file {
        let pac_content = fs::read(pac_file)
            .map_err(|e| anyhow!("failed to read PAC file {:?}: {}", pac_file, e))?;
        let pac_content = Arc::new(RwLock::new(pac_content));
        let pac_listen = args.pac_listen;

        // Spawn PAC file watcher for hot reload
        let pac_file_clone = pac_file.clone();
        let pac_content_clone = pac_content.clone();
        tokio::spawn(async move {
            watch_pac_file(pac_file_clone, pac_content_clone).await;
        });

        // Spawn PAC server
        tokio::spawn(async move {
            if let Err(e) = start_pac_server(&pac_listen, pac_content).await {
                tracing::error!("PAC server error: {}", e);
            }
        });
    }

    match args.role {
        Role::Client => {
            let tunnel_sender: UnboundedSender<tunnel::Message> =
                match args.remote.as_ref().unwrap().scheme() {
                    "quic" => {
                        tunnel::new_quic_client(
                            args.remote.as_ref().unwrap(),
                            args.cert.as_ref().unwrap(),
                            &args.tls_host,
                            args.concurrent,
                            args.idle_timeout_secs,
                        )
                        .await?
                    }
                    "tls" => {
                        tunnel::new_tls_client(
                            args.remote.as_ref().unwrap(),
                            args.cert.as_ref().unwrap(),
                            &args.tls_host,
                            args.concurrent,
                            args.idle_timeout_secs,
                        )
                        .await?
                    }
                    _ => {
                        tracing::error!("unsupported");
                        return Err(anyhow!("unsupported"));
                    }
                };

            let health_checker = tunnel_sender.clone();
            tokio::spawn(async move {
                let mut interval = time::interval(Duration::from_secs(1));
                loop {
                    interval.tick().await;
                    if let Err(e) = health_checker.send(tunnel::Message::HealthCheck) {
                        tracing::error!("health check error:{}", e);
                    }
                }
            });

            // Start local tunnel server in background
            let listen_addr = args.listen;
            let tproxy = args.tproxy;
            tokio::spawn(async move {
                if let Err(e) = tunnel::start_local_tunnel_server(&listen_addr, tunnel_sender, tproxy).await {
                    tracing::error!("local tunnel server error: {e:?}");
                }
            });

            // Wait a moment for the proxy to start
            tokio::time::sleep(Duration::from_millis(100)).await;

            // Start PAC server from autoproxy list (fetch through the proxy we just started)
            if let Some(autoproxy_url) = &args.autoproxy_url {
                let proxy_addr = if args.listen.ip().is_unspecified() {
                    let ip = get_local_ip().unwrap_or_else(|| "127.0.0.1".parse().unwrap());
                    format!("{}:{}", ip, args.listen.port())
                } else {
                    args.listen.to_string()
                };
                let pac_proxy = format!("SOCKS5 {}; DIRECT", proxy_addr);
                let fetch_proxy = format!("socks5://{}", proxy_addr);

                match utils::fetch_and_generate_pac(autoproxy_url, &pac_proxy, Some(&fetch_proxy)).await {
                    Ok(pac_content) => {
                        let pac_content = Arc::new(RwLock::new(pac_content));
                        let pac_listen = args.pac_listen;

                        // Spawn autoproxy updater if interval > 0
                        if args.autoproxy_update_secs > 0 {
                            let autoproxy_url = autoproxy_url.clone();
                            let pac_content_clone = pac_content.clone();
                            let update_interval = args.autoproxy_update_secs;
                            let fetch_proxy_clone = fetch_proxy.clone();
                            tokio::spawn(async move {
                                watch_autoproxy(
                                    autoproxy_url,
                                    pac_proxy,
                                    Some(fetch_proxy_clone),
                                    pac_content_clone,
                                    update_interval,
                                )
                                .await;
                            });
                        }

                        // Spawn PAC server
                        tokio::spawn(async move {
                            if let Err(e) = start_pac_server(&pac_listen, pac_content).await {
                                tracing::error!("PAC server error: {}", e);
                            }
                        });
                    }
                    Err(e) => {
                        tracing::error!("Failed to fetch autoproxy list: {}", e);
                    }
                }
            }

            // Keep the main task running
            loop {
                tokio::time::sleep(Duration::from_secs(3600)).await;
            }
        }
        Role::Server => match args.protocol {
            Protocol::Quic => {
                if let Err(e) = tunnel::start_quic_remote_server(
                    &args.listen,
                    args.cert.as_ref().unwrap(),
                    args.key.as_ref().unwrap(),
                    args.idle_timeout_secs,
                )
                .await
                {
                    tracing::error!("{e:?}");
                }
            }
            Protocol::Tls => {
                if let Err(e) = tunnel::start_tls_remote_server(
                    &args.listen,
                    args.cert.as_ref().unwrap(),
                    args.key.as_ref().unwrap(),
                    args.idle_timeout_secs,
                )
                .await
                {
                    tracing::error!("{e:?}");
                }
            }
        },
    }
    Ok(())
}

extern crate cfg_if;
fn main() {
    let args: Args = Args::parse();

    if args.rcgen {
        if let Err(e) = rcgen(&args.tls_host) {
            eprintln!("rcgen failed: {}", e);
        }
        return;
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(args.threads)
        .enable_all()
        .thread_stack_size(args.thread_stack_size)
        .build()
        .expect("failed to build tokio runtime");
    runtime.block_on(async {
        if let Err(e) = service_main(&args).await {
            tracing::error!("service_main error:{e:?}");
        }
    });
}
