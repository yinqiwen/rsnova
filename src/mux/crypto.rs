use bytes::{BufMut, BytesMut};
use tokio::prelude::*;
use tokio_io::io::read_exact;

use ring::aead::*;
use std::io::{Error, ErrorKind};

use crc::{crc32, Hasher32};

use crate::mux::event::*;

pub const METHOD_AES128_GCM: &str = "aes128gcm";
pub const METHOD_CHACHA20_POLY1305: &str = "chacha20poly1305";
pub const METHOD_NONE: &str = "none";

pub struct CryptoContext {
    pub key: String,
    pub encrypt_nonce: u64,
    pub decrypt_nonce: u64,
    pub encrypter: EncryptFunc,
    pub decrypter: DecryptFunc,

    sealing_key: Option<SealingKey>,
    opening_key: Option<OpeningKey>,
}

type DecryptError = (u32, &'static str);

type EncryptFunc = fn(ctx: &CryptoContext, ev: &Event, out: &mut BytesMut);
type DecryptFunc = fn(ctx: &CryptoContext, buf: &mut BytesMut) -> Result<Event, DecryptError>;

impl CryptoContext {
    pub fn new(method: &str, k: &str, nonce: u64) -> Self {
        let mut key = String::from(k);
        while key.len() < 32 {
            key.push('F');
        }
        let aes_key = key.clone();
        match method {
            METHOD_CHACHA20_POLY1305 => CryptoContext {
                encrypt_nonce: nonce,
                decrypt_nonce: nonce,
                encrypter: chacha20poly1305_encrypt_event,
                decrypter: chacha20poly1305_decrypt_event,
                sealing_key: Some(
                    SealingKey::new(&CHACHA20_POLY1305, &key.as_bytes()[0..32]).unwrap(),
                ),
                opening_key: Some(
                    OpeningKey::new(&CHACHA20_POLY1305, &key.as_bytes()[0..32]).unwrap(),
                ),
                key,
            },
            METHOD_NONE => CryptoContext {
                key,
                encrypt_nonce: nonce,
                decrypt_nonce: nonce,
                encrypter: none_encrypt_event,
                decrypter: none_decrypt_event,
                sealing_key: None,
                opening_key: None,
            },
            METHOD_AES128_GCM => CryptoContext {
                key,
                encrypt_nonce: nonce,
                decrypt_nonce: nonce,
                encrypter: aes128gcm_encrypt_event,
                decrypter: aes128gcm_decrypt_event,
                sealing_key: Some(
                    SealingKey::new(&AES_128_GCM, &aes_key.as_bytes()[0..16]).unwrap(),
                ),
                opening_key: Some(
                    OpeningKey::new(&AES_128_GCM, &aes_key.as_bytes()[0..16]).unwrap(),
                ),
            },
            _ => panic!("not supported crypto method."),
        }
    }

    fn get_decrypt_nonce(&self) -> Nonce {
        let mut d = [0u8; NONCE_LEN];
        let v = self.decrypt_nonce.to_le_bytes();
        d[0..8].copy_from_slice(&v[..]);
        Nonce::assume_unique_for_key(d)
    }

    fn get_encrypt_nonce(&self) -> Nonce {
        let mut d = [0u8; NONCE_LEN];
        let v = self.encrypt_nonce.to_le_bytes();
        d[0..8].copy_from_slice(&v[..]);
        Nonce::assume_unique_for_key(d)
    }

    fn skip32_decrypt_key(&self) -> [u8; 10] {
        let mut sk: [u8; 10] = Default::default();
        sk[0..10].copy_from_slice(&self.key.as_bytes()[0..10]);
        let dk = self.decrypt_nonce.to_le_bytes();
        for i in 2..10 {
            sk[i] |= dk[i - 2];
        }
        sk
    }
    fn skip32_encrypt_key(&self) -> [u8; 10] {
        let mut sk: [u8; 10] = Default::default();
        sk[0..10].copy_from_slice(&self.key.as_bytes()[0..10]);
        let dk = self.encrypt_nonce.to_le_bytes();
        for i in 2..10 {
            sk[i] |= dk[i - 2];
        }
        sk
    }
    pub fn encrypt(&mut self, ev: &Event, out: &mut BytesMut) {
        (self.encrypter)(&self, ev, out);
        self.encrypt_nonce += 1;
    }
    pub fn decrypt(&mut self, buf: &mut BytesMut) -> Result<Event, DecryptError> {
        let r = (self.decrypter)(&self, buf);
        if r.is_ok() {
            self.decrypt_nonce += 1;
        }
        r
    }

    pub fn reset(&mut self, nonce: u64) {
        self.decrypt_nonce = nonce;
        self.encrypt_nonce = nonce;
    }
}

pub fn read_encrypt_event<T: AsyncRead>(
    mut ctx: CryptoContext,
    r: T,
) -> impl Future<Item = (CryptoContext, T, Event), Error = std::io::Error> {
    let buf = vec![0; EVENT_HEADER_LEN];
    read_exact(r, buf).and_then(move |(_stream, data)| {
        let mut buf = BytesMut::from(data);
        let r = ctx.decrypt(&mut buf);
        match r {
            Ok(ev) => future::Either::A(future::ok((ctx, _stream, ev))),
            Err((n, reason)) => {
                if !reason.is_empty() {
                    return future::Either::A(future::err(Error::from(
                        ErrorKind::PermissionDenied,
                    )));
                }
                let data_buf = vec![0; n as usize];
                let r = read_exact(_stream, data_buf).and_then(move |(_r, _body)| {
                    buf.reserve(n as usize);
                    buf.put_slice(&_body[..]);
                    // let ev = ctx.decrypt(&mut buf).unwrap();
                    // Ok((ctx, _r, ev))
                    match ctx.decrypt(&mut buf) {
                        Ok(ev) => return Ok((ctx, _r, ev)),
                        Err(e) => return Err(Error::from(ErrorKind::InvalidInput)),
                    }
                });
                future::Either::B(r)
            }
        }
    })
}

pub fn none_encrypt_event(ctx: &CryptoContext, ev: &Event, out: &mut BytesMut) {
    out.reserve(EVENT_HEADER_LEN);
    out.put_u32_le(ev.header.flag_len);
    out.put_u32_le(ev.header.stream_id);

    if !ev.body.is_empty() {
        out.reserve(ev.body.len());
        out.put_slice(&ev.body[..]);
    }
}

pub fn none_decrypt_event(ctx: &CryptoContext, buf: &mut BytesMut) -> Result<Event, DecryptError> {
    if buf.len() < EVENT_HEADER_LEN {
        //println!("decrypt error0:{}", buf.len());
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
    Ok(Event { header, body: out })
}

pub fn chacha20poly1305_encrypt_event(ctx: &CryptoContext, ev: &Event, out: &mut BytesMut) {
    let sk = ctx.skip32_encrypt_key();
    let e1 = skip32::encode(&sk, ev.header.flag_len);
    let e2 = skip32::encode(&sk, ev.header.stream_id);
    out.reserve(EVENT_HEADER_LEN);
    out.put_u32_le(e1);
    out.put_u32_le(e2);

    if !ev.body.is_empty() {
        info!(
            "encrypt ev: {} {} {} {} {} {}",
            ev.header.stream_id,
            ev.header.flags(),
            ev.header.len(),
            ev.body.len(),
            ctx.encrypt_nonce,
            out.len(),
        );
        //let sealing_key = SealingKey::new(&CHACHA20_POLY1305, &key).unwrap();
        let additional_data: [u8; 0] = [];
        let vlen = CHACHA20_POLY1305.tag_len() + ev.body.len() as usize;
        let dlen = EVENT_HEADER_LEN + vlen;

        out.reserve(dlen);
        out.put_slice(&ev.body[..]);
        out.put_slice(&vec![0; CHACHA20_POLY1305.tag_len()]);
        let tlen = out.len();
        let data = &mut out[(tlen - vlen)..tlen];
        match seal_in_place(
            ctx.sealing_key.as_ref().unwrap(),
            ctx.get_encrypt_nonce(),
            Aad::from(&additional_data),
            data,
            CHACHA20_POLY1305.tag_len(),
        ) {
            Ok(_) => {}
            Err(e) => {
                error!("encrypt error:{} {}", e, out.len());
            }
        }
    }
}

pub fn chacha20poly1305_decrypt_event(
    ctx: &CryptoContext,
    buf: &mut BytesMut,
) -> Result<Event, DecryptError> {
    if buf.len() < EVENT_HEADER_LEN {
        return Err((EVENT_HEADER_LEN as u32 - buf.len() as u32, ""));
    }
    //info!("decrypt ev with counter:{}", ctx.decrypt_nonce);
    let sk = ctx.skip32_decrypt_key();
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
        return Ok(Event {
            header,
            body: vec![],
        });
    }
    if buf.len() - EVENT_HEADER_LEN < (header.len() as usize + CHACHA20_POLY1305.tag_len()) {
        return Err((
            header.len() + (EVENT_HEADER_LEN + CHACHA20_POLY1305.tag_len()) as u32
                - buf.len() as u32,
            "",
        ));
    }
    buf.advance(EVENT_HEADER_LEN);
    let dlen = header.len() as usize;
    // let mut out = Vec::with_capacity(dlen);
    // unsafe {
    //     out.set_len(dlen);
    // }
    info!(
        "decrypt event:{} {} {} {} {}",
        header.stream_id,
        header.flags(),
        header.len(),
        buf.len(),
        ctx.decrypt_nonce,
    );
    //let key = chacha20poly1305::SecretKey::from_slice(&ctx.key.as_bytes()[0..32]).unwrap();
    //let xnonce: u128 = ctx.decrypt_nonce as u128;
    // let nonce = chacha20poly1305::Nonce::from_slice(&xnonce.to_le_bytes()[0..12]).unwrap();

    let additional_data: [u8; 0] = [];
    //match chacha20poly1305::open(&key, &nonce, &buf[0..dlen + 16], None, &mut out) {
    match open_in_place(
        ctx.opening_key.as_ref().unwrap(),
        ctx.get_decrypt_nonce(),
        Aad::from(&additional_data),
        0,
        &mut buf[0..(dlen + CHACHA20_POLY1305.tag_len())],
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
                ctx.decrypt_nonce,
            );
            return Err((0, "Decrypt error"));
        }
    }
    let out = Vec::from(&buf[0..dlen]);
    buf.advance(dlen + CHACHA20_POLY1305.tag_len());
    Ok(Event { header, body: out })
}

