use crate::mux::event::*;

pub struct MuxStream {
    pub sid: u32,
    //pub session: S,
}

pub fn handle_event(stream: Option<&MuxStream>, ev: Event) {}
