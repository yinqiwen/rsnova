use std::sync::atomic::{AtomicI32, AtomicU32, Ordering};
use std::sync::Mutex;

lazy_static! {
    static ref GLOBAL_STAT: Stat = Stat::new();
}

#[derive(Debug, Default)]
pub struct Stat {
    pub alive_relay_sessions: AtomicI32,
    pub alive_conns: AtomicI32,
    pub retired_conns: AtomicI32,
    pub alive_streams: AtomicI32,
    pub retired_streams: AtomicI32,
    pub relay_id_seed: AtomicU32,
    pub connection_id_seed: AtomicU32,
}

impl Stat {
    fn new() -> Self {
        let s: Stat = Default::default();
        s
    }
}

pub fn inc_alive_relay_sessions() -> i32 {
    GLOBAL_STAT
        .alive_relay_sessions
        .fetch_add(1, Ordering::SeqCst)
}
pub fn dec_alive_relay_sessions() -> i32 {
    GLOBAL_STAT
        .alive_relay_sessions
        .fetch_sub(1, Ordering::SeqCst)
}

pub fn set_alive_conns(c: usize) {
    GLOBAL_STAT.alive_conns.store(c as i32, Ordering::SeqCst)
}

pub fn set_retired_conns(c: usize) {
    GLOBAL_STAT.retired_conns.store(c as i32, Ordering::SeqCst)
}

pub fn set_retired_streams(c: usize) {
    GLOBAL_STAT
        .retired_streams
        .store(c as i32, Ordering::SeqCst)
}
pub fn inc_retired_streams(c: usize) {
    GLOBAL_STAT
        .retired_streams
        .fetch_add(c as i32, Ordering::SeqCst);
}

pub fn set_alive_streams(c: usize) {
    GLOBAL_STAT.alive_streams.store(c as i32, Ordering::SeqCst)
}
pub fn inc_alive_streams(c: usize) {
    GLOBAL_STAT
        .alive_streams
        .fetch_add(c as i32, Ordering::SeqCst);
}

pub fn next_relay_id() -> u32 {
    GLOBAL_STAT.relay_id_seed.fetch_add(1, Ordering::SeqCst)
}
pub fn next_connection_id() -> u32 {
    GLOBAL_STAT
        .connection_id_seed
        .fetch_add(1, Ordering::SeqCst)
}

pub fn dump_stat() {
    info!(
        "===========Period Dump Stat=============
    alive_relay_sessions:{}
    alive_conns:{}
    retired_conns:{}
    alive_streams:{}
    retired_streams:{}\n",
        GLOBAL_STAT.alive_relay_sessions.load(Ordering::SeqCst),
        GLOBAL_STAT.alive_conns.load(Ordering::SeqCst),
        GLOBAL_STAT.retired_conns.load(Ordering::SeqCst),
        GLOBAL_STAT.alive_streams.load(Ordering::SeqCst),
        GLOBAL_STAT.retired_streams.load(Ordering::SeqCst),
    )
}
