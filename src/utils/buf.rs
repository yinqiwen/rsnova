use bytes::{Buf, BytesMut};
use std::cmp::{self};
use std::collections::VecDeque;
use std::io::IoSlice;

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
const MAX_VEC_BUF: usize = 64;
pub struct VBuf {
    cur: usize,
    inner: VecDeque<Vec<u8>>,
    empty: [u8; 0],
}

impl VBuf {
    pub fn new() -> Self {
        Self {
            cur: 0,
            inner: VecDeque::new(),
            empty: [0; 0],
        }
    }
    pub fn vlen(&self) -> usize {
        self.inner.len()
    }
    pub fn push(&mut self, data: Vec<u8>) -> bool {
        if self.inner.len() >= MAX_VEC_BUF || data.is_empty() {
            return false;
        }
        self.inner.push_back(data);
        true
    }
}

impl Buf for VBuf {
    fn remaining(&self) -> usize {
        let sum = self.inner.iter().map(|bytes| bytes.len()).sum::<usize>();
        sum - self.cur
    }

    fn bytes(&self) -> &[u8] {
        if self.inner.is_empty() {
            return &self.empty;
        }
        &self.inner[0][self.cur..]
    }

    fn advance(&mut self, mut cnt: usize) {
        while !self.inner.is_empty() && cnt > 0 {
            if self.cur + cnt >= self.inner[0].len() {
                cnt -= self.inner[0].len() - self.cur;
                self.cur = 0;
                self.inner.pop_front();
            } else {
                self.cur += cnt;
                return;
            }
        }
    }

    #[allow(clippy::needless_range_loop)]
    fn bytes_vectored<'c>(&'c self, dst: &mut [IoSlice<'c>]) -> usize {
        let len = cmp::min(self.inner.len(), dst.len());

        if len > 0 {
            dst[0] = IoSlice::new(self.bytes());
        }

        for i in 1..len {
            dst[i] = IoSlice::new(&self.inner[i]);
        }

        len
    }
}
