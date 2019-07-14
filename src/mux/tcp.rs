use super::channel::ChannelState;
use super::multiplex::*;
use crate::common::proxy_connect;
use std::sync::Arc;
use tokio::net::{TcpListener, TcpStream};
use tokio::prelude::*;

use url::Url;

pub fn init_local_tcp_channel(channel: Arc<ChannelState>) {
    let remote = ::common::get_hostport_from_url(&channel.url).unwrap();
    let remote_addr = remote.parse().unwrap();
    let channel_str = String::from(&channel.channel);
    let url_str = String::from(channel.url.as_str());
    let proxy_str = String::from(channel.config.proxy.as_str());

    if proxy_str.is_empty() {
        let f = TcpStream::connect(&remote_addr)
            .and_then(move |socket| {
                process_client_connection(channel_str.as_str(), url_str.as_str(), socket).then(
                    |_| {
                        info!("Server connection closed.");
                        Ok(())
                    },
                )
            })
            .map_err(|e| error!("tcp connect error:{}", e));
        tokio::spawn(f);
    } else {
        let f = proxy_connect(proxy_str.as_str(), remote.as_str())
            .and_then(move |socket| {
                process_client_connection(channel_str.as_str(), url_str.as_str(), socket).then(
                    |_| {
                        info!("Server connection closed.");
                        Ok(())
                    },
                )
            })
            .map_err(|e| error!("tcp proxy connect error:{}", e));
        tokio::spawn(f);
    }
}

pub fn init_remote_tcp_channel(url: &Url) {
    let remote = ::common::get_hostport_from_url(url).unwrap();
    let remote_addr = ::common::get_listen_addr(remote.as_str());
    if let Err(e) = remote_addr {
        error!("invalid listen addr:{} with error:{}", remote, e);
        return;
    }
    info!("Remote server listen on {}", url);
    let r = remote_addr.unwrap();
    let listener = TcpListener::bind(&r);
    match listener {
        Err(e) => {
            error!("Failed to bind on addr:{} with error:{}", remote, e);
        }
        Ok(l) => {
            let server = l
                .incoming()
                .map_err(|e| error!("accept failed = {:?}", e))
                .for_each(|sock| {
                    let handle_conn = process_server_connection(sock).then(|_| {
                        info!("Client connection closed.");
                        Ok(())
                    });
                    tokio::spawn(handle_conn)
                });
            // Start the Tokio runtime
            tokio::spawn(server);
        }
    }
}
