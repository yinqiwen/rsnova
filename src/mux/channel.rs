use crate::config::*;
use crate::mux::mux::*;
use crate::mux::tcp::*;
use std::collections::HashMap;
use std::sync::Mutex;

use std::time::Duration;
use tokio::prelude::*;
use tokio::timer::Interval;

use futures::sync::mpsc;
use futures::Stream;

use url::{ParseError, Url};

lazy_static! {
    static ref SESSIONS_HOLDER: Mutex<MuxSessionManager> = Mutex::new(MuxSessionManager::new());
}

#[derive(Clone)]
struct ChannelState {
    //config: ChannelConfig,
    channel: String,
    url: Url,
    conns: u32,
    conns_per_host: u32,
}

struct SessionData {
    channel: String,
    url: String,
    task_sender: mpsc::Sender<SessionTaskClosure>,
}

struct MuxSessionManager {
    all_sessions: Vec<SessionData>,
    cursor: u32,

    channel_states: HashMap<String, ChannelState>,
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

impl MuxSessionManager {
    fn new() -> Self {
        MuxSessionManager {
            all_sessions: Vec::new(),
            cursor: 0,
            channel_states: HashMap::new(),
        }
    }

    fn routine(&mut self) {
        for state in self.channel_states.values() {
            if state.conns < state.conns_per_host {
                let gap = state.conns_per_host - state.conns;
                for n in 0..gap {
                    init_local_mux_connection(&state.channel, &state.url);
                }
            }
        }
        for s in self.all_sessions.iter_mut() {
            if !s.task_sender.is_closed() {
                s.task_sender.start_send(Box::new(ping_session));
            }
        }
    }

    fn add_channel_url(&mut self, key: String, state: ChannelState) {
        info!("add state for key:{}", key);
        self.channel_states.entry(key).or_insert(state);
    }

    fn add_session(
        &mut self,
        channel: &str,
        url: &str,
        task_sender: &mpsc::Sender<SessionTaskClosure>,
    ) -> bool {
        let data = SessionData {
            channel: String::from(channel),
            url: String::from(url),
            task_sender: task_sender.clone(),
        };
        let k = channel_url_key(data.channel.as_str(), data.url.as_str());
        info!("add session for {}", k);
        if let Some(s) = self.channel_states.get_mut(&k) {
            s.conns = s.conns + 1;
            info!("session count {}", s.conns);
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
        if let Some(data) = self.all_sessions.get(idx) {
            if data.task_sender.is_closed() {
                let k = channel_url_key(data.channel.as_str(), data.url.as_str());
                if let Some(s) = self.channel_states.get_mut(&k) {
                    s.conns = s.conns - 1;
                }
                self.all_sessions.remove(idx as usize);
                return None;
            }
            if ch.len() == 0 || data.channel == ch {
                return Some(data.task_sender.clone());
            }
        }
        return None;
    }

    pub fn select_session_by_channel(
        &mut self,
        ch: &str,
    ) -> Option<mpsc::Sender<SessionTaskClosure>> {
        let mut loop_count = 0;
        while loop_count < self.all_sessions.len() {
            if self.all_sessions.len() == 0 {
                return None;
            }
            if let Some(v) = self.do_select_session(ch) {
                return Some(v);
            }
            loop_count = loop_count + 1;
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

fn init_local_mux_connection(channel: &str, url: &Url) {
    match url.scheme() {
        "tcp" => {
            init_local_tcp_channel(channel, url);
        }
        _ => {
            error!("unknown scheme:{}", url.scheme());
        }
    }
}

pub fn init_local_mux_channels(cs: &Vec<ChannelConfig>) {
    for c in cs {
        for u in &c.urls {
            match Url::parse(u.as_str()) {
                Ok(url) => {
                    let state = ChannelState {
                        channel: String::from(c.name.as_str()),
                        url: url,
                        conns: 0,
                        conns_per_host: c.conns_per_host,
                    };
                    let init_state = state.clone();
                    let key = channel_url_key(c.name.as_str(), state.url.as_str());
                    SESSIONS_HOLDER.lock().unwrap().add_channel_url(key, state);

                    for _ in 0..c.conns_per_host {
                        init_local_mux_connection(&init_state.channel, &init_state.url);
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
            return;
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
