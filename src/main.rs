use anyhow::anyhow;
use clap_serde_derive::{
    ClapSerde,
    clap::{self, Parser, ValueEnum},
};
use serde::Deserialize;

use std::fs;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc::Sender;
use url::Url;

mod admin;
mod app_config;
pub mod mux;
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

#[derive(ValueEnum, Clone, Debug, PartialEq, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Role {
    Client,
    Server,
}

/// Outer CLI struct: only handles --config path and forwards the rest
#[derive(Parser)]
#[command(author, version = concat!(env!("CARGO_PKG_VERSION"), " (", env!("CARGO_PKG_AUTHORS"), ")"), about, long_about = None)]
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

    #[default(120)]
    #[arg(long)]
    idle_timeout_secs: usize,

    /// Per-stream flow control window in bytes (TLS protocol only)
    #[default(mux::INITIAL_STREAM_WINDOW)]
    #[arg(long)]
    mux_stream_window: u32,

    /// Max connection lifetime in seconds (0 = no retirement)
    #[default(1800)]
    #[arg(long = "connection-max-age")]
    connection_max_age: u64,

    /// Interval between connection health pings in seconds
    #[default(1)]
    #[arg(long = "ping-interval")]
    ping_interval_secs: u64,

    /// Consecutive ping failures before a connection is retired
    #[default(3)]
    #[arg(long = "ping-fail-threshold")]
    ping_fail_threshold: u32,

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

fn validate_mux_stream_window(window: u32) -> anyhow::Result<()> {
    if !(mux::MIN_STREAM_WINDOW..=mux::MAX_STREAM_WINDOW).contains(&window) {
        return Err(anyhow!(
            "--mux-stream-window must be between {} and {} bytes",
            mux::MIN_STREAM_WINDOW,
            mux::MAX_STREAM_WINDOW
        ));
    }
    Ok(())
}

