use super::crypto::{read_encrypt_event, CryptoContext};
use super::event::{
    get_event_type_str, new_ping_event, new_pong_event, new_shutdown_event, new_syn_event,
    new_window_update_event, Event, FLAG_DATA, FLAG_FIN, FLAG_PING, FLAG_PONG, FLAG_SHUTDOWN,
    FLAG_SYN, FLAG_WIN_UPDATE,
};
use super::message::ConnectRequest;
use super::stream::MuxStream;
use crate::channel::get_channel_stream;
use crate::channel::ChannelStream;
use crate::tunnel::relay;
use crate::utils::make_io_error;
use bytes::BytesMut;
use futures::future::join3;
use futures::FutureExt;
use rand::Rng;
use std::collections::HashMap;
use std::error::Error;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::time;

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

lazy_static! {
    static ref CHANNEL_SESSIONS: Mutex<ChannelSessionManager> =
        Mutex::new(ChannelSessionManager::new());
}

struct ChannelSessionManager {
    channels: HashMap<String, ChannelMuxSession>,
    retired: Vec<MuxSession>,
}

impl ChannelSessionManager {
    fn new() -> Self {
        Self {
            channels: HashMap::new(),
            retired: Vec::new(),
        }
    }
}

struct ChannelMuxSession {
    //session_ids: HashMap<u32, usize>,
    sessions: Vec<Option<MuxSession>>,
    cursor: AtomicU32,
}

pub struct MuxSessionState {
    last_ping_send_time: AtomicU32,
    last_pong_recv_time: AtomicU32,
    pub born_time: Instant,
    retired: AtomicBool,
    io_active_unix_secs: AtomicU32,
    closed: AtomicBool,
}

impl MuxSessionState {
    fn ping_pong_gap(&self) -> i64 {
        let t1 = self.last_ping_send_time.load(Ordering::SeqCst);
        let t2 = self.last_pong_recv_time.load(Ordering::SeqCst);
        if t1 > 0 && t2 > 0 {
            return t2 as i64 - t1 as i64;
        }
        0
    }
    fn is_retired(&self) -> bool {
        self.retired.load(Ordering::SeqCst)
    }
    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }
    fn get_io_idle_secs(&self, now_unix_secs: u32) -> u32 {
        let secs = self.io_active_unix_secs.load(Ordering::SeqCst);
        if secs == 0 {
            return 0;
        }
        now_unix_secs - secs
    }
}

pub struct MuxSession {
    id: u32,
    event_tx: mpsc::Sender<Event>,
    pendding_streams: Vec<MuxStream>,
    stream_id_seed: AtomicU32,
    state: Arc<MuxSessionState>,
    max_alive_secs: u64,
    //streams: HashMap<u32, MuxStream>,
}

fn store_mux_session(channel: &str, session: MuxSession) {
    let cmap = &mut CHANNEL_SESSIONS.lock().unwrap().channels;
    //info!("{}0 store cmap size:{}", channel, cmap.len());
    if cmap.get_mut(channel).is_none() {
        let csession = ChannelMuxSession {
            //session_ids: HashMap::new(),
            sessions: Vec::new(),
            cursor: AtomicU32::new(0),
        };
        cmap.insert(String::from(channel), csession);
    }
    //info!("{}1 store cmap size:{}", channel, cmap.len());
    if let Some(csession) = cmap.get_mut(channel) {
        for s in csession.sessions.iter_mut() {
            if s.is_none() {
                *s = Some(session);
                //csession.session_ids.insert(sid, idx);
                return;
            }
        }
        csession.sessions.push(Some(session));
    }
}

fn erase_mux_session(channel: &str, sid: u32) {
    let mut holder = CHANNEL_SESSIONS.lock().unwrap();
    let cmap = &mut holder.channels;
    if let Some(csession) = cmap.get_mut(channel) {
        for s in csession.sessions.iter_mut() {
            if let Some(ss) = s {
                if ss.id == sid {
                    let _ = s.take();
                    return;
                }
            }
        }
    }
    for i in 0..holder.retired.len() {
        if holder.retired[i].id == sid {
            holder.retired.remove(i);
            return;
        }
    }
}

