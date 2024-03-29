#![feature(map_try_insert)]

use anyhow::{anyhow, Result};
use clap::{Parser, ValueEnum};

use metrics::gauge;
use std::fs;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;
use tokio::sync::mpsc::UnboundedSender;
use tokio::{task, time};
use tracing_subscriber;
use url::{ParseError, Url};
use veil::Redact; // 1.3.0

mod mux;
mod tunnel;
mod utils;

#[derive(ValueEnum, Clone, Debug)]
enum Protocol {
    TLS,
    QUIC,
}

#[derive(ValueEnum, Clone, Debug)]
enum Role {
    CLIENT,
    SERVER,
}

#[derive(Parser, Redact)]
#[clap(author, version, about, long_about = None)]
struct Args {
    // #[clap(default_value = "", long, env)]
    // #[redact(partial)]
    // model_id: String,
    #[structopt(long = "listen", default_value = "127.0.0.1:48100")]
    listen: SocketAddr,

    #[clap(long, value_enum, default_value_t=Protocol::TLS)]
    protocol: Protocol,

    #[structopt(long = "remote")]
    remote: Option<Url>,

    #[clap(default_value = "127.0.0.1:48101", long, env)]
    admin: String,

    #[clap(long = "key", requires = "cert", default_value = "key.der")]
    #[redact(partial)]
    key: Option<PathBuf>,
    /// TLS certificate in PEM format
    #[clap(long = "cert", default_value = "cert.der")]
    cert: Option<PathBuf>,

    #[clap(long, value_enum, default_value_t=Role::CLIENT)]
    role: Role,

    #[clap(default_value = "5", long)]
    concurrent: usize,

    #[clap(default_value = "mydomain.io", long)]
    tls_host: String,

    #[clap(default_value = "false", long)]
    rcgen: bool,

    #[clap(default_value = "", long)]
    log: String,
}

// async fn handler() -> Html<&'static str> {
//     Html("<h1>Hello, World!</h1>")
// }

fn rcgen(tls_host: &String) {
    let cert_path = std::path::PathBuf::from(r"./cert.der");
    let key_path = std::path::PathBuf::from(r"./key.der");
    // let cert_der_path = std::path::PathBuf::from(r"./cert.der");

    println!(
        "generating self-signed certificate at {:?}  & {:?} with host:{}",
        cert_path, key_path, tls_host,
    );
    let cert = rcgen::generate_simple_self_signed(vec![tls_host.into()]).unwrap();
    let key = cert.serialize_private_key_der();
    let cert = cert.serialize_der().unwrap();
    // let cert = cert.serialize_pem().unwrap();

    if let Err(e) = fs::write(&cert_path, &cert) {
        println!("failed to write certificate:{}", e);
        return;
    }
    if let Err(e) = fs::write(&key_path, &key) {
        println!("failed to write certificate:{}", e);
        return;
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Args = Args::parse();
    if args.log.is_empty() {
        tracing_subscriber::fmt::init();
    } else {
        let file_appender = tracing_appender::rolling::daily("./", args.log.as_str());
        //let (non_blocking_appender, _guard) = tracing_appender::non_blocking(file_appender);
        tracing_subscriber::fmt().with_writer(file_appender).init();
        tokio::spawn(utils::clean_rotate_logs(format!("./{}", args.log.as_str())));
    }
    tracing::info!("{args:?}");

    if args.rcgen {
        rcgen(&args.tls_host);
        return Ok(());
    }

    let recorder = utils::MetricsLogRecorder::new(Duration::from_secs(10));
    metrics::set_boxed_recorder(Box::new(recorder)).unwrap();

    match args.role {
        Role::CLIENT => {
            let tunnel_sender: UnboundedSender<tunnel::Message>;
            match args.remote.as_ref().unwrap().scheme() {
                "quic" => {
                    tunnel_sender = tunnel::MuxClient::<tunnel::QuicInnerConnection>::from(
                        &args.remote.as_ref().unwrap(),
                        &args.cert.as_ref().unwrap(),
                        &args.tls_host,
                        args.concurrent,
                    )
                    .await?;
                }
                "tls" => {
                    tunnel_sender = tunnel::MuxClient::<tunnel::TlsInnerConnection>::from(
                        &args.remote.as_ref().unwrap(),
                        &args.cert.as_ref().unwrap(),
                        &args.tls_host,
                        args.concurrent,
                    )
                    .await?;
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

            if let Err(e) = tunnel::start_local_tunnel_server(&args.listen, tunnel_sender)
                .await
                .map_err(anyhow::Error::from)
            {
                tracing::error!("{e:?}");
                return Err(e);
            }
        }
        Role::SERVER => match args.protocol {
            Protocol::QUIC => {
                if let Err(e) = tunnel::start_quic_remote_server(
                    &args.listen,
                    args.cert.as_ref().unwrap(),
                    args.key.as_ref().unwrap(),
                )
                .await
                {
                    tracing::error!("{e:?}");
                }
            }
            Protocol::TLS => {
                if let Err(e) = tunnel::start_tls_remote_server(
                    &args.listen,
                    args.cert.as_ref().unwrap(),
                    args.key.as_ref().unwrap(),
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