pub fn aes128gcm_encrypt_event(ctx: &CryptoContext, ev: &Event, out: &mut BytesMut) {
    let sk = ctx.skip32_encrypt_key();
    let e1 = skip32::encode(&sk, ev.header.flag_len);
    let e2 = skip32::encode(&sk, ev.header.stream_id);
    out.reserve(EVENT_HEADER_LEN);
    out.put_u32_le(e1);
    out.put_u32_le(e2);

    if !ev.body.is_empty() {
        //let sealing_key = SealingKey::new(&CHACHA20_POLY1305, &key).unwrap();
        let additional_data: [u8; 0] = [];
        let vlen = AES_128_GCM.tag_len() + ev.body.len() as usize + 4;
        let dlen = EVENT_HEADER_LEN + vlen;
        out.reserve(dlen);
        out.put_slice(&ev.body[..]);
        out.put_slice(&vec![0; AES_128_GCM.tag_len()]);
        let tlen = out.len();
        let data = &mut out[(tlen - vlen)..(tlen - 4)];
        match seal_in_place(
            ctx.sealing_key.as_ref().unwrap(),
            ctx.get_encrypt_nonce(),
            Aad::from(&additional_data),
            data,
            AES_128_GCM.tag_len(),
        ) {
            Ok(_) => {
                let cksm = crc32::checksum_ieee(&out[EVENT_HEADER_LEN..(dlen - 4)]).to_le_bytes();
                out[(tlen - 4)..].copy_from_slice(&cksm);
            }
            Err(e) => {
                error!("encrypt error:{} {}", e, out.len());
            }
        }
    }
}

