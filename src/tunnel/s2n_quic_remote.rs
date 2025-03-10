use anyhow::anyhow;
use anyhow::Result;
use tokio::sync::mpsc;
use url::Url;

use std::path::PathBuf;
use std::{net::SocketAddr, path::Path};

use crate::tunnel::{client::MuxClientTrait, stream::handle_server_stream};

use super::client::mux_client_loop;
use super::{
    client::{MuxClient, MuxConnection},
    s2n_quic_client::S2NQuicConnection,
    Message,
};

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

pub struct S2NReverseQuicConnection {
    pub(crate) inner: Option<s2n_quic::Connection>,
}
impl MuxConnection for S2NReverseQuicConnection {
    type SendStream = s2n_quic::stream::SendStream;
    type RecvStream = s2n_quic::stream::ReceiveStream;
    fn is_valid(&self) -> bool {
        self.inner.is_some()
    }
    async fn ping(&mut self) -> anyhow::Result<()> {
        match &mut self.inner {
            None => Err(anyhow!("null connection")),
            Some(c) => {
                if let Err(e) = c.ping() {
                    c.close(s2n_quic::application::Error::UNKNOWN);
                    self.inner = None;
                    tracing::info!("ping fail:{}", e);
                    Err(e.into())
                } else {
                    Ok(())
                }
            }
        }
    }
    async fn connect(&mut self, _url: &Url, _key_path: &Path, host: &str) -> anyhow::Result<()> {
        Err(anyhow!("unsupported connection"))
    }
    async fn open_stream(&mut self) -> anyhow::Result<(Self::SendStream, Self::RecvStream)> {
        match &mut self.inner {
            None => Err(anyhow!("null connection")),
            Some(c) => match c.open_bidirectional_stream().await {
                Err(e) => {
                    c.close(s2n_quic::application::Error::UNKNOWN);
                    self.inner = None;
                    tracing::info!("open stream fail:{}", e);
                    Err(e.into())
                }
                Ok(stream) => {
                    let (r, s) = stream.split();
                    Ok((s, r))
                }
            },
        }
    }

    fn set_connection(&mut self, new_c: Self) {
        *self = new_c;
    }
}