fn hanle_pendding_mux_streams(channel: &str, sid: u32, streams: &mut HashMap<u32, MuxStream>) {
    let cmap = &mut CHANNEL_SESSIONS.lock().unwrap().channels;
    if let Some(csession) = cmap.get_mut(channel) {
        for cs in csession.sessions.iter_mut() {
            if let Some(ss) = cs {
                if ss.id == sid {
                    loop {
                        if let Some(s) = ss.pendding_streams.pop() {
                            streams.insert(s.id(), s);
                        } else {
                            return;
                        }
                    }
                }
            }
        }
    }
}

pub fn get_channel_session_size(channel: &str) -> usize {
    let cmap = &mut CHANNEL_SESSIONS.lock().unwrap().channels;
    //info!("{}1 cmap size:{}", channel, cmap.len());
    let mut len: usize = 0;
    if let Some(csession) = cmap.get_mut(channel) {
        for s in csession.sessions.iter() {
            if let Some(_ss) = s {
                len += 1;
            }
        }
        return len;
    }
    0
}

struct RoutineAction {
    ev: Option<Event>,
    sender: mpsc::Sender<Event>,
}

impl RoutineAction {
    fn new(ev: Event, sender: mpsc::Sender<Event>) -> Self {
        Self {
            ev: Some(ev),
            sender,
        }
    }
}

pub async fn routine_all_sessions() {
    let mut actions = Vec::new();
    {
        let mut holder = CHANNEL_SESSIONS.lock().unwrap();
        let cmap = &mut holder.channels;
        let mut retired = Vec::new();
        for (channel, csession) in cmap.iter_mut() {
            if channel.is_empty() {
                continue;
            }
            for session in csession.sessions.iter_mut() {
                if let Some(s) = session {
                    if s.state.ping_pong_gap() < -60 {
                        error!("[{}]Session heartbeat timeout.", s.id);
                        let shutdown = new_shutdown_event(0, false);
                        actions.push(RoutineAction::new(shutdown, s.event_tx.clone()));
                        continue;
                    } else {
                        let ping = new_ping_event(0, false);
                        actions.push(RoutineAction::new(ping, s.event_tx.clone()));
                        if s.max_alive_secs > 0 {
                            let rand_inc: i64 = {
                                let mut rng = rand::thread_rng();
                                rng.gen_range(-60, 60)
                            };
                            //let session_id = s.id;
                            let cmp_secs = s.max_alive_secs as i64 + rand_inc;
                            if s.state.born_time.elapsed().as_secs() > cmp_secs as u64 {
                                s.state.retired.store(true, Ordering::SeqCst);
                                retired.push(session.take().unwrap());
                                //csession.session_ids.remove(&session_id);
                            }
                        }
                    }
                }
            }
        }
        holder.retired.append(&mut retired);
        for s in holder.retired.iter_mut() {
            let ping = new_ping_event(0, false);
            actions.push(RoutineAction::new(ping, s.event_tx.clone()));
        }
    }
    for action in actions.iter_mut() {
        let ev = action.ev.take().unwrap();
        let _ = action.sender.send(ev).await;
    }
}

pub async fn create_stream(
    channel: &str,
    proto: &str,
    addr: &str,
) -> Result<MuxStream, std::io::Error> {
    let (stream, ev, ev_sender) = {
        let mut stream: Option<MuxStream> = None;
        let mut ev: Option<Event> = None;
        let mut ev_sender: Option<mpsc::Sender<Event>> = None;

        let cmap = &mut CHANNEL_SESSIONS.lock().unwrap().channels;
        //let mut cmap: HashMap<String, ChannelMuxSession> = HashMap::new();
        if let Some(csession) = cmap.get_mut(channel) {
            for _ in 0..csession.sessions.len() {
                let mut idx = csession.cursor.fetch_add(1, Ordering::SeqCst);
                idx %= csession.sessions.len() as u32;
                if let Some(session) = &mut csession.sessions.as_mut_slice()[idx as usize] {
                    let creq = ConnectRequest {
                        proto: String::from(proto),
                        addr: String::from(addr),
                    };
                    let cev =
                        new_syn_event(session.stream_id_seed.fetch_add(2, Ordering::SeqCst), &creq);
                    let pendding_stream = MuxStream::new(
                        channel,
                        session.id,
                        cev.header.stream_id,
                        session.event_tx.clone(),
                        creq,
                    );
                    session.pendding_streams.push(pendding_stream.clone());
                    stream = Some(pendding_stream);
                    ev = Some(cev);
                    ev_sender = Some(session.event_tx.clone());
                    break;
                }
            }
        }
        (stream, ev, ev_sender)
    };
    if stream.is_some() {
        let _ = ev_sender.unwrap().send(ev.unwrap()).await;
        return Ok(stream.unwrap());
    }
    Err(make_io_error("no channel found."))
}

