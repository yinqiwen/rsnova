use anyhow::anyhow;
use clap_serde_derive::{
    clap::{self, Parser, ValueEnum},
    ClapSerde,
};
use serde::Deserialize;

use std::fs;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc::Sender;
use tokio::time;

use url::Url;

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

#[derive(ValueEnum, Clone, Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Protocol {
    Tls,
    #[cfg(feature = "s2n_quic")]
    Quic,
}

#[derive(ValueEnum, Clone, Debug, PartialEq, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Role {
    Client,
    Server,
}

/// Outer CLI struct: only handles --config path and forwards the rest
#[derive(Parser)]
#[command(author, version, about, long_about = None)]
struct Cli {
    /// Config file path (TOML format)
    #[arg(short, long)]
    config: Option<PathBuf>,

    /// All other arguments (merged with config file)
    #[command(flatten)]
    args: <Args as ClapSerde>::Opt,
}

/// Main configuration (supports both CLI and TOML config file)
#[derive(ClapSerde, Debug)]
struct Args {
    #[default("127.0.0.1:48100".parse::<SocketAddr>().unwrap())]
    #[arg(long)]
    listen: SocketAddr,

    #[default(Protocol::Tls)]
    #[arg(long, value_enum)]
    protocol: Protocol,

    #[arg(long)]
    remote: Option<Url>,

    #[default(PathBuf::from("key.pem"))]
    #[arg(long = "key", requires = "cert")]
    key: PathBuf,

    /// TLS certificate in PEM format
    #[default(PathBuf::from("cert.pem"))]
    #[arg(long = "cert")]
    cert: PathBuf,

    #[default(Role::Client)]
    #[arg(long, value_enum)]
    role: Role,

    #[default(5)]
    #[arg(long)]
    concurrent: usize,

    #[default(2)]
    #[arg(long)]
    threads: usize,

    #[default(1048576)]
    #[arg(long)]
    thread_stack_size: usize,

    #[default(30)]
    #[arg(long)]
    idle_timeout_secs: usize,

    /// Per-stream mux inbound channel size (TLS protocol only)
    #[default(mux::DEFAULT_STREAM_CHANNEL_SIZE)]
    #[arg(long)]
    mux_stream_channel_size: usize,

    #[default("mydomain.io".to_string())]
    #[arg(long)]
    tls_host: String,

    #[default(false)]
    #[arg(long)]
    tproxy: bool,

    #[default(false)]
    #[arg(long)]
    rcgen: bool,

    #[default(false)]
    #[arg(long)]
    profile: bool,

    /// Run in the background (Unix only)
    #[default(false)]
    #[arg(short = 'd', long = "daemon", conflicts_with = "profile")]
    daemon: bool,

    #[default(String::new())]
    #[arg(long)]
    log: String,

    #[default(256)]
    #[arg(long)]
    max_connections: usize,

    /// HTTP server listen address (serves /metrics)
    #[default("127.0.0.1:48102".parse::<SocketAddr>().unwrap())]
    #[arg(long)]
    admin_listen: SocketAddr,

    /// Tunnel entries for NAT traversal (client mode).
    /// Formats: port | localPort:remotePort | host:localPort:remotePort | host:localPort:remotePort:sni
    #[default(Vec::new())]
    #[arg(long = "tunnel", conflicts_with_all = ["listen", "tproxy"])]
    tunnel: Vec<String>,

    /// Client identifier for tunnel mode (required when --tunnel is specified)
    #[default(String::new())]
    #[arg(long = "tunnel-client-id")]
    tunnel_client_id: String,

    /// Server-side allowed port ranges for tunnel (e.g., "8000-9000,10000-10100")
    #[default(String::new())]
    #[arg(long = "tunnel-port-range")]
    tunnel_port_range: String,
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

async fn start_admin_server(
    listen: &SocketAddr,
    metrics_registry: utils::MetricsRegistry,
) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(listen).await?;
    tracing::info!("Admin server listening on {}", listen);

