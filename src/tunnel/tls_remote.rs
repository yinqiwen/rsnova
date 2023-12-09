use anyhow::{Context, Result};
// use pki_types::{CertificateDer, PrivateKeyDer};
use crate::mux;
use crate::tunnel::stream::handle_server_stream;
use rustls_pemfile::Item;
use std::{collections::VecDeque, fs, io, net::SocketAddr, path::PathBuf, sync::Arc, sync::Mutex};

use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;

pub const ALPN_QUIC_HTTP: &[&[u8]] = &[b"hq-29"];
pub async fn start_tls_remote_server(
    listen: &SocketAddr,
    cert_path: &PathBuf,
    key_path: &PathBuf,
) -> Result<()> {
    let key = fs::read(key_path.clone()).context("failed to read private key")?;
    let key = if key_path.extension().map_or(false, |x| x == "der") {
        tracing::debug!("private key with DER format");
        tokio_rustls::rustls::PrivateKey(key)
    } else {
        match rustls_pemfile::read_one(&mut &*key) {
            Ok(x) => match x.unwrap() {
                Item::RSAKey(key) => {
                    tracing::debug!("private key with PKCS #1 format");
                    tokio_rustls::rustls::PrivateKey(key)
                }
                Item::PKCS8Key(key) => {
                    tracing::debug!("private key with PKCS #8 format");
                    tokio_rustls::rustls::PrivateKey(key)
                }
                Item::ECKey(key) => {
                    tracing::debug!("private key with SEC1 format");
                    tokio_rustls::rustls::PrivateKey(key)
                }
                Item::X509Certificate(_) => {
                    anyhow::bail!("you should provide a key file instead of cert");
                }
                _ => {
                    anyhow::bail!("no private keys found");
                }
            },
            Err(_) => {
                anyhow::bail!("malformed private key");
            }
        }
    };

    let certs = fs::read(cert_path.clone()).context("failed to read certificate chain")?;
    let certs = if cert_path.extension().map_or(false, |x| x == "der") {
        vec![tokio_rustls::rustls::Certificate(certs)]
    } else {
        rustls_pemfile::certs(&mut &*certs)
            .context("invalid PEM-encoded certificate")?
            .into_iter()
            .map(tokio_rustls::rustls::Certificate)
            .collect()
    };

    let mut server_crypto = tokio_rustls::rustls::ServerConfig::builder()
        .with_safe_defaults()
        .with_no_client_auth()
        .with_single_cert(certs, key)?;
    server_crypto.alpn_protocols = ALPN_QUIC_HTTP.iter().map(|&x| x.into()).collect();

    let acceptor = TlsAcceptor::from(Arc::new(server_crypto));
    let listener = TcpListener::bind(listen).await?;
    tracing::info!("TLS server listening on {:?}", listen);

    let mut id: u32 = 0;
    let free_ids = Arc::new(Mutex::new(VecDeque::new()));
    loop {
        let conn_id = if free_ids.lock().unwrap().is_empty() {
            id += 1;
            id - 1
        } else {
            free_ids.lock().unwrap().pop_front().unwrap()
        };
        let (stream, _) = listener.accept().await?;
        let acceptor = acceptor.clone();
        let fut_free_ids = free_ids.clone();
        let fut = async move {
            let stream = acceptor.accept(stream).await?;
            tracing::info!("TLS connection incoming");
            handle_tls_connection(stream, conn_id).await?;
            tracing::info!("TLS connection close");
            fut_free_ids.lock().unwrap().push_back(conn_id);
            Ok(()) as Result<()>
        };

        tokio::spawn(async move {
            if let Err(e) = fut.await {
                tracing::error!("connection failed: {reason}", reason = e.to_string())
            }
        });
    }
}

async fn handle_tls_connection(
    conn: tokio_rustls::server::TlsStream<tokio::net::TcpStream>,
    id: u32,
) -> Result<()> {
    let (r, w) = tokio::io::split(conn);
    let mux_conn = mux::Connection::new(r, w, mux::Mode::Server, id);

    loop {
        let stream = mux_conn.accept_stream().await?;
        metrics::increment_gauge!("tls_server_proxy_streams", 1.0);
        tokio::spawn(async move {
            let stream_id = stream.id();
            let (mut stream_reader, mut stream_writer) = tokio::io::split(stream);
            if let Err(e) = handle_server_stream(&mut stream_reader, &mut stream_writer).await {
                tracing::error!(
                    "[{}/{}]failed: {reason}",
                    id,
                    stream_id,
                    reason = e.to_string()
                );
            }
            metrics::decrement_gauge!("tls_server_proxy_streams", 1.0);
        });
    }
}
