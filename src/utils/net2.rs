use futures::{AsyncRead, AsyncWrite};
use std::pin::Pin;
use std::task::{Context, Poll};

pub struct AsyncTokioIO<S> {
    s: S,
}

impl<S> AsyncTokioIO<S> {
    pub fn new(s: S) -> Self
    where
        S: futures::AsyncRead + futures::AsyncWrite + Unpin,
    {
        Self { s }
    }
}

impl<S> tokio::io::AsyncRead for AsyncTokioIO<S>
where
    S: futures::AsyncRead + futures::AsyncWrite + Unpin,
{
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<futures::io::Result<usize>> {
        let Self { s } = &mut *self;
        pin_mut!(s);
        s.poll_read(cx, buf)
    }
}

impl<S> tokio::io::AsyncWrite for AsyncTokioIO<S>
where
    S: futures::AsyncRead + futures::AsyncWrite + Unpin,
{
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<futures::io::Result<usize>> {
        let Self { s } = &mut *self;
        pin_mut!(s);
        s.poll_write(cx, buf)
    }
    fn poll_flush(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        let Self { s } = &mut *self;
        pin_mut!(s);
        s.poll_flush(cx)
    }
    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        let Self { s } = &mut *self;
        pin_mut!(s);
        s.poll_close(cx)
    }
}
