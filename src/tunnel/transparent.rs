use crate::tunnel::Message;
use crate::tunnel::client::ProxySender;
use crate::utils::get_original_dst;
use anyhow::Result;

use tokio::net::TcpStream;

pub async fn handle_transparent(
    tunnel_id: u32,
    mut inbound: TcpStream,
    sender: ProxySender,
    direct_ctx: crate::tunnel::direct::DirectCtx,
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
    if direct_ctx
        .try_bypass(tunnel_id, &mut inbound, &target_addr, None)
        .await?
    {
        return Ok(());
    }
    let msg = Message::open_tcp_stream(inbound, target_addr, None);
    sender.send(msg).await?;

    Ok(())
}
