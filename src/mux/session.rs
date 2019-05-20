#![feature(integer_atomics)]

use crate::mux::event::*;
use crate::mux::stream::*;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc;
use std::sync::mpsc::{Receiver, Sender};

use std::collections::HashMap;

//use tokio::io::write_all;
use tokio_io::{AsyncRead, AsyncWrite};

use futures::Future;
use futures::Sink;
use futures::Stream;

use tokio::codec::*;

pub struct Session<S> {
    stream_id_seed: AtomicU32,
    closed: bool,
    event_channles: (Sender<i32>, Receiver<i32>),
    streams: HashMap<u32, MuxStream>,
    transport: Framed<S, EventCodec>,
}

impl<S: AsyncRead + AsyncWrite> Session<S> {
    pub fn new(item: S) -> Session<S> {
        let s = Session {
            stream_id_seed: AtomicU32::new(0),
            closed: false,
            event_channles: mpsc::channel(),
            streams: HashMap::new(),
            transport: Framed::new(item, EventCodec::new()),
        };

        return s;
    }

    pub fn new_stream(&mut self, sid: u32) -> Option<&MuxStream> {
        let s = MuxStream { sid: sid };
        self.streams.insert(sid, s);
        return self.streams.get(&sid);
    }


    pub fn start(&self) {
        //let framed_sock = Framed::new(self.conn, EventCodec::new());
    }

    pub fn write_event(&mut self, ev: Event) {
        //self.transport.send(ev);
    }


    pub fn open_stream(&mut self) {
        let sid = self.stream_id_seed.fetch_add(1, Ordering::SeqCst);
    }

    pub fn accespt_stream(&mut self) {}

}

pub fn write_event<T: AsyncRead + AsyncWrite>(
    session: Session<T>,
    frame: Event,
) -> Result<(), Box<std::error::Error>> {
    session.transport.send(frame).wait()?;
    Ok(())
}

pub fn handle_events<T: AsyncRead + AsyncWrite>(session: &mut Session<T>) {
    let t = session.transport.by_ref();
    t.for_each(|ev| match ev.header.flags {
        FLAG_SYN => {
            let s = session.new_stream(ev.header.stream_id).unwrap();
            //Ok((stream, ev))
            //handle_event(s, ev);
            Ok(())
        }
        FLAG_FIN => Ok(()),
        FLAG_DATA => Ok(()),
        FLAG_WIN_UPDATE => Ok(()),
        FLAG_PING => Ok(()),
        FLAG_PING_ACK => Ok(()),
        _ => {
            error!("invalid flags:{}", ev.header.flags);
            //Err("invalid flags")
            Ok(())
        }
    });
}