    loop {
        let (mut stream, addr) = listener.accept().await?;
        let registry = metrics_registry.clone();

        tokio::spawn(async move {
            let mut buf = [0u8; 4096];
            let n = match stream.read(&mut buf).await {
                Ok(n) => n,
                Err(e) => {
                    tracing::warn!("Admin server read error from {}: {}", addr, e);
                    return;
                }
            };

            // Parse HTTP request to get the path
            let request = String::from_utf8_lossy(&buf[..n]);
            let path = request
                .lines()
                .next()
                .and_then(|line| line.split_whitespace().nth(1))
                .unwrap_or("/");

            tracing::debug!("Admin server request from {}: {}", addr, path);

            let (status, content_type, body) = match path {
                "/metrics" => {
                    let metrics = utils::format_metrics(&registry);
                    ("200 OK", "text/plain; charset=utf-8", metrics.into_bytes())
                }
                "/" => {
                    let body = "rsnova admin server\n\nEndpoints:\n  /metrics - Server metrics\n";
                    ("200 OK", "text/plain; charset=utf-8", body.as_bytes().to_vec())
                }
                _ => {
                    let body = "404 Not Found\n\nAvailable endpoints:\n  /metrics - Server metrics\n";
                    ("404 Not Found", "text/plain; charset=utf-8", body.as_bytes().to_vec())
                }
            };

            let response = format!(
                "HTTP/1.1 {}\r\n\
                 Content-Type: {}\r\n\
                 Content-Length: {}\r\n\
                 Connection: close\r\n\
                 \r\n",
                status,
                content_type,
                body.len()
            );

            if let Err(e) = stream.write_all(response.as_bytes()).await {
                tracing::warn!("Admin server write header error to {}: {}", addr, e);
                return;
            }
            if let Err(e) = stream.write_all(&body).await {
                tracing::warn!("Admin server write body error to {}: {}", addr, e);
                return;
            }
        });
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

    let tunnel_entries = if !args.tunnel.is_empty() {
        if args.tunnel_client_id.is_empty() {
            return Err(anyhow!("--tunnel-client-id is required when --tunnel is specified"));
        }
        let mut entries = Vec::new();
        for t in &args.tunnel {
            entries.push(tunnel::tunnel_config::parse_tunnel_arg(t)?);
        }
        Some(entries)
    } else {
        None
    };

    let tunnel_port_ranges = if !args.tunnel_port_range.is_empty() {
        Some(tunnel::tunnel_config::parse_port_range(&args.tunnel_port_range)?)
    } else {
        None
    };

    let recorder = utils::MetricsLogRecorder::new();
    let metrics_registry = recorder.get_registry();
    if let Err(e) = metrics::set_boxed_recorder(Box::new(recorder)) {
        tracing::warn!("set metrics recorder failed: {}", e);
    }

    // Start admin server (always enabled)
    let admin_listen = args.admin_listen;
    let registry = metrics_registry.clone();
    tokio::spawn(async move {
        if let Err(e) = start_admin_server(&admin_listen, registry).await {
            tracing::error!("Admin server error: {}", e);
        }
    });

    match args.role {
        Role::Client => {
            if let Some(entries) = tunnel_entries {
                tracing::info!(
                    "Starting in tunnel mode with client_id: {}",
                    args.tunnel_client_id
                );
                tunnel::start_tunnel_client(
                    args.remote.as_ref().unwrap(),
                    &args.cert,
                    &args.tls_host,
                    args.idle_timeout_secs,
                    args.mux_stream_channel_size,
                    &args.tunnel_client_id,
                    entries,
                )
                .await?;
                return Ok(());
            }

            let tunnel_sender: Sender<tunnel::Message> =
                match args.remote.as_ref().unwrap().scheme() {
                    #[cfg(feature = "s2n_quic")]
                    "quic" => {
                        tunnel::new_quic_client(
                            args.remote.as_ref().unwrap(),
                            &args.cert,
                            &args.tls_host,
                            args.concurrent,
                            args.idle_timeout_secs,
                        )
                        .await?
                    }
                    "tls" => {
                        tunnel::new_tls_client(
                            args.remote.as_ref().unwrap(),
                            &args.cert,
                            &args.tls_host,
                            args.concurrent,
                            args.idle_timeout_secs,
                            args.mux_stream_channel_size,
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
                    if let Err(e) = health_checker.send(tunnel::Message::HealthCheck).await {
                        tracing::error!("health check error:{}", e);
                    }
                }
            });

            // Start local tunnel server in background
            let listen_addr = args.listen;
            let tproxy = args.tproxy;
            let max_connections = args.max_connections;
            tokio::spawn(async move {
                if let Err(e) = tunnel::start_local_tunnel_server(&listen_addr, tunnel_sender, tproxy, max_connections).await {
                    tracing::error!("local tunnel server error: {e:?}");
                }
            });

            // Keep the main task running
            loop {
                tokio::time::sleep(Duration::from_secs(3600)).await;
            }
        }
        Role::Server => {
            let registry = if let Some(ranges) = tunnel_port_ranges {
                let listen_port = args.listen.port();
                let admin_port = args.admin_listen.port();
                let reserved = vec![listen_port, admin_port];
                Some(Arc::new(tokio::sync::Mutex::new(
                    tunnel::tunnel_registry::TunnelRegistry::new(ranges, reserved),
                )))
            } else {
                None
            };

            match args.protocol {
                #[cfg(feature = "s2n_quic")]
                Protocol::Quic => {
                    if let Err(e) = tunnel::start_quic_remote_server(
                        &args.listen,
                        &args.cert,
                        &args.key,
                        args.idle_timeout_secs,
                        registry,
                    )
                    .await
                    {
                        tracing::error!("{e:?}");
                    }
                }
                Protocol::Tls => {
                    if let Err(e) = tunnel::start_tls_remote_server(
                        &args.listen,
                        &args.cert,
                        &args.key,
                        args.idle_timeout_secs,
                        args.mux_stream_channel_size,
                        registry,
                    )
                    .await
                    {
                        tracing::error!("{e:?}");
                    }
                }
            }
        }
    }
    Ok(())
}

extern crate cfg_if;
fn main() {
    // Install rustls crypto provider (required for rustls 0.23+)
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("Failed to install rustls crypto provider");

    let mut cli = Cli::parse();

    // Load config file and merge: CLI args > config file > defaults
    let args = if let Some(ref config_path) = cli.config {
        if config_path.exists() {
            let content = fs::read_to_string(config_path)
                .unwrap_or_else(|e| panic!("Failed to read config file {:?}: {}", config_path, e));
            let file_config: <Args as ClapSerde>::Opt = toml::from_str(&content)
                .unwrap_or_else(|e| panic!("Invalid TOML in {:?}: {}", config_path, e));
            Args::from(file_config).merge(&mut cli.args)
        } else {
            eprintln!("Warning: config file {:?} not found, using CLI args only", config_path);
            Args::from(&mut cli.args)
        }
    } else {
        Args::from(&mut cli.args)
    };

    if args.rcgen {
        if let Err(e) = rcgen(&args.tls_host) {
            eprintln!("rcgen failed: {}", e);
        }
        return;
    }

    if args.daemon {
        if let Err(e) = utils::daemonize(!args.log.is_empty()) {
            eprintln!("daemonize failed: {}", e);
            std::process::exit(1);
        }
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
