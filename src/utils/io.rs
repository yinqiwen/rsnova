use bytes::{Buf, BytesMut};
use tokio::io::ReadBuf;

pub fn fill_read_buf(src: &mut BytesMut, dst: &mut ReadBuf<'_>) -> usize {
    if src.is_empty() {
        return 0;
    }
    let mut n = src.len();
    if n > dst.remaining() {
        n = dst.remaining();
    }

    dst.put_slice(&src[0..n]);
    src.advance(n);
    if src.is_empty() {
        src.clear();
    }
    n
}
