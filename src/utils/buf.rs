use bytes::{Buf, BytesMut};

pub fn fill_read_buf(src: &mut BytesMut, dst: &mut [u8]) -> usize {
    if src.is_empty() {
        return 0;
    }
    let mut n = src.len();
    if n > src.len() {
        n = src.len();
    }
    dst[0..n].copy_from_slice(&src[0..n]);
    src.advance(n);
    if src.is_empty() {
        src.clear();
    }
    n
}
