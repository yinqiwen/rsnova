use std::{net::SocketAddr, num::NonZeroUsize};

use super::Message;
use super::client::ProxySender;
use crate::utils::{
    LinuxTproxyUdpSocket, MAXIMUM_UDP_PAYLOAD_SIZE, UdpServerStream, new_udp_listener,
};
use bytes::Bytes;
use lru::LruCache;
use tokio::sync::mpsc;

pub struct UdpAssociateManager {
    sessions: LruCache<SocketAddr, mpsc::Sender<Bytes>>,
}

impl UdpAssociateManager {
    pub fn new() -> Self {
        Self {
            sessions: LruCache::new(NonZeroUsize::new(64).unwrap()),
        }
    }
    pub fn get(
        &mut self,
        addr: SocketAddr,
    ) -> (mpsc::Sender<Bytes>, Option<mpsc::Receiver<Bytes>>) {
        if let Some(sender) = self.sessions.get(&addr) {
            (sender.clone(), None)
        } else {
            let (sender, receiver) = mpsc::channel::<Bytes>(2);
            if let Some((_, old_sender)) = self.sessions.push(addr, sender.clone()) {
                let _ = old_sender.try_send(Bytes::new());
            }
            (sender, Some(receiver))
        }
    }
}

pub(crate) fn start_local_udp_tunnel_server(
    addr: &SocketAddr,
    msg_sender: ProxySender,
) -> Result<(), std::io::Error> {
    let udp_socket = new_udp_listener(addr, true)?;
    let tproxy_udp_server = LinuxTproxyUdpSocket::new(udp_socket)?;
    let mut pkt_buf = [0u8; MAXIMUM_UDP_PAYLOAD_SIZE];
    let (tunnel_data_sender, mut tunnel_data_receiver) = mpsc::channel::<(Bytes, SocketAddr)>(4);
    let mut manager = UdpAssociateManager::new();

    tokio::spawn(async move {
        loop {
            tokio::select! {
                recv_result = tproxy_udp_server.recv_dest_from(&mut pkt_buf) => {
                    let (recv_len, src, dst) = match recv_result {
                        Ok(o) => o,
                        Err(err) => {
                            tracing::error!("recv_dest_from failed with err: {}", err);
                            continue;
                        }
                    };
                    // Packet length is limited by MAXIMUM_UDP_PAYLOAD_SIZE, excess bytes will be discarded.
                    // Copy bytes, because udp_associate runs in another tokio Task
                    let pkt = &pkt_buf[..recv_len];
                    tracing::debug!(
                        "received UDP packet from {}, destination {}, length {} bytes",
                        src,
                        dst,
                        recv_len
                    );

                    if recv_len == 0 {
                        continue;
                    }
                    let data = Bytes::copy_from_slice(pkt);
                    let (sender, receiver) = manager.get(src);
                    if let Some(rx) = receiver{
                        let stream = UdpServerStream::new(rx, tunnel_data_sender.clone(), src);
                        let msg = Message::open_udp_stream(stream, dst.to_string(), None);
                        // Forward to the mux client loop for tunneling. If the
                        // proxy channel is full or closed, drop this datagram
                        // rather than blocking the local UDP listener.
                        if let Err(e) = msg_sender.try_send(msg) {
                            tracing::warn!(
                                "drop UDP visitor from {}: proxy channel send failed: {}",
                                src,
                                e
                            );
                        }
                    }else{
                        let _ = sender.send(data).await;
                    }

                }
                // Reply datagrams from the remote end: forward back to the
                // visitor's source address via the bound tproxy socket. UDP
                // allows concurrent recv+send on the same socket.
                to_write_back = tunnel_data_receiver.recv()=>{
                    if let Some((data, addr)) = to_write_back
                        && let Err(e) = tproxy_udp_server.socket().send_to(&data, addr).await
                    {
                        tracing::warn!(
                            "send_to visitor {} failed: {}",
                            addr,
                            e
                        );
                    }
                }

            }
        }
    });
    Ok(())
}