pub fn report_update_window(
    cx: &mut Context<'_>,
    channel: &str,
    session_id: u32,
    stream_id: u32,
    window: u32,
) -> bool {
    let cmap = &mut CHANNEL_SESSIONS.lock().unwrap().channels;
    if let Some(csession) = cmap.get_mut(channel) {
        for cs in csession.sessions.iter_mut() {
            if let Some(ss) = cs {
                if ss.id == session_id {
                    let ev = new_window_update_event(stream_id, window, false);
                    match ss.event_tx.poll_ready(cx) {
                        Poll::Ready(Ok(())) => {}
                        _ => {
                            return false;
                        }
                    }
                    if let Ok(()) = ss.event_tx.try_send(ev) {
                        return true;
                    }
                }
            }
        }
    }
    true
}

async fn handle_rmux_stream(mut stream: MuxStream) -> Result<(), Box<dyn Error>> {
    let stream_id = stream.state.stream_id;
    let target = String::from(stream.target.addr.as_str());
    let result = get_channel_stream(String::from("direct"), target).await;
    match result {
        Ok(mut remote) => {
            {
                let (mut ri, mut wi) = stream.split();
                let (mut ro, mut wo) = remote.split();
                relay(stream_id, &mut ri, &mut wi, &mut ro, &mut wo).await?;
            }
            let _ = stream.close();
            let _ = remote.close();
            Ok(())
        }
        Err(e) => {
            let _ = stream.close();
            Err(Box::new(e))
        }
    }
}

fn handle_syn(
    channel: &str,
    session_id: u32,
    ev: Event,
    evtx: mpsc::Sender<Event>,
) -> Option<MuxStream> {
    let connect_req: ConnectRequest = match bincode::deserialize(&ev.body[..]) {
        Ok(m) => m,
        Err(err) => {
            error!(
                "Failed to parse ConnectRequest with error:{} while data len:{} {}",
                err,
                ev.body.len(),
                ev.header.len(),
            );
            return None;
        }
    };
    let sid = ev.header.stream_id;
    info!(
        "[{}]Handle conn request:{} {}",
        sid, connect_req.proto, connect_req.addr
    );
    let stream = MuxStream::new(channel, session_id, sid, evtx, connect_req);
    let handle = handle_rmux_stream(stream.clone()).map(move |r| {
        if let Err(e) = r {
            error!("[{}]Failed to handle rmux stream; error={}", sid, e);
        }
    });
    tokio::spawn(handle);
    Some(stream)
}

fn get_streams_stat_info(streams: &mut HashMap<u32, MuxStream>) -> String {
    let mut info = String::new();
    for (id, stream) in streams.iter_mut() {
        info.push_str(
            format!(
                "{}:target:{}, age:{:?}, send_bytes:{}, recv_bytes:{}, send_window:{}, closed:{}\n",
                id,
                stream.target.addr,
                stream.state.born_time.elapsed(),
                stream.state.total_send_bytes.load(Ordering::SeqCst),
                stream.state.total_recv_bytes.load(Ordering::SeqCst),
                stream.state.send_buf_window.load(Ordering::SeqCst),
                stream.state.closed.load(Ordering::SeqCst),
            )
            .as_str(),
        );
    }
    info
}

