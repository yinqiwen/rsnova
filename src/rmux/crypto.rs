use bytes::{Buf, BufMut, BytesMut};
//use tokio::io::read_exact;
use tokio::prelude::*;

use ring::aead::*;

use super::event::*;

pub const METHOD_AES128_GCM: &str = "aes128gcm";
pub const METHOD_CHACHA20_POLY1305: &str = "chacha20poly1305";
pub const METHOD_NONE: &str = "none";

struct CryptoNonceSequence {
    nonce: u64,
}

impl CryptoNonceSequence {
    fn new(nonce: u64) -> Self {
        Self { nonce }
    }
}

impl NonceSequence for CryptoNonceSequence {
    fn advance(&mut self) -> Result<Nonce, ring::error::Unspecified> {
        self.nonce += 1;
        let mut d = [0u8; NONCE_LEN];
        let v = self.nonce.to_le_bytes();
        d[0..8].copy_from_slice(&v[..]);
        Ok(Nonce::assume_unique_for_key(d))
    }
}

pub struct CryptoContext {
    pub key: String,
    pub nonce: u64,
    sealing_key: Option<SealingKey<CryptoNonceSequence>>,
    opening_key: Option<OpeningKey<CryptoNonceSequence>>,
}

type DecryptError = (u32, &'static str);

// type EncryptFunc = fn(ctx: &CryptoContext, ev: &Event, out: &mut BytesMut);
// type DecryptFunc = fn(ctx: &CryptoContext, buf: &mut BytesMut) -> Result<Event, DecryptError>;

fn make_key<K: BoundKey<CryptoNonceSequence>>(
    algorithm: &'static Algorithm,
    key: &[u8],
    nonce: u64,
) -> K {
    let key = UnboundKey::new(algorithm, key).unwrap();
    let nonce_sequence = CryptoNonceSequence::new(nonce);
    K::new(key, nonce_sequence)
}

impl CryptoContext {
    pub fn new(method: &str, k: &str, nonce: u64) -> Self {
        let mut key = String::from(k);
        while key.len() < 32 {
            key.push('F');
        }
        let aes_key = key.clone();
        match method {
            METHOD_CHACHA20_POLY1305 => CryptoContext {
                nonce,
                sealing_key: Some(make_key(
                    &CHACHA20_POLY1305,
                    &aes_key.as_bytes()[0..32],
                    nonce,
                )),
                opening_key: Some(make_key(
                    &CHACHA20_POLY1305,
                    &aes_key.as_bytes()[0..32],
                    nonce,
                )),
                key,
            },
            METHOD_NONE => CryptoContext {
                key,
                nonce,
                sealing_key: None,
                opening_key: None,
            },
            METHOD_AES128_GCM => CryptoContext {
                key,
                nonce,
                sealing_key: Some(make_key(&AES_128_GCM, &aes_key.as_bytes()[0..16], nonce)),
                opening_key: Some(make_key(&AES_128_GCM, &aes_key.as_bytes()[0..16], nonce)),
            },
            _ => panic!("not supported crypto method."),
        }
    }

    // fn get_decrypt_nonce(&self) -> Nonce {
    //     let mut d = [0u8; NONCE_LEN];
    //     let v = self.nonce.to_le_bytes();
    //     d[0..8].copy_from_slice(&v[..]);
    //     Nonce::assume_unique_for_key(d)
    // }

    // fn get_encrypt_nonce(&self) -> Nonce {
    //     let mut d = [0u8; NONCE_LEN];
    //     let v = self.nonce.to_le_bytes();
    //     d[0..8].copy_from_slice(&v[..]);
    //     Nonce::assume_unique_for_key(d)
    // }

    fn skip32_decrypt_key(&self) -> [u8; 10] {
        let mut sk: [u8; 10] = Default::default();
        sk[0..10].copy_from_slice(&self.key.as_bytes()[0..10]);
        let dk = self.nonce.to_le_bytes();
        for i in 2..10 {
            sk[i] |= dk[i - 2];
        }
        sk
    }
    fn skip32_encrypt_key(&self) -> [u8; 10] {
        //let mut key = [0; 32];
        let mut sk: [u8; 10] = Default::default();
        sk[0..10].copy_from_slice(&self.key.as_bytes()[0..10]);
        let dk = self.nonce.to_le_bytes();
        for i in 2..10 {
            sk[i] |= dk[i - 2];
        }
        sk
    }
    pub fn encrypt(&mut self, ev: &mut Event, out: &mut BytesMut) {
        if self.sealing_key.is_none() {
            out.reserve(EVENT_HEADER_LEN + ev.body.len());
            out.put_u32_le(ev.header.flag_len);
            out.put_u32_le(ev.header.stream_id);
            if !ev.body.is_empty() {
                out.put_slice(&ev.body[..]);
            }
        } else {
            let sk = self.skip32_encrypt_key();
            let e1 = skip32::encode(&sk, ev.header.flag_len);
            let e2 = skip32::encode(&sk, ev.header.stream_id);
            out.reserve(EVENT_HEADER_LEN);
            out.put_u32_le(e1);
            out.put_u32_le(e2);
            if !ev.body.is_empty() {
                match self
                    .sealing_key
                    .as_mut()
                    .unwrap()
                    .seal_in_place_append_tag(Aad::empty(), &mut ev.body)
                {
                    Ok(_) => {}
                    Err(e) => {
                        error!("encrypt error:{} {}", e, out.len());
                    }
                }
            }
            out.put_slice(&ev.body[..]);
            //warn!("[{}]send bytes {}", self.nonce, out.len());
            self.nonce += 1;
        }
        //self.nonce += 1;
    }
    pub fn decrypt(&mut self, buf: &mut BytesMut) -> Result<Event, DecryptError> {
        if self.opening_key.is_none() {
            if buf.len() < EVENT_HEADER_LEN {
                return Err((EVENT_HEADER_LEN as u32 - buf.len() as u32, ""));
            }
            let mut xbuf: [u8; 4] = Default::default();
            xbuf.copy_from_slice(&buf[0..4]);
            let e1 = u32::from_le_bytes(xbuf);
            xbuf.copy_from_slice(&buf[4..8]);
            let e2 = u32::from_le_bytes(xbuf);
            let header = Header {
                flag_len: e1,
                stream_id: e2,
            };
            let flags = header.flags();
            if (FLAG_WIN_UPDATE == flags) || 0 == header.len() {
                buf.advance(EVENT_HEADER_LEN);
                return Ok(Event {
                    header,
                    body: vec![],
                    remote: true,
                });
            }
            if buf.len() - EVENT_HEADER_LEN < header.len() as usize {
                return Err((
                    header.len() + EVENT_HEADER_LEN as u32 - buf.len() as u32,
                    "",
                ));
            }
            buf.advance(EVENT_HEADER_LEN);
            let dlen = header.len() as usize;
            let mut out = Vec::with_capacity(dlen);
            out.put_slice(&buf[0..dlen]);
            buf.advance(dlen);
            Ok(Event {
                header,
                body: out,
                remote: true,
            })
        } else {
            if buf.len() < EVENT_HEADER_LEN {
                return Err((EVENT_HEADER_LEN as u32 - buf.len() as u32, ""));
            }
            //info!("decrypt ev with counter:{}", ctx.decrypt_nonce);
            let sk = self.skip32_decrypt_key();
            let mut xbuf: [u8; 4] = Default::default();
            xbuf.copy_from_slice(&buf[0..4]);
            let e1 = skip32::decode(&sk, u32::from_le_bytes(xbuf));
            xbuf.copy_from_slice(&buf[4..8]);
            let e2 = skip32::decode(&sk, u32::from_le_bytes(xbuf));
            let header = Header {
                flag_len: e1,
                stream_id: e2,
            };
            let flags = header.flags();
            if (FLAG_WIN_UPDATE == flags) || 0 == header.len() {
                buf.advance(EVENT_HEADER_LEN);
                self.nonce += 1;
                return Ok(Event {
                    header,
                    body: vec![],
                    remote: true,
                });
            }
            let opening_key = self.opening_key.as_mut().unwrap();
            let expected_len = header.len() as usize + opening_key.algorithm().tag_len();
            // error!(
            //     "[{}]expected len:{} {} {} {} {}",
            //     self.nonce,
            //     buf.len(),
            //     header.len(),
            //     expected_len,
            //     opening_key.algorithm().tag_len(),
            //     buf.len() - EVENT_HEADER_LEN < expected_len
            // );
            if buf.len() - EVENT_HEADER_LEN < expected_len {
                let missing = expected_len + EVENT_HEADER_LEN - buf.len();
                return Err((missing as u32, ""));
            }
            buf.advance(EVENT_HEADER_LEN);
            let dlen = header.len() as usize;
            // info!(
            //     "decrypt event:{} {} {} {} {}",
            //     header.stream_id,
            //     header.flags(),
            //     header.len(),
            //     buf.len(),
            //     ctx.decrypt_nonce,
            // );
            //let key = chacha20poly1305::SecretKey::from_slice(&ctx.key.as_bytes()[0..32]).unwrap();
            //let xnonce: u128 = ctx.decrypt_nonce as u128;
            // let nonce = chacha20poly1305::Nonce::from_slice(&xnonce.to_le_bytes()[0..12]).unwrap();
            //let additional_data: [u8; 0] = [];
            //match chacha20poly1305::open(&key, &nonce, &buf[0..dlen + 16], None, &mut out) {
            match opening_key.open_in_place(
                Aad::empty(),
                &mut buf[0..(dlen + opening_key.algorithm().tag_len())],
            ) {
                Ok(_) => {}
                Err(e) => {
                    error!(
                        "decrypt error:{} for event:{} {} {} {} {}",
                        e,
                        header.stream_id,
                        header.flags(),
                        header.len(),
                        buf.len(),
                        self.nonce,
                    );
                    return Err((0, "Decrypt error"));
                }
            }
            let out = Vec::from(&buf[0..dlen]);
            buf.advance(dlen + opening_key.algorithm().tag_len());
            self.nonce += 1;
            Ok(Event {
                header,
                body: out,
                remote: true,
            })
        }
    }
}

pub async fn read_encrypt_event<'a, T>(
    ctx: &'a mut CryptoContext,
    reader: &'a mut T,
    recv_buf: &mut BytesMut,
) -> Result<Option<Event>, std::io::Error>
where
    T: AsyncRead + Unpin + ?Sized,
{
    let mut next_read_n: u32 = 0;
    loop {
        if recv_buf.is_empty() {
            recv_buf.clear();
        }
        if next_read_n > 0 && next_read_n < 4096 {
            next_read_n = 4096;
        }
        if next_read_n > 0 {
            recv_buf.reserve(next_read_n as usize);
            let pos = recv_buf.len();
            let cap = recv_buf.capacity();
            unsafe {
                recv_buf.set_len(cap);
            }
            // info!(
            //     "Start read at pos:{}, available buf:{}",
            //     pos,
            //     recv_buf[pos..].len()
            // );
            let n = reader.read(&mut recv_buf[pos..]).await?;
            if 0 == n {
                return Ok(None);
            }
            unsafe {
                recv_buf.set_len(pos + n);
            }
        }
        let r = ctx.decrypt(recv_buf);
        match r {
            Ok(ev) => return Ok(Some(ev)),
            Err((n, reason)) => {
                if !reason.is_empty() {
                    return Err(std::io::Error::new(std::io::ErrorKind::Other, reason));
                }
                next_read_n = n;
            }
        }
    }
}

