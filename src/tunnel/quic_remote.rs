use anyhow::Result;
// use quinn::ConnectionError;

use std::{net::SocketAddr, path::PathBuf};

use crate::tunnel::stream::handle_server_stream;

// fn print_type_of<T>(_: &T) {
//     println!("{}", std::any::type_name::<T>())
// }

pub async fn start_quic_remote_server(
    listen: &SocketAddr,
    cert_path: &PathBuf,
    key_path: &PathBuf,
) -> Result<()> {
    let io = s2n_quic::provider::io::tokio::Builder::default()
        .with_receive_address(listen.clone())?
        .build()?;
    let mut server = s2n_quic::Server::builder()
        .with_tls((cert_path.as_path(), key_path.as_path()))?
        .with_io(io)?
        .start()?;

    while let Some(mut connection) = server.accept().await {
        // spawn a new task for the connection
        tracing::info!("QUIC connection incoming");
        tokio::spawn(async move {
            while let Ok(Some(stream)) = connection.accept_bidirectional_stream().await {
                metrics::increment_gauge!("quic_server_proxy_streams", 1.0);
                let (mut recv_stream, mut send_stream) = stream.split();
                tokio::spawn(async move {
                    if let Err(e) = handle_server_stream(&mut recv_stream, &mut send_stream).await {
                        tracing::error!("failed: {reason}", reason = e.to_string());
                    }
                    metrics::decrement_gauge!("quic_server_proxy_streams", 1.0);
                });
            }
        });
    }
    Ok(())
}

// pub async fn start_quic_remote_server(
//     listen: &SocketAddr,
//     cert_path: &PathBuf,
//     key_path: &PathBuf,
// ) -> Result<()> {
//     let key = fs::read(key_path.clone()).context("failed to read private key")?;
//     let key = if key_path.extension().map_or(false, |x| x == "der") {
//         tracing::debug!("private key with DER format");
//         rustls::PrivateKey(key)
//     } else {
//         match rustls_pemfile::read_one(&mut &*key) {
//             Ok(x) => match x.unwrap() {
//                 Item::RSAKey(key) => {
//                     tracing::debug!("private key with PKCS #1 format");
//                     rustls::PrivateKey(key)
//                 }
//                 Item::PKCS8Key(key) => {
//                     tracing::debug!("private key with PKCS #8 format");
//                     rustls::PrivateKey(key)
//                 }
//                 Item::ECKey(key) => {
//                     tracing::debug!("private key with SEC1 format");
//                     rustls::PrivateKey(key)
//                 }
//                 Item::X509Certificate(_) => {
//                     anyhow::bail!("you should provide a key file instead of cert");
//                 }
//                 _ => {
//                     anyhow::bail!("no private keys found");
//                 }
//             },
//             Err(_) => {
//                 anyhow::bail!("malformed private key");
//             }
//         }
//     };

//     let certs = fs::read(cert_path.clone()).context("failed to read certificate chain")?;
//     let certs = if cert_path.extension().map_or(false, |x| x == "der") {
//         vec![rustls::Certificate(certs)]
//     } else {
//         rustls_pemfile::certs(&mut &*certs)
//             .context("invalid PEM-encoded certificate")?
//             .into_iter()
//             .map(rustls::Certificate)
//             .collect()
//     };

//     let mut server_crypto = rustls::ServerConfig::builder()
//         .with_safe_defaults()
//         .with_no_client_auth()
//         .with_single_cert(certs, key)?;
//     server_crypto.alpn_protocols = ALPN_QUIC_HTTP.iter().map(|&x| x.into()).collect();

//     let mut server_config = quinn::ServerConfig::with_crypto(Arc::new(server_crypto));
//     let transport_config = Arc::get_mut(&mut server_config.transport).unwrap();
//     transport_config.max_concurrent_uni_streams(0_u8.into());

//     let endpoint = quinn::Endpoint::server(server_config, listen.clone())?;
//     tracing::info!("QUIC server listening on {}", endpoint.local_addr()?);

//     while let Some(conn) = endpoint.accept().await {
//         tracing::info!("QUIC connection incoming");

//         let fut = handle_quic_connection(conn);
//         tokio::spawn(async move {
//             match fut.await {
//                 Err(e) => {
//                     tracing::error!("connection failed: {reason}", reason = e.to_string())
//                 }
//                 _ => {}
//             }
//         });
//     }

//     Ok(())
// }

// async fn handle_quic_connection(conn: quinn::Connecting) -> Result<()> {
//     let connection = conn.await?;

//     async {
//         // tracing::info!("QUIC connection established");

//         // Each stream initiated by the client constitutes a new request.
//         loop {
//             let stream = connection.accept_bi().await;
//             metrics::increment_gauge!("quic_server_proxy_streams", 1.0);
//             let (mut send_stream, mut recv_stream) = match stream {
//                 Err(quinn::ConnectionError::ApplicationClosed { .. }) => {
//                     tracing::info!("connection closed");
//                     return Ok(());
//                 }
//                 Err(e) => {
//                     return Err(e);
//                 }
//                 Ok(s) => s,
//             };
//             tokio::spawn(async move {
//                 //tracing::info!("handle quic stream");
//                 if let Err(e) = handle_server_stream(&mut recv_stream, &mut send_stream).await {
//                     //print_type_of(&e);
//                     tracing::error!("failed: {reason}", reason = e.to_string());
//                 }
//                 metrics::decrement_gauge!("quic_server_proxy_streams", 1.0);
//             });
//         }
//     }
//     .await?;
//     Ok(())
// }
