use crate::tunnel::client::ProxySender;
use crate::tunnel::Message;
use crate::utils::get_original_dst;
use anyhow::Result;

use tokio::net::TcpStream;

pub async fn handle_transparent(
    tunnel_id: u32,
    inbound: TcpStream,
    sender: ProxySender,
) -> Result<()> {
    let target_addr = match get_original_dst(&inbound) {
        Ok(addr) => addr,
        Err(_) => inbound.local_addr()?, // TPROXY
    };

    let target_addr = target_addr.to_string();
    tracing::info!(
        "[{}]Handle transparent proxy to {} ",
        tunnel_id,
        target_addr
    );
    let msg = Message::open_tcp_stream(inbound, target_addr, None);
    sender.send(msg).await?;

    Ok(())
}