pub async fn write_encrypt_event<'a, T>(
    ctx: &'a mut CryptoContext,
    writer: &'a mut T,
    mut ev: Event,
) -> Result<(), std::io::Error>
where
    T: AsyncWrite + Unpin + ?Sized,
{
    let mut buf = BytesMut::new();
    ctx.encrypt(&mut ev, &mut buf);
    let evbuf = buf.to_vec();
    writer.write_all(&evbuf[..]).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    // Note this useful idiom: importing names from outer (for mod tests) scope.
    use super::*;
    use std::str;
    #[test]
    fn test_crypto1() {
        let ev = new_fin_event(100);
        let mut encrypt_ctx = CryptoContext::new(
            METHOD_CHACHA20_POLY1305,
            "21321321321321312321321321212asdfasdasdas1",
            21321312,
        );
        let mut decrypt_ctx = encrypt_ctx.clone();
        let mut buf = BytesMut::new();
        encrypt_ctx.encrypt(&ev, &mut buf);
        println!("encoded buf len:{} {}", buf.capacity(), buf.len());

        let r = decrypt_ctx.decrypt(&mut buf).unwrap();
        assert_eq!(r.header.stream_id, 100);
        //assert_eq!(r.header.flags(), FLAG_FIN);
        assert_eq!(r.header.len(), 0);
        assert_eq!(buf.len(), 0);
    }
    #[test]
    fn test_crypto2() {
        let s = "hello,world";
        let ev = new_data_event(100, s.as_bytes());
        let mut ctx = CryptoContext::new(
            METHOD_CHACHA20_POLY1305,
            "21321321321321312321321321212asdfasdasdas1",
            21321312,
        );
        let mut buf = BytesMut::new();
        ctx.encrypt(&ev, &mut buf);
        println!(
            "encoded buf len:{} {} {} {}",
            buf.capacity(),
            buf.len(),
            ev.header.flag_len,
            ev.header.stream_id
        );

        let r = ctx.decrypt(&mut buf).unwrap();
        println!(
            "decode event len:{} {}",
            r.header.flag_len, r.header.stream_id
        );
        assert_eq!(r.header.stream_id, 100);
        assert_eq!(r.header.flags(), FLAG_DATA);
        assert_eq!(buf.len(), 0);
        assert_eq!(str::from_utf8(&r.body[..]).unwrap(), s);
    }

    #[test]
    fn test_crypto3() {
        let ev = new_fin_event(100);
        let mut ctx = CryptoContext::new(
            "none",
            "21321321321321312321321321212asdfasdasdas1",
            21321312,
        );
        let mut buf = BytesMut::new();
        ctx.encrypt(&ev, &mut buf);
        println!("encoded buf len:{} {}", buf.capacity(), buf.len());

        let r = ctx.decrypt(&mut buf).unwrap();
        assert_eq!(r.header.stream_id, 100);
        //assert_eq!(r.header.flags(), FLAG_FIN);
        assert_eq!(r.header.len(), 0);
        assert_eq!(buf.len(), 0);
    }
    #[test]
    fn test_crypto4() {
        let s = "hello,world";
        let ev = new_data_event(100, s.as_bytes());
        let mut ctx = CryptoContext::new(
            "none",
            "21321321321321312321321321212asdfasdasdas1",
            21321312,
        );
        let mut buf = BytesMut::new();
        ctx.encrypt(&ev, &mut buf);
        println!(
            "encoded buf len:{} {} {} {}",
            buf.capacity(),
            buf.len(),
            ev.header.flag_len,
            ev.header.stream_id
        );

        let r = ctx.decrypt(&mut buf).unwrap();
        println!(
            "decode event len:{} {}",
            r.header.flag_len, r.header.stream_id
        );
        assert_eq!(r.header.stream_id, 100);
        assert_eq!(r.header.flags(), FLAG_DATA);
        assert_eq!(buf.len(), 0);
        assert_eq!(str::from_utf8(&r.body[..]).unwrap(), s);
    }

    #[test]
    fn test_crypto5() {
        let s = "hello,world";
        let ev = new_data_event(100, s.as_bytes());
        let mut ctx = CryptoContext::new(
            "aes128gcm",
            "21321321321321312321321321212asdfasdasdas1",
            21321312,
        );
        let mut buf = BytesMut::new();
        ctx.encrypt(&ev, &mut buf);
        println!(
            "encoded buf len:{} {} {} {}",
            buf.capacity(),
            buf.len(),
            ev.header.flag_len,
            ev.header.stream_id
        );

        let r = ctx.decrypt(&mut buf).unwrap();
        println!(
            "decode event len:{} {}",
            r.header.flag_len, r.header.stream_id
        );
        assert_eq!(r.header.stream_id, 100);
        assert_eq!(r.header.flags(), FLAG_DATA);
        assert_eq!(buf.len(), 0);
        assert_eq!(str::from_utf8(&r.body[..]).unwrap(), s);
    }
}
