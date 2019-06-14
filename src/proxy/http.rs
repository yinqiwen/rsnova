use crate::common::http::*;
use crate::proxy::local::*;

use bytes::BufMut;
use bytes::BytesMut;
use tokio::net::TcpStream;
use tokio::prelude::*;

use tokio_io::{AsyncRead, AsyncWrite};

use url::{Host, HostAndPort, Url};

pub fn handle_http_connection<R, W>(mut ctx: LocalContext, reader: R, writer: W)
where
    R: AsyncRead,
    W: AsyncWrite,
{
    if ctx.is_https {}
}
