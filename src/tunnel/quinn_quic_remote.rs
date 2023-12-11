use anyhow::Context;
use anyhow::Result;
// use quinn::ConnectionError;

use rustls_pemfile::Item;
use std::sync::Arc;
use std::{net::SocketAddr, path::PathBuf};

use crate::tunnel::stream::handle_server_stream;
use crate::tunnel::ALPN_QUIC_HTTP;
use crate::utils::read_tokio_tls_certs;

// fn print_type_of<T>(_: &T) {
//     println!("{}", std::any::type_name::<T>())
// }

pub async fn start_quic_remote_server(
    listen: &SocketAddr,
    cert_path: &PathBuf,
    key_path: &PathBuf,
) -> Result<()> {
    let key = std::fs::read(key_path.clone()).context("failed to read private key")?;
    let key = if key_path.extension().map_or(false, |x| x == "der") {
        tracing::debug!("private key with DER format");
        tokio_rustls::rustls::PrivateKey(key)
    } else {
        match rustls_pemfile::read_one(&mut &*key) {
            Ok(x) => match x.unwrap() {
                Item::Pkcs1Key(key) => {
                    tracing::debug!("private key with PKCS #1 format");
                    tokio_rustls::rustls::PrivateKey(Vec::from(key.secret_pkcs1_der()))
                }
                Item::Pkcs8Key(key) => {
                    tracing::debug!("private key with PKCS #8 format");
                    tokio_rustls::rustls::PrivateKey(Vec::from(key.secret_pkcs8_der()))
                }
                Item::Sec1Key(key) => {
                    tracing::debug!("private key with SEC1 format");
                    tokio_rustls::rustls::PrivateKey(Vec::from(key.secret_sec1_der()))
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

    let certs = read_tokio_tls_certs(&cert_path)?;

    let mut server_crypto = tokio_rustls::rustls::ServerConfig::builder()
        .with_safe_defaults()
        .with_no_client_auth()
        .with_single_cert(certs, key)?;
    server_crypto.alpn_protocols = ALPN_QUIC_HTTP.iter().map(|&x| x.into()).collect();

    let mut server_config = quinn::ServerConfig::with_crypto(Arc::new(server_crypto));
    let transport_config = Arc::get_mut(&mut server_config.transport).unwrap();
    transport_config.max_concurrent_uni_streams(0_u8.into());

    let endpoint = quinn::Endpoint::server(server_config, listen.clone())?;
    tracing::info!("QUIC server listening on {}", endpoint.local_addr()?);

    while let Some(conn) = endpoint.accept().await {
        tracing::info!("QUIC connection incoming");

        let fut = handle_quic_connection(conn);
        tokio::spawn(async move {
            match fut.await {
                Err(e) => {
                    tracing::error!("connection failed: {reason}", reason = e.to_string())
                }
                _ => {}
            }
        });
    }

    Ok(())
}

async fn handle_quic_connection(conn: quinn::Connecting) -> Result<()> {
    let connection = conn.await?;

    async {
        // tracing::info!("QUIC connection established");

        // Each stream initiated by the client constitutes a new request.
        loop {
            let stream = connection.accept_bi().await;
            metrics::increment_gauge!("quic_server_proxy_streams", 1.0);
            let (mut send_stream, mut recv_stream) = match stream {
                Err(quinn::ConnectionError::ApplicationClosed { .. }) => {
                    tracing::info!("connection closed");
                    return Ok(());
                }
                Err(e) => {
                    return Err(e);
                }
                Ok(s) => s,
            };
            tokio::spawn(async move {
                //tracing::info!("handle quic stream");
                if let Err(e) = handle_server_stream(&mut recv_stream, &mut send_stream).await {
                    //print_type_of(&e);
                    tracing::error!("failed: {reason}", reason = e.to_string());
                }
                metrics::decrement_gauge!("quic_server_proxy_streams", 1.0);
            });
        }
    }
    .await?;
    Ok(())
}