fn log_session_state(
    sid: u32,
    streams: &mut HashMap<u32, MuxStream>,
    now_unix_secs: u32,
    session_state: &Arc<MuxSessionState>,
) -> u32 {
    let mut stat_info = format!(
        "\n========================Session:{}====================\n",
        sid
    );
    stat_info.push_str(format!("Streams:{}\n", streams.len()).as_str());
    stat_info.push_str(format!("Age:{:?}\n", session_state.born_time.elapsed()).as_str());
    stat_info.push_str(format!("PingPongGap:{}\n", session_state.ping_pong_gap()).as_str());
    let idle_secs = session_state.get_io_idle_secs(now_unix_secs);
    stat_info.push_str(format!("IOIdleSecs:{}\n", idle_secs).as_str());
    stat_info.push_str(format!("Retired:{}\n", session_state.is_retired()).as_str());
    stat_info.push_str(format!("Closed:{}\n", session_state.is_closed()).as_str());
    stat_info.push_str(get_streams_stat_info(streams).as_str());
    warn!("{}", stat_info);
    idle_secs
}

fn handle_ping_event(
    sid: u32,
    streams: &mut HashMap<u32, MuxStream>,
    session_state: &Arc<MuxSessionState>,
    is_remote: bool,
) -> bool {
    let now_unix_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as u32;
    let idle_io_secs = log_session_state(sid, streams, now_unix_secs, &session_state);
    if session_state.is_retired() {
        if idle_io_secs >= 300 || streams.is_empty() {
            error!(
                "[{}]Close session since no data send/recv {} secs ago.",
                sid, idle_io_secs
            );
            session_state.closed.store(true, Ordering::SeqCst);
        }
        return false;
    }
    if !is_remote {
        session_state
            .last_ping_send_time
            .store(now_unix_secs, Ordering::SeqCst);
    }
    true
}

