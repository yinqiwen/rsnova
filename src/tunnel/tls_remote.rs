// use anyhow::Context;
use anyhow::Result;
use tokio::io::{AsyncRead, AsyncWrite};

use crate::tunnel::stream::handle_server_stream;
use crate::{mux, tunnel::ALPN_QUIC_HTTP};

use std::{collections::VecDeque, net::SocketAddr, path::Path, sync::Arc, sync::Mutex};

use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;

use crate::utils::read_private_key;
use crate::utils::read_tokio_tls_certs;

pub async fn start_tls_remote_server(
    listen: &SocketAddr,
    cert_path: &Path,
    key_path: &Path,
    idle_timeout_secs: usize,
    stream_channel_size: usize,
) -> Result<()> {
    let certs = read_tokio_tls_certs(cert_path)?;
    let key = read_private_key(key_path)?;

    let mut server_crypto = tokio_rustls::rustls::ServerConfig::builder()
        // .with_safe_defaults()
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
            handle_tls_connection(stream, conn_id, idle_timeout_secs, stream_channel_size).await?;
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

pub(crate) async fn handle_tls_connection<T: AsyncRead + AsyncWrite + Unpin + Send + 'static>(
    conn: T,
    id: u32,
    idle_timeout_secs: usize,
    stream_channel_size: usize,
) -> Result<()> {
    let (r, w) = tokio::io::split(conn);
    let mux_conn = mux::Connection::new_with_stream_channel_size(
        r,
        w,
        mux::Mode::Server,
        id,
        stream_channel_size,
    );

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