fn rcgen(tls_host: &String) -> anyhow::Result<()> {
    let cert_path = std::path::PathBuf::from(r"./cert.pem");
    let key_path = std::path::PathBuf::from(r"./key.pem");

    println!(
        "generating self-signed certificate at {:?}  & {:?} with host:{}",
        cert_path, key_path, tls_host,
    );
    let rcgen::CertifiedKey { cert, signing_key } =
        rcgen::generate_simple_self_signed(vec![tls_host.into()])
            .map_err(|e| anyhow!("generate cert failed: {}", e))?;
    let key = signing_key.serialize_pem();
    let cert = cert.pem();

    fs::write(&cert_path, cert).map_err(|e| anyhow!("write cert failed: {}", e))?;
    fs::write(&key_path, key).map_err(|e| anyhow!("write key failed: {}", e))?;
    println!("certificate generated successfully");
    Ok(())
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
            let file_appender = tracing_appender::rolling::RollingFileAppender::builder()
                .rotation(tracing_appender::rolling::Rotation::DAILY)
                .filename_prefix(args.log.as_str())
                .max_log_files(7)
                .build("./")
                .expect("failed to initialize rolling file appender");
            tracing_subscriber::fmt().with_writer(file_appender).init();
        }
    }

    tracing::info!("{args:?}");

    validate_mux_stream_window(args.mux_stream_window)?;

    let tunnel_entries = if !args.tunnel.is_empty() {
        if args.tunnel_client_id.is_empty() {
            return Err(anyhow!(
                "--tunnel-client-id is required when --tunnel is specified"
            ));
        }
        let mut entries = Vec::new();
        for t in &args.tunnel {
            entries.push(tunnel::tunnel_config::parse_tunnel_arg(t)?);
        }
        entries
    } else {
        Vec::new()
    };

    let tunnel_port_ranges = if !args.tunnel_port_range.is_empty() {
        Some(tunnel::tunnel_config::parse_port_range(
            &args.tunnel_port_range,
        )?)
    } else {
        None
    };

    let recorder = utils::MetricsLogRecorder::new();
    let metrics_registry = recorder.get_registry();
    if let Err(e) = metrics::set_global_recorder(recorder) {
        tracing::warn!("set metrics recorder failed: {}", e);
    }

    // Build shared AppConfig for admin server and tunnel client
    let app_config = Arc::new(app_config::AppConfig {
        reloadable: Arc::new(tokio::sync::Mutex::new(app_config::ReloadableConfig {
            tunnel_entries,
            tunnel_client_id: args.tunnel_client_id.clone(),
        })),
        static_args: app_config::StaticArgs {
            listen: args.listen.to_string(),
            role: match args.role {
                Role::Client => "client".to_string(),
                Role::Server => "server".to_string(),
            },
            is_tunnel: !args.tunnel.is_empty(),
            remote: args.remote.as_ref().map(|u| u.to_string()),
            key: args.key.display().to_string(),
            cert: args.cert.display().to_string(),
            tls_host: args.tls_host.clone(),
            concurrent: args.concurrent,
            threads: args.threads,
            idle_timeout_secs: args.idle_timeout_secs,
            max_connections: args.max_connections,
            admin_listen: args.admin_listen.to_string(),
            tproxy: args.tproxy,
        },
        reload_token: Arc::new(tokio::sync::Mutex::new(
            tokio_util::sync::CancellationToken::new(),
        )),
    });

    // Start admin server (always enabled)
    let admin_listen = args.admin_listen;
    let metrics_reg = metrics_registry.clone();
    let admin_config = app_config.clone();
    tokio::spawn(async move {
        if let Err(e) = admin::start_admin_server(&admin_listen, metrics_reg, admin_config).await {
            tracing::error!("Admin server error: {}", e);
        }
    });

    match args.role {
        Role::Client => {
            let has_tunnel = {
                let cfg = app_config.reloadable.lock().await;
                !cfg.tunnel_entries.is_empty()
            };

            if has_tunnel {
                tracing::info!(
                    "Starting in tunnel mode with client_id: {}",
                    args.tunnel_client_id
                );
                tunnel::start_tunnel_client(
                    args.remote.as_ref().unwrap(),
                    &args.cert,
                    &args.tls_host,
                    args.idle_timeout_secs,
                    args.mux_stream_window,
                    app_config,
                    args.connection_max_age,
                    args.concurrent,
                )
                .await?;
                return Ok(());
            }

            let tunnel_sender: Sender<tunnel::Message> =
                match args.remote.as_ref().unwrap().scheme() {
                    "quic" => {
                        tunnel::new_quic_client(
                            args.remote.as_ref().unwrap(),
                            &args.cert,
                            &args.tls_host,
                            args.concurrent,
                            args.idle_timeout_secs,
                            args.connection_max_age,
                            args.ping_interval_secs,
                            args.ping_fail_threshold,
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
                            args.mux_stream_window,
                            args.connection_max_age,
                            args.ping_interval_secs,
                            args.ping_fail_threshold,
                        )
                        .await?
                    }
                    _ => {
                        tracing::error!("unsupported");
                        return Err(anyhow!("unsupported"));
                    }
                };

            // Health checks are now self-managed by each connection's
            // health_loop (see src/tunnel/client.rs). No external ticker
            // needed.

            // Start local tunnel server
            let listen_addr = args.listen;
            let tproxy = args.tproxy;
            let max_connections = args.max_connections;
            tunnel::start_local_tunnel_server(&listen_addr, tunnel_sender, tproxy, max_connections)
                .await?;

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

            let tls_listen = args.listen;
            let quic_listen = args.listen;
            let tls_cert = args.cert.clone();
            let tls_key = args.key.clone();
            let quic_cert = args.cert.clone();
            let quic_key = args.key.clone();
            let idle = args.idle_timeout_secs;
            let stream_window = args.mux_stream_window;
            let tls_registry = registry.clone();

            let tls_handle = tokio::spawn(async move {
                if let Err(e) = tunnel::start_tls_remote_server(
                    &tls_listen,
                    &tls_cert,
                    &tls_key,
                    idle,
                    stream_window,
                    tls_registry,
                )
                .await
                {
                    tracing::error!("TLS server error: {e:?}");
                }
            });

            let quic_handle = tokio::spawn(async move {
                if let Err(e) = tunnel::start_quic_remote_server(
                    &quic_listen,
                    &quic_cert,
                    &quic_key,
                    idle,
                    registry,
                )
                .await
                {
                    tracing::error!("QUIC server error: {e:?}");
                }
            });

            tokio::select! {
                r = tls_handle => r?,
                r = quic_handle => r?,
            }
        }
    }
    Ok(())
}

extern crate cfg_if;
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_mux_stream_window_rejects_out_of_range_values() {
        assert!(validate_mux_stream_window(mux::MIN_STREAM_WINDOW - 1).is_err());
        assert!(validate_mux_stream_window(mux::MIN_STREAM_WINDOW).is_ok());
        assert!(validate_mux_stream_window(mux::MAX_STREAM_WINDOW).is_ok());
        assert!(validate_mux_stream_window(mux::MAX_STREAM_WINDOW + 1).is_err());
    }
}

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
            eprintln!(
                "Warning: config file {:?} not found, using CLI args only",
                config_path
            );
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

    if args.daemon
        && let Err(e) = utils::daemonize(!args.log.is_empty())
    {
        eprintln!("daemonize failed: {}", e);
        std::process::exit(1);
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
            std::process::exit(1);
        }
    });
}
