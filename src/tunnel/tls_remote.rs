// use anyhow::Context;
use anyhow::Result;
// use pki_types::{CertificateDer, PrivateKeyDer};
use crate::tunnel::stream::handle_server_stream;

use crate::{mux, tunnel::ALPN_QUIC_HTTP};
// use pki_types::CertificateDer;
// use pki_types::PrivateKeyDer;

use std::{collections::VecDeque, net::SocketAddr, path::Path, sync::Arc, sync::Mutex};

use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;

use crate::utils::read_pem_private_key;
use crate::utils::read_tokio_tls_certs;

// fn load_certs(path: &std::path::Path) -> io::Result<Vec<CertificateDer<'static>>> {
//     certs(&mut BufReader::new(File::open(path)?)).collect()
// }

pub async fn start_tls_remote_server(
    listen: &SocketAddr,
    cert_path: &Path,
    key_path: &Path,
    idle_timeout_secs: usize,
) -> Result<()> {
    // let key = fs::read(key_path.clone()).context("failed to read private key")?;
    // let key = rsa_private_keys(&mut BufReader::new(File::open(key_path)?))
    //     .next()
    //     .unwrap()
    //     .map(Into::into)?;

    let key = read_pem_private_key(key_path)?;

    let certs = read_tokio_tls_certs(cert_path)?;
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
        let (stream, _) = listener.accept().await?;
        let conn_id = if free_ids.lock().unwrap().is_empty() {
            id += 1;
            id - 1
        } else {
            free_ids.lock().unwrap().pop_front().unwrap()
        };
        let acceptor = acceptor.clone();
        let fut_free_ids = free_ids.clone();
        let fut = async move {
            let stream = acceptor.accept(stream).await?;
            tracing::info!("TLS connection incoming");
            handle_tls_connection(stream, conn_id, idle_timeout_secs).await?;
            Ok(()) as Result<()>
        };

        tokio::spawn(async move {
            if let Err(e) = fut.await {
                tracing::error!("connection failed: {reason}", reason = e.to_string())
            }
            fut_free_ids.lock().unwrap().push_back(conn_id);
        });
    }
}

async fn handle_tls_connection(
    conn: tokio_rustls::server::TlsStream<tokio::net::TcpStream>,
    id: u32,
    idle_timeout_secs: usize,
) -> Result<()> {
    let (r, w) = tokio::io::split(conn);
    let mux_conn = mux::Connection::new(r, w, mux::Mode::Server, id);

    loop {
        let stream = mux_conn.accept_stream().await?;
        metrics::increment_gauge!("tls_server_proxy_streams", 1.0);
        tokio::spawn(async move {
            let stream_id = stream.id();
            let (mut stream_reader, mut stream_writer) = tokio::io::split(stream);
            if let Err(e) =
                handle_server_stream(&mut stream_reader, &mut stream_writer, idle_timeout_secs)
                    .await
            {
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
