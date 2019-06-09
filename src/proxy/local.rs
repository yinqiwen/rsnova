use bytes::BytesMut;

use futures::try_ready;
use std::str;

use tokio::io::Error as TokioIOError;
use tokio::net::TcpListener;

use std::io::ErrorKind;
use tokio::prelude::*;
use tokio_io::io::copy;

struct PreProcess<R> {
    inner: R,
    buffer: BytesMut,
}

type PeekAddress = (String, BytesMut);

pub fn extract_tls_target(buffer: &mut BytesMut) -> Result<Async<PeekAddress>, TokioIOError> {
    let parser = ::common::client_hello::ClientHelloBuilder::new();
    match parser.parse_bytes(&buffer[..]) {
        Err(error) => match error.kind() {
            ErrorKind::UnexpectedEof => {
                return Ok(Async::NotReady);
            }
            _ => {
                let reason = error.to_string();
                return Err(TokioIOError::new(tokio::io::ErrorKind::Other, reason));
            }
        },
        Ok(client_hello) => {
            for ext in client_hello.extensions {
                // zero indicates the SNI extension
                if ext.extension_type.0 == 0 {
                    let name = str::from_utf8(&ext.extension_data[..]).unwrap();
                    return Ok(Async::Ready((name.to_string(), buffer.take())));
                }
            }
        }
    }
    return Ok(Async::NotReady);
}

pub fn extract_http_target(buffer: &mut BytesMut) -> Result<Async<PeekAddress>, TokioIOError> {
    let mut headers = [httparse::EMPTY_HEADER; 16];
    let mut req = httparse::Request::new(&mut headers);
    match req.parse(&buffer[..]) {
        Err(why) => {
            let reason = why.to_string();
            return Err(TokioIOError::new(tokio::io::ErrorKind::Other, reason));
        }
        Ok(status) => {
            for h in req.headers {
                println!(
                    "{}:{}",
                    h.name,
                    str::from_utf8(&h.value).unwrap().to_string()
                );
                if h.name.to_lowercase() == "Host" {
                    let host_value = str::from_utf8(&h.value).unwrap().to_string();
                    return Ok(Async::Ready((host_value, buffer.take())));
                }
            }
            if status.is_complete() {
                return Err(TokioIOError::new(
                    tokio::io::ErrorKind::Other,
                    "no address found",
                ));
            }
            return Ok(Async::NotReady);
        }
    }
}

impl<R: AsyncRead> Future for PreProcess<R> {
    type Item = PeekAddress;
    type Error = TokioIOError;

    fn poll(&mut self) -> Result<Async<PeekAddress>, TokioIOError> {
        self.buffer.reserve(4096);
        let empty_buf = BytesMut::with_capacity(0);
        let empty_str = String::from("");
        if 0 == try_ready!(self.inner.read_buf(&mut self.buffer)) {
            Ok(Async::Ready((empty_str, empty_buf)))
        } else {
            if self.buffer.len() < 8 {
                return Ok(Async::NotReady);
            }
            if self.buffer[0] == 4 || self.buffer[0] == 5 {
                //socks4/socks5
            }
            let prefix_str = str::from_utf8(&self.buffer.to_vec()[0..3])
                .unwrap()
                .to_uppercase();
            match prefix_str.as_str() {
                "GET" | "PUT" | "POS" | "DEL" | "OPT" | "TRA" | "PAT" | "HEA" => {
                    return extract_http_target(&mut self.buffer)
                }
                _ => Ok(Async::NotReady),
            }
        }
    }
}

pub fn start_local_server(addr: &str) {
    let mut laddr = addr.to_owned();
    if addr.chars().nth(0).unwrap() == ':' {
        laddr = "0.0.0.0".to_owned();
        laddr.push_str(addr);
    }
    info!("Local server listen on {}", laddr);
    let net_addr = laddr.parse().unwrap();
    let listener = TcpListener::bind(&net_addr).expect("unable to bind TCP listener");

    // Pull out a stream of sockets for incoming connections
    let server = listener
        .incoming()
        .map_err(|e| eprintln!("accept failed = {:?}", e))
        .for_each(|sock| {
            // Split up the reading and writing parts of the
            // socket.
            let (local_reader, local_writer) = sock.split();

            //let peek_remote = ;
            // A future that echos the data and returns how
            // many bytes were copied...
            let bytes_copied = copy(local_reader, local_writer);

            // ... after which we'll print what happened.
            let handle_conn = bytes_copied
                .map(|amt| println!("wrote {:?} bytes", amt))
                .map_err(|err| eprintln!("IO error {:?}", err));

            // Spawn the future as a concurrent task.
            tokio::spawn(handle_conn)
        });

    // Start the Tokio runtime
    tokio::spawn(server);
}