pub fn aes128gcm_decrypt_event(
    ctx: &CryptoContext,
    buf: &mut BytesMut,
) -> Result<Event, DecryptError> {
    if buf.len() < EVENT_HEADER_LEN {
        return Err((EVENT_HEADER_LEN as u32 - buf.len() as u32, ""));
    }
    //info!("decrypt ev with counter:{}", ctx.decrypt_nonce);
    let sk = ctx.skip32_decrypt_key();
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
        return Ok(Event {
            header,
            body: vec![],
        });
    }
    if buf.len() - EVENT_HEADER_LEN < (header.len() as usize + AES_128_GCM.tag_len() + 4) {
        return Err((
            header.len() + (EVENT_HEADER_LEN + AES_128_GCM.tag_len() + 4) as u32 - buf.len() as u32,
            "",
        ));
    }
    buf.advance(EVENT_HEADER_LEN);
    let dlen = header.len() as usize;
    let klen = dlen + AES_128_GCM.tag_len();
    let crc32 = crc32::checksum_ieee(&buf[0..(klen)]);
    let mut tmpv = [0u8; 4];
    tmpv[..].copy_from_slice(&buf[klen..(klen + 4)]);
    let recv_crc32 = u32::from_le_bytes(tmpv);
    if crc32 != recv_crc32 {
        error!("invalid crc32 {}:{}", crc32, recv_crc32);
    }

    let additional_data: [u8; 0] = [];
    //match chacha20poly1305::open(&key, &nonce, &buf[0..dlen + 16], None, &mut out) {
    match open_in_place(
        ctx.opening_key.as_ref().unwrap(),
        ctx.get_decrypt_nonce(),
        Aad::from(&additional_data),
        0,
        &mut buf[0..klen],
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
                ctx.decrypt_nonce,
            );
            return Err((0, "Decrypt error"));
        }
    }
    let out = Vec::from(&buf[0..dlen]);
    buf.advance(dlen + AES_128_GCM.tag_len() + 4);
    Ok(Event { header, body: out })
}

#[cfg(test)]
mod tests {
    // Note this useful idiom: importing names from outer (for mod tests) scope.
    use super::*;
    use std::str;
    #[test]
    fn test_crypto1() {
        let ev = new_fin_event(100);
        let mut ctx = CryptoContext::new(
            METHOD_CHACHA20_POLY1305,
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
