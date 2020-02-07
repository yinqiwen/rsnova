use futures::stream::{SplitSink, SplitStream};
use futures::{Future, Sink, SinkExt, Stream, StreamExt};
use rustls::Session;
use std::error::Error;
use std::pin::Pin;
use std::task::{Context, Poll, Waker};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;

/// This encapsulates the TCP-level connection, some connection
/// state, and the underlying TLS-level session.
struct TlsClient {
    socket: TcpStream,
    closing: bool,
    clean_closure: bool,
    tls_session: rustls::ClientSession,
}

impl AsyncRead for TlsClient {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<std::io::Result<usize>> {
        return Poll::Ready(Ok(0));
    }
}
