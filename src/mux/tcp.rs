use crate::common::utils::*;
use crate::mux::mux::*;

use tokio::net::{TcpListener, TcpStream};

use tokio::prelude::*;
use tokio::runtime::Runtime;
use tokio_udp::UdpSocket;

use std::sync::{Arc, Mutex};

use url::{ParseError, Url};

pub fn init_local_tcp_channel(channel: &str, url: &Url) {
    let remote = ::common::utils::get_hostport_from_url(url).unwrap();
    let remote_addr = remote.parse().unwrap();
    let channel_str = String::from(channel);
    let url_str = String::from(url.as_str());
    let f = TcpStream::connect(&remote_addr)
        .and_then(move |socket| {
            tokio::spawn(process_client_connection(
                channel_str.as_str(),
                url_str.as_str(),
                socket,
            ));
            Ok(())
        })
        .map_err(|e| error!("tcp connect error:{}", e));
    tokio::spawn(f);
}

pub fn init_remote_tcp_channel(url: &Url) {
    let remote = ::common::utils::get_hostport_from_url(url).unwrap();
    let remote_addr = ::common::utils::get_listen_addr(remote.as_str());
    if let Err(e) = remote_addr {
        error!("invalid listen addr:{} with error:{}", remote, e);
        return;
    }
    info!("Remote server listen on {}", url);
    let r = remote_addr.unwrap();
    let listener = TcpListener::bind(&r);
    match listener {
        Err(e) => {
            error!("Failed to bind on addr:{}", remote);
            return;
        }
        Ok(l) => {
            let server = l
                .incoming()
                .map_err(|e| error!("accept failed = {:?}", e))
                .for_each(|sock| {
                    let handle_conn = process_server_connection(sock);
                    tokio::spawn(handle_conn)
                });
            // Start the Tokio runtime
            tokio::spawn(server);
        }
    }
}
