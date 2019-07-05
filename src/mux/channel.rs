use crate::config::*;
use crate::mux::mux::*;
use crate::mux::tcp::*;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::sync::Mutex;

use std::time::{Duration, Instant};
use tokio::prelude::*;
use tokio::timer::Interval;

use futures::Stream;
use tokio::sync::mpsc;

use url::Url;

lazy_static! {
    static ref SESSIONS_HOLDER: Mutex<MuxSessionManager> = Mutex::new(MuxSessionManager::new());
}

pub struct ChannelState {
    pub config: ChannelConfig,
    pub channel: String,
    pub url: Url,
    pub conns: AtomicU32,
    //conns_per_host: u32,
}

#[derive(Clone)]
struct SessionData {
    channel: Arc<ChannelState>,
    born_time: Instant,
    max_alive_secs: u32,
    task_sender: mpsc::Sender<SessionTaskClosure>,
}

struct MuxSessionManager {
    all_sessions: Vec<SessionData>,
    retire_sessions: Vec<SessionData>,
    cursor: u32,

    channel_states: HashMap<String, Arc<ChannelState>>,
}

fn channel_url_key(n: &str, u: &str) -> String {
    let mut s = String::from(n);
    s.push('#');
    s.push_str(u);
    s
}

fn ping_session(session: &mut dyn MuxSession) {
    session.ping();
}

fn try_close_session(session: &mut dyn MuxSession) {
    if session.num_of_streams() == 0 {
        info!("Close retired session.");
        session.close();
    }
}

impl MuxSessionManager {
    fn new() -> Self {
        MuxSessionManager {
            all_sessions: Vec::new(),
            retire_sessions: Vec::new(),
            cursor: 0,
            channel_states: HashMap::new(),
        }
    }

    fn routine(&mut self) {
        for state in self.channel_states.values() {
            if state.conns.load(Ordering::SeqCst) < state.config.conns_per_host {
                let gap = state.config.conns_per_host - state.conns.load(Ordering::SeqCst);
                for _ in 0..gap {
                    init_local_mux_connection(state.clone());
                }
            }
        }
        let mut retired = self
            .all_sessions
            .drain_filter(|s| {
                if s.task_sender.poll_ready().is_err() {
                    true
                } else {
                    let _ = s.task_sender.start_send(Box::new(ping_session));
                    if 0 == s.max_alive_secs {
                        return false;
                    }
                    s.born_time.elapsed().as_secs() > u64::from(s.max_alive_secs)
                }
            })
            .collect::<Vec<_>>();
        for s in &retired {
            s.channel.conns.fetch_sub(1, Ordering::SeqCst);
        }

        self.retire_sessions.append(&mut retired);

        self.retire_sessions.drain_filter(|s| {
            if s.task_sender.poll_ready().is_err() {
                true
            } else {
                s.task_sender.start_send(Box::new(try_close_session));
                false
            }
        });
    }

    fn add_channel_url(&mut self, key: String, state: Arc<ChannelState>) {
        info!("add state for key:{}", key);
        self.channel_states.entry(key).or_insert(state);
    }

    fn add_session(
        &mut self,
        channel: &str,
        url: &str,
        task_sender: &mpsc::Sender<SessionTaskClosure>,
    ) -> bool {
        let k = channel_url_key(channel, url);
        info!("add session for {}", k);
        if let Some(s) = self.channel_states.get_mut(&k) {
            let n = s.conns.fetch_add(1, Ordering::SeqCst);
            info!("session count {}", n);
            let data = SessionData {
                channel: s.clone(),
                born_time: Instant::now(),
                max_alive_secs: s.config.max_alive_mins * 60,
                task_sender: task_sender.clone(),
            };
            self.all_sessions.push(data);
            true
        } else {
            error!("No channel config found for {}", k);
            false
        }
    }

    fn do_select_session(&mut self, ch: &str) -> Option<mpsc::Sender<SessionTaskClosure>> {
        let c = self.cursor + 1;
        self.cursor = c;
        let idx = c as usize % self.all_sessions.len();
        if let Some(data) = self.all_sessions.get_mut(idx) {
            if data.task_sender.poll_ready().is_err() {
                let _ = data.channel.conns.fetch_sub(1, Ordering::SeqCst);
                self.all_sessions.remove(idx as usize);
                return None;
            }
            if ch.is_empty() || data.channel.channel == ch {
                return Some(data.task_sender.clone());
            }
        }
        None
    }

    pub fn select_session_by_channel(
        &mut self,
        ch: &str,
    ) -> Option<mpsc::Sender<SessionTaskClosure>> {
        let mut loop_count = 0;
        while loop_count < self.all_sessions.len() {
            if self.all_sessions.is_empty() {
                return None;
            }
            if let Some(v) = self.do_select_session(ch) {
                return Some(v);
            }
            loop_count += 1;
        }
        None
    }
}

pub fn select_session() -> Option<mpsc::Sender<SessionTaskClosure>> {
    SESSIONS_HOLDER
        .lock()
        .unwrap()
        .select_session_by_channel("")
}

#[allow(dead_code)]
pub fn select_session_by_channel(ch: &str) -> Option<mpsc::Sender<SessionTaskClosure>> {
    SESSIONS_HOLDER
        .lock()
        .unwrap()
        .select_session_by_channel(ch)
}

pub fn add_session(
    channel: &str,
    url: &str,
    task_sender: &mpsc::Sender<SessionTaskClosure>,
) -> bool {
    SESSIONS_HOLDER
        .lock()
        .unwrap()
        .add_session(channel, url, task_sender)
}

fn init_local_mux_connection(channel: Arc<ChannelState>) {
    match channel.url.scheme() {
        "tcp" => {
            init_local_tcp_channel(channel);
        }
        _ => {
            error!("unknown scheme:{}", channel.url.scheme());
        }
    }
}

pub fn init_local_mux_channels(cs: &[ChannelConfig]) {
    for c in cs {
        for u in &c.urls {
            match Url::parse(u.as_str()) {
                Ok(url) => {
                    let state = Arc::new(ChannelState {
                        config: c.clone(),
                        channel: String::from(c.name.as_str()),
                        url,
                        conns: AtomicU32::new(0),
                    });
                    //let v = Arc::new(state);
                    //let init_state = state.clone();
                    let key = channel_url_key(c.name.as_str(), state.url.as_str());
                    SESSIONS_HOLDER
                        .lock()
                        .unwrap()
                        .add_channel_url(key, state.clone());

                    for _ in 0..c.conns_per_host {
                        init_local_mux_connection(state.clone());
                    }
                }
                Err(e) => {
                    error!("invalid url:{} for reason:{}", u, e);
                }
            }
        }
    }

    let interval = Interval::new_interval(Duration::from_secs(3));
    let routine = interval
        .for_each(|_| {
            SESSIONS_HOLDER.lock().unwrap().routine();
            Ok(())
        })
        .map_err(|e| {
            error!("routine task error:{}", e);
        });
    tokio::spawn(routine);
}

pub fn init_remote_mux_server(url: &String) {
    let remote_url = Url::parse(url.as_str());
    match remote_url {
        Err(e) => {
            error!("invalid remote url:{} with error:{}", url, e);
        }
        Ok(u) => match u.scheme() {
            "tcp" => {
                init_remote_tcp_channel(&u);
            }
            _ => {
                error!("unknown scheme:{}", u.scheme());
            }
        },
    }
}
