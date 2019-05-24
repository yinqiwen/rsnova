#![feature(integer_atomics)]

use crate::mux::event::*;
use crate::mux::stream::*;
use std::sync::atomic::{AtomicU32, Ordering};
//use std::sync::mpsc;
//use std::sync::mpsc::{Receiver, Sender};

use std::collections::HashMap;

//use tokio::io::write_all;
use tokio_io::{AsyncRead, AsyncWrite};

use futures::Stream;

use tokio::codec::*;

// pub fn write_event<T: AsyncRead + AsyncWrite>(
//     session: Session<T>,
//     frame: Event,
// ) -> Result<(), Box<std::error::Error>> {
//     session.transport.send(frame).wait()?;
//     Ok(())
// }

struct Session {
    streams: HashMap<u32, MuxStream>,
}

impl Session {
    pub fn new_stream(&mut self, id: u32) -> &MuxStream {
        let s = MuxStream { sid: id };
        self.streams.entry(id).or_insert(s)
    }
}

pub fn process_mux_connection<T: AsyncRead + AsyncWrite>(conn: T) {
    let transport = Framed::new(conn, EventCodec::new());
    let mut session = Session {
        streams: HashMap::new(),
    };
    let mut id_seed = AtomicU32::new(0);

    transport.for_each(|ev| match ev.header.flags {
        FLAG_SYN => {
            let s = session.new_stream(ev.header.stream_id);
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
