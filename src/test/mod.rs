use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

struct Stream {
    s: *mut Session,
}
struct Session {
    streams: HashMap<u32, Stream>,
}

impl Session {
    pub fn new_stream(&mut self) -> Stream {
        Stream { s: self }
    }

    pub fn write(&mut self) {}
}

impl Stream {
    pub fn write(&mut self) {
        let ref_mut: &mut Session = unsafe { &mut *self.s };
        ref_mut.write();
    }
}