pub async fn handle_rmux_session(
    channel: &str,
    tunnel_id: u32,
    mut inbound: TcpStream,
    mut rctx: CryptoContext,
    mut wctx: CryptoContext,
    recv_buf: &mut BytesMut,
    max_alive_secs: u64,
    //cfg: &TunnelConfig,
) -> Result<(), std::io::Error> {
    let (mut ri, mut wi) = inbound.split();
    let (mut event_tx, mut event_rx) = mpsc::channel::<Event>(16);
    let (mut send_tx, mut send_rx) = mpsc::channel(16);

    let seed = if channel.is_empty() { 2 } else { 1 };
    let session_state = MuxSessionState {
        last_ping_send_time: AtomicU32::new(0),
        last_pong_recv_time: AtomicU32::new(0),
        born_time: Instant::now(),
        retired: AtomicBool::new(false),
        io_active_unix_secs: AtomicU32::new(0),
        closed: AtomicBool::new(false),
    };
    let session_state = Arc::new(session_state);
    let send_session_state = session_state.clone();
    let recv_session_state = session_state.clone();
    let mux_session = MuxSession {
        id: tunnel_id,
        event_tx: event_tx.clone(),
        pendding_streams: Vec::new(),
        stream_id_seed: AtomicU32::new(seed),
        state: session_state.clone(),
        max_alive_secs,
        //streams: HashMap::new(),
    };
    info!(
        "[{}][{}]Start tunnel session with crypto {} {}",
        channel, tunnel_id, rctx.nonce, rctx.key
    );
    store_mux_session(channel, mux_session);

    let mut handle_recv_event_tx = event_tx.clone();
    let mut handle_recv_send_tx = send_tx.clone();
    let handle_recv = async move {
        loop {
            let recv_event = read_encrypt_event(&mut rctx, &mut ri, recv_buf).await;
            match recv_event {
                Ok(Some(mut ev)) => {
                    recv_session_state.io_active_unix_secs.store(
                        SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .unwrap()
                            .as_secs() as u32,
                        Ordering::SeqCst,
                    );
                    ev.remote = true;
                    if FLAG_DATA != ev.header.flags() {
                        info!(
                            "[{}][{}][{}]remote recv event type:{}, len:{}",
                            channel,
                            tunnel_id,
                            ev.header.stream_id,
                            get_event_type_str(ev.header.flags()),
                            ev.header.len(),
                        );
                    }
                    let _ = handle_recv_event_tx.send(ev).await;
                }
                Ok(None) => {
                    break;
                }
                Err(err) => {
                    error!("Close remote recv since of error:{}", err);
                    break;
                }
            }
        }
        error!("[{}][{}]handle_recv done", channel, tunnel_id);
        let shutdown_ev = new_shutdown_event(0, false);
        let _ = handle_recv_event_tx.send(shutdown_ev).await;
        let _ = handle_recv_send_tx.send(Vec::new()).await;
    };

    let handle_event_event_tx = event_tx.clone();
    let mut handle_event_send_tx = send_tx.clone();
    let handle_event = async move {
        let mut streams = HashMap::new();
        while !session_state.closed.load(Ordering::SeqCst) {
            let rev = event_rx.recv().await;
            if let Some(ev) = rev {
                if FLAG_PING == ev.header.flags()
                    && !handle_ping_event(tunnel_id, &mut streams, &session_state, ev.remote)
                {
                    continue;
                }
                if !ev.remote {
                    if FLAG_SHUTDOWN == ev.header.flags() {
                        break;
                    }
                    if FLAG_SYN == ev.header.flags() {
                        hanle_pendding_mux_streams(channel, tunnel_id, &mut streams);
                    }
                    if FLAG_FIN == ev.header.flags() {
                        if let Some(mut stream) = streams.remove(&ev.header.stream_id) {
                            let _ = stream.close();
                        }
                        if session_state.is_retired() && streams.is_empty() {
                            session_state.closed.store(true, Ordering::SeqCst);
                            break;
                        }
                    }
                    let mut buf = BytesMut::with_capacity(ev.body.len() + 64);
                    wctx.encrypt(&ev, &mut buf);
                    let evbuf = buf.to_vec();
                    let _ = send_tx.send(evbuf).await;
                    continue;
                }
                match ev.header.flags() {
                    FLAG_SYN => {
                        if let Some(stream) =
                            handle_syn(channel, tunnel_id, ev, handle_event_event_tx.clone())
                        {
                            streams.entry(stream.state.stream_id).or_insert(stream);
                        } else {
                        }
                    }
                    FLAG_FIN => {
                        if let Some(mut stream) = streams.remove(&ev.header.stream_id) {
                            let _ = stream.close();
                        }
                        if session_state.is_retired() && streams.is_empty() {
                            session_state.closed.store(true, Ordering::SeqCst);
                            break;
                        }
                    }
                    FLAG_DATA => {
                        if let Some(stream) = streams.get_mut(&ev.header.stream_id) {
                            stream.offer_data(ev.body).await;
                        } else {
                            warn!(
                                "[{}][{}]No stream found for data event.",
                                channel, ev.header.stream_id
                            );
                        }
                    }
                    FLAG_PING => {
                        let mut buf = BytesMut::new();
                        let pong = new_pong_event(ev.header.stream_id, false);
                        wctx.encrypt(&pong, &mut buf);
                        let evbuf = buf.to_vec();
                        let _ = send_tx.send(evbuf).await;
                    }
                    FLAG_PONG => {
                        //
                        session_state.last_pong_recv_time.store(
                            SystemTime::now()
                                .duration_since(UNIX_EPOCH)
                                .unwrap()
                                .as_secs() as u32,
                            Ordering::SeqCst,
                        );
                    }
                    FLAG_WIN_UPDATE => {
                        if let Some(stream) = streams.get_mut(&ev.header.stream_id) {
                            stream.update_send_window(ev.header.len());
                        }
                    }
                    _ => {
                        error!("invalid flags:{}", ev.header.flags());
                        //None
                    }
                }
            } else {
                //None
                break;
            }
        }
        error!("[{}][{}]handle_event done", channel, tunnel_id);
        for (_, stream) in streams.iter_mut() {
            let _ = stream.close();
        }
        event_rx.close();
        let _ = handle_event_send_tx.send(Vec::new()).await;
    };

    let handle_send = async {
        while let Some(data) = send_rx.recv().await {
            if data.is_empty() {
                break;
            }
            if let Err(e) = wi.write_all(&data[..]).await {
                error!("Failed to write data with err:{}", e);
                break;
            }
            send_session_state.io_active_unix_secs.store(
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_secs() as u32,
                Ordering::SeqCst,
            );
        }
        error!("[{}][{}]handle_send done", channel, tunnel_id);
        send_rx.close();
        let _ = wi.shutdown().await;
        let shutdown_ev = new_shutdown_event(0, false);
        let _ = event_tx.send(shutdown_ev).await;
    };

    join3(handle_recv, handle_event, handle_send).await;
    erase_mux_session(channel, tunnel_id);
    info!("[{}][{}]Close tunnel session", channel, tunnel_id);
    Ok(())
}
