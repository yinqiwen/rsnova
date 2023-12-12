use anyhow::Result;
// use quinn::ConnectionError;

use std::{net::SocketAddr, path::Path};

use crate::tunnel::stream::handle_server_stream;

// fn print_type_of<T>(_: &T) {
//     println!("{}", std::any::type_name::<T>())
// }

pub async fn start_quic_remote_server(
    listen: &SocketAddr,
    cert_path: &Path,
    key_path: &Path,
    idle_timeout_secs: usize,
) -> Result<()> {
    let io = s2n_quic::provider::io::tokio::Builder::default()
        .with_receive_address(*listen)?
        .build()?;
    let mut server = s2n_quic::Server::builder()
        .with_tls((cert_path, key_path))?
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
                    if let Err(e) =
                        handle_server_stream(&mut recv_stream, &mut send_stream, idle_timeout_secs)
                            .await
                    {
                        tracing::error!("failed: {reason}", reason = e.to_string());
                    }
                    metrics::decrement_gauge!("quic_server_proxy_streams", 1.0);
                });
            }
        });
    }
    Ok(())
}
