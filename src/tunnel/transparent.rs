use crate::tunnel::Message;
use crate::utils::get_original_dst;
use anyhow::Result;

use tokio::net::TcpStream;
use tokio::sync::mpsc;

pub async fn handle_transparent(
    tunnel_id: u32,
    inbound: TcpStream,
    sender: mpsc::UnboundedSender<Message>,
) -> Result<()> {
    let target_addr = get_original_dst(&inbound)?;

    let target_addr = target_addr.to_string();
    tracing::info!(
        "[{}]Handle transparent proxy to {} ",
        tunnel_id,
        target_addr
    );
    let msg = Message::open_stream(inbound, target_addr, None);
    sender.send(msg)?;

    Ok(())
}
