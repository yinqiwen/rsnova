#![feature(integer_atomics)]

use crate::mux::event::*;
use crate::mux::stream::*;
use bytes::BytesMut;
use std::sync::atomic::{AtomicU32, Ordering};
//use std::sync::mpsc::{Receiver, Sender};

use std::collections::HashMap;

//use tokio::io::write_all;
use tokio_io::{AsyncRead, AsyncWrite};

use futures::sink::Send;
use futures::Stream;
use std::io;
use tokio::codec::*;
use tokio::prelude::*;

// pub fn write_event<T: AsyncRead + AsyncWrite>(
//     session: Session<T>,
//     frame: Event,
// ) -> Result<(), Box<std::error::Error>> {
//     session.transport.send(frame).wait()?;
//     Ok(())
// }

// pub struct Session<T: AsyncRead + AsyncWrite> {
//     streams: HashMap<u32, MuxStream<T>>,
//     conn: T,
//     //transport: Framed<T, EventCodec>,
// }

// impl<T: AsyncRead + AsyncWrite> Session<T> {
//     pub fn new(conn: T) -> Self {
//         Self {
//             streams: HashMap::new(),
//             conn: conn,
//             //transport: Framed::new(conn, EventCodec::new()),
//         }
//     }

//     pub fn new_stream(&mut self, id: u32) -> &MuxStream<T> {
//         let s = MuxStream {
//             sid: id,
//             rbuf: BytesMut::new(),
//             session: self,
//         };
//         self.streams.entry(id).or_insert(s)
//     }

//     pub fn connect(&mut self, proto: &str, addr: &str) {}

//     pub fn process_events(&mut self) {
//         self.transport.for_each(|ev| match ev.header.flags {
//             FLAG_SYN => {
//                 let s = self.new_stream(ev.header.stream_id);
//                 Ok(())
//             }
//             FLAG_FIN => Ok(()),
//             FLAG_DATA => Ok(()),
//             FLAG_WIN_UPDATE => Ok(()),
//             FLAG_PING => Ok(()),
//             FLAG_PING_ACK => Ok(()),
//             _ => {
//                 error!("invalid flags:{}", ev.header.flags);
//                 //Err("invalid flags")
//                 Ok(())
//             }
//         });
//     }
// }

pub struct MuxSession<T: AsyncRead + AsyncWrite> {
    streams: HashMap<u32, MuxStream<T>>,
    transport: stream::SplitSink<Framed<T, EventCodec>>,
}

impl<T: AsyncRead + AsyncWrite> MuxSession<T> {
    // pub fn new(conn: T) -> MuxSession<T> {
    //     MuxSession {
    //         streams: HashMap::new(),
    //         transport: Framed::new(conn, EventCodec::new()).split(),
    //     }
    // }
    pub fn new_stream(&mut self, id: u32) -> &MuxStream<T> {
        let s = MuxStream {
            sid: id,
            rbuf: BytesMut::new(),
            session: self,
        };
        self.streams.entry(id).or_insert(s)
    }

    pub fn get_stream(&mut self, id: u32) -> Option<&mut MuxStream<T>> {
        self.streams.get_mut(&id)
    }

    pub fn write_event(&mut self, ev: Event) -> Result<AsyncSink<Event>, io::Error> {
        self.transport.start_send(ev)
    }

    fn handle_syn_event(&mut self, ev: &Event) {
        let s = self.new_stream(ev.header.stream_id);
    }
}

pub fn process_mux_connection<T: AsyncRead + AsyncWrite>(conn: T) -> MuxSession<T> {
    let (mut writer, reader) = Framed::new(conn, EventCodec::new()).split();
    let mut session = MuxSession {
        streams: HashMap::new(),
        transport: writer,
    };
    let mut id_seed = AtomicU32::new(0);

    let f = reader.for_each(|ev| match ev.header.flags {
        FLAG_SYN => {
            session.handle_syn_event(&ev);
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
    return session;
}
