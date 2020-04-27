use super::crypto::{read_rmux_event, CryptoContext};
use super::event::{
    get_event_type_str, new_ping_event, new_pong_event, new_routine_event, new_shutdown_event,
    new_syn_event, new_window_update_event, Event, FLAG_DATA, FLAG_FIN, FLAG_PING, FLAG_PONG,
    FLAG_ROUTINE, FLAG_SHUTDOWN, FLAG_SYN, FLAG_WIN_UPDATE,
};
use super::message::ConnectRequest;
use super::stream::MuxStream;
use super::DEFAULT_RECV_BUF_SIZE;
use crate::channel::get_channel_stream;
use crate::channel::ChannelStream;
use crate::tunnel::relay;
use crate::utils::{clear_channel, make_io_error, VBuf};
use bytes::BytesMut;
use futures::future::join3;
use futures::FutureExt;
use rand::Rng;
use std::collections::HashMap;
use std::error::Error;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncBufRead, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TryRecvError;

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
    process_event_state: AtomicU32,
    process_send_state: AtomicU32,
    process_recv_state: AtomicU32,
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
}

fn store_mux_session(channel: &str, session: MuxSession) {
    let cmap = &mut CHANNEL_SESSIONS.lock().unwrap().channels;
    //info!("{}0 store cmap size:{}", channel, cmap.len());
    if cmap.get_mut(channel).is_none() {
        let csession = ChannelMuxSession {
            sessions: Vec::new(),
            cursor: AtomicU32::new(0),
        };
        cmap.insert(String::from(channel), csession);
    }
    if let Some(csession) = cmap.get_mut(channel) {
        for s in csession.sessions.iter_mut() {
            if s.is_none() {
                *s = Some(session);
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
    let mut len: usize = 0;
    if let Some(csession) = cmap.get_mut(channel) {
        for s in csession.sessions.iter() {
            if s.is_some() {
                len += 1;
            }
        }
    }
    len
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
            for session in csession.sessions.iter_mut() {
                if let Some(s) = session {
                    if s.state.ping_pong_gap() < -60 {
                        error!("[{}]Session heartbeat timeout.", s.id);
                        let shutdown = new_shutdown_event(0, false);
                        actions.push(RoutineAction::new(shutdown, s.event_tx.clone()));
                        s.state.retired.store(true, Ordering::SeqCst);
                        retired.push(session.take().unwrap());
                        continue;
                    } else {
                        if !channel.is_empty() {
                            let ping = new_ping_event(0, false);
                            actions.push(RoutineAction::new(ping, s.event_tx.clone()));
                        }
                        if s.state.closed.load(Ordering::SeqCst) {
                            s.state.retired.store(true, Ordering::SeqCst);
                            retired.push(session.take().unwrap());
                        } else {
                            let r = new_routine_event(0);
                            actions.push(RoutineAction::new(r, s.event_tx.clone()));
                            if s.max_alive_secs > 0 && !channel.is_empty() {
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
        }
        for s in holder.retired.iter_mut() {
            let r = new_routine_event(0);
            actions.push(RoutineAction::new(r, s.event_tx.clone()));
        }
        holder.retired.append(&mut retired);
    }
    for action in actions.iter_mut() {
        let ev = action.ev.take().unwrap();
        let _ = action.sender.try_send(ev);
    }
}

pub async fn create_stream(
    channel: &str,
    proto: &str,
    addr: &str,
    relay_buf_size: usize,
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
                        relay_buf_size,
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
    let relay_buf_size = stream.relay_buf_size();
    let target = String::from(stream.target.addr.as_str());
    let result = get_channel_stream(String::from("direct"), target).await;
    match result {
        Ok(mut remote) => {
            {
                let (mut ri, mut wi) = stream.split();
                let (mut ro, mut wo) = remote.split();
                relay(
                    stream_id,
                    &mut ri,
                    &mut wi,
                    &mut ro,
                    &mut wo,
                    relay_buf_size,
                )
                .await?;
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
    relay_buf_size: usize,
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
    let stream = MuxStream::new(channel, session_id, sid, evtx, connect_req, relay_buf_size);
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

fn log_session_state(sid: u32, now_unix_secs: u32, session_state: &Arc<MuxSessionState>) -> String {
    let mut stat_info = format!(
        "========================Session:{}====================\n",
        sid
    );
    stat_info.push_str(format!("Age:{:?}\n", session_state.born_time.elapsed()).as_str());
    stat_info.push_str(format!("PingPongGap:{}\n", session_state.ping_pong_gap()).as_str());
    let idle_secs = session_state.get_io_idle_secs(now_unix_secs);
    stat_info.push_str(format!("IOIdleSecs:{}\n", idle_secs).as_str());
    stat_info.push_str(format!("Retired:{}\n", session_state.is_retired()).as_str());
    stat_info.push_str(format!("Closed:{}\n", session_state.is_closed()).as_str());
    stat_info.push_str(
        format!(
            "ProcEventState:{}\n",
            session_state.process_event_state.load(Ordering::SeqCst)
        )
        .as_str(),
    );
    stat_info.push_str(
        format!(
            "ProcSendState:{}\n",
            session_state.process_send_state.load(Ordering::SeqCst)
        )
        .as_str(),
    );
    stat_info.push_str(
        format!(
            "ProcRecvState:{}\n",
            session_state.process_recv_state.load(Ordering::SeqCst)
        )
        .as_str(),
    );
    //stat_info.push_str(get_streams_stat_info(streams).as_str());
    stat_info
}

pub fn dump_session_state() -> String {
    let now_unix_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as u32;
    let mut stat_info = String::from("========================Sessions====================\n");
    {
        let ss = &mut CHANNEL_SESSIONS.lock().unwrap();
        let cmap = &mut ss.channels;
        for (channel, csession) in cmap {
            stat_info.push_str(format!("======Channel:{}=======\n", channel).as_str());
            let mut count = 0;
            for s in csession.sessions.iter() {
                match s {
                    Some(session) => {
                        let info = log_session_state(session.id, now_unix_secs, &session.state);
                        stat_info.push_str(info.as_str());
                        count += 1;
                    }
                    None => {
                        //
                    }
                }
            }
            stat_info
                .push_str(format!("======Channel:{} Count:{}======\n", channel, count).as_str());
        }
        stat_info.push_str("\nRetired Sessions:\n");
        let retired = &mut ss.retired;
        for s in retired {
            let info = log_session_state(s.id, now_unix_secs, &s.state);
            stat_info.push_str(info.as_str());
        }
    }
    stat_info
}

fn handle_ping_event(
    _sid: u32,
    _streams: &mut HashMap<u32, MuxStream>,
    session_state: &Arc<MuxSessionState>,
    is_remote: bool,
) {
    if !is_remote {
        let now_unix_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as u32;
        session_state
            .last_ping_send_time
            .store(now_unix_secs, Ordering::SeqCst);
    }
}

fn handle_routine_event(
    sid: u32,
    streams: &mut HashMap<u32, MuxStream>,
    session_state: &Arc<MuxSessionState>,
) -> bool {
    let now_unix_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as u32;
    let idle_io_secs = session_state.get_io_idle_secs(now_unix_secs);
    let mut stat_info = log_session_state(sid, now_unix_secs, &session_state);
    stat_info.push_str(get_streams_stat_info(streams).as_str());
    stat_info.push_str(format!("Streams:{}\n", streams.len()).as_str());
    info!("{}", stat_info);
    let should_close = (session_state.is_retired() && streams.is_empty()) || idle_io_secs >= 300;

    if should_close {
        error!(
            "[{}]Close session since no data send/recv {} secs ago, stream count:{}",
            sid,
            idle_io_secs,
            streams.len()
        );
        session_state.closed.store(true, Ordering::SeqCst);
        return true;
    }
    false
}

fn handle_fin_event(
    sid: u32,
    streams: &mut HashMap<u32, MuxStream>,
    session_state: &Arc<MuxSessionState>,
) -> bool {
    if let Some(mut stream) = streams.remove(&sid) {
        let _ = stream.close();
    }
    if session_state.is_retired() && streams.is_empty() {
        session_state.closed.store(true, Ordering::SeqCst);
        return true;
    }
    false
}

async fn send_local_event(
    mut ev: Event,
    wctx: &mut CryptoContext,
    send_tx: &mut mpsc::Sender<Vec<u8>>,
) -> bool {
    let mut buf = BytesMut::with_capacity(ev.body.len() + 64);
    wctx.encrypt(&mut ev, &mut buf);
    let evbuf = buf.to_vec();
    let send_rc = send_tx.send(evbuf).await;
    send_rc.is_ok()
}

async fn handle_local_event<'a>(
    channel: &'a str,
    tunnel_id: u32,
    streams: &mut HashMap<u32, MuxStream>,
    session_state: &Arc<MuxSessionState>,
    ev: Event,
    wctx: &mut CryptoContext,
    send_tx: &mut mpsc::Sender<Vec<u8>>,
) -> bool {
    if FLAG_SHUTDOWN == ev.header.flags() {
        return false;
    }
    if FLAG_SYN == ev.header.flags() {
        hanle_pendding_mux_streams(channel, tunnel_id, streams);
    }
    if FLAG_FIN == ev.header.flags()
        && handle_fin_event(ev.header.stream_id, streams, &session_state)
    {
        return false;
    }
    if FLAG_ROUTINE == ev.header.flags() {
        return !handle_routine_event(tunnel_id, streams, &session_state);
    }
    send_local_event(ev, wctx, send_tx).await
}

async fn process_event<'a>(
    channel: &'a str,
    tunnel_id: u32,
    mut wctx: CryptoContext,
    session_state: Arc<MuxSessionState>,
    mut event_rx: mpsc::Receiver<Event>,
    event_tx: mpsc::Sender<Event>,
    mut send_tx: mpsc::Sender<Vec<u8>>,
    relay_buf_size: usize,
) {
    let mut streams = HashMap::new();
    while !session_state.closed.load(Ordering::SeqCst) {
        session_state.process_event_state.store(0, Ordering::SeqCst);
        let rev = event_rx.recv().await;
        if let Some(ev) = rev {
            if FLAG_PING == ev.header.flags() {
                handle_ping_event(tunnel_id, &mut streams, &session_state, ev.remote);
            }
            if !ev.remote {
                session_state.process_event_state.store(3, Ordering::SeqCst);
                if handle_local_event(
                    channel,
                    tunnel_id,
                    &mut streams,
                    &session_state,
                    ev,
                    &mut wctx,
                    &mut send_tx,
                )
                .await
                {
                    session_state.process_event_state.store(4, Ordering::SeqCst);
                    continue;
                }
                session_state.process_event_state.store(4, Ordering::SeqCst);
                break;
            }
            match ev.header.flags() {
                FLAG_SYN => {
                    if let Some(stream) =
                        handle_syn(channel, tunnel_id, ev, event_tx.clone(), relay_buf_size)
                    {
                        streams.entry(stream.state.stream_id).or_insert(stream);
                    } else {
                    }
                }
                FLAG_FIN => {
                    if handle_fin_event(ev.header.stream_id, &mut streams, &session_state) {
                        break;
                    }
                }
                FLAG_DATA => {
                    if let Some(stream) = streams.get_mut(&ev.header.stream_id) {
                        session_state.process_event_state.store(1, Ordering::SeqCst);
                        stream.offer_data(ev.body).await;
                        session_state.process_event_state.store(2, Ordering::SeqCst);
                    } else {
                        warn!(
                            "[{}][{}]No stream found for data event.",
                            channel, ev.header.stream_id
                        );
                    }
                }
                FLAG_PING => {
                    if !send_local_event(
                        new_pong_event(ev.header.stream_id, false),
                        &mut wctx,
                        &mut send_tx,
                    )
                    .await
                    {
                        break;
                    }
                }
                FLAG_PONG => {
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
    session_state.process_event_state.store(5, Ordering::SeqCst);
    error!("[{}][{}]handle_event done", channel, tunnel_id);
    session_state.closed.store(true, Ordering::SeqCst);
    for (_, stream) in streams.iter_mut() {
        let _ = stream.close();
    }
    clear_channel(&mut event_rx);

    let _ = send_tx.send(Vec::new()).await;
    session_state.process_event_state.store(6, Ordering::SeqCst);
}

pub struct MuxContext<'a> {
    channel: &'a str,
    tunnel_id: u32,
    rctx: CryptoContext,
    wctx: CryptoContext,
    max_alive_secs: u64,
}
impl<'a> MuxContext<'a> {
    pub fn new(
        channel: &'a str,
        tunnel_id: u32,
        rctx: CryptoContext,
        wctx: CryptoContext,
        max_alive_secs: u64,
    ) -> Self {
        Self {
            channel,
            tunnel_id,
            rctx,
            wctx,
            max_alive_secs,
        }
    }
}

pub async fn process_rmux_session<'a, R, W>(
    ctx: MuxContext<'a>,
    ri: &'a mut R,
    wi: &'a mut W,
    relay_buf_size: usize,
) -> Result<(), std::io::Error>
where
    R: AsyncBufRead + Unpin + Sized,
    W: AsyncWrite + Unpin + Sized,
{
    let channel = ctx.channel;
    let tunnel_id = ctx.tunnel_id;
    let mut rctx = ctx.rctx;
    let wctx = ctx.wctx;
    let max_alive_secs = ctx.max_alive_secs;
    let (mut event_tx, event_rx) = mpsc::channel::<Event>(16);
    let (send_tx, mut send_rx) = mpsc::channel(16);

    //let is_server = channel.is_empty();

    let seed = if channel.is_empty() { 2 } else { 1 };
    let session_state = MuxSessionState {
        last_ping_send_time: AtomicU32::new(0),
        last_pong_recv_time: AtomicU32::new(0),
        born_time: Instant::now(),
        retired: AtomicBool::new(false),
        io_active_unix_secs: AtomicU32::new(0),
        closed: AtomicBool::new(false),
        process_event_state: AtomicU32::new(0),
        process_send_state: AtomicU32::new(0),
        process_recv_state: AtomicU32::new(0),
    };
    let session_state = Arc::new(session_state);
    //let send_session_state = session_state.clone();
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

    let (mut close_tx, mut close_rx) = mpsc::channel::<()>(1);
    //let mut drop = close_rx.fuse();

    let mut handle_recv_event_tx = event_tx.clone();
    let mut handle_recv_send_tx = send_tx.clone();
    let handle_recv_session_state = session_state.clone();
    let handle_send_session_state = session_state.clone();
    let handle_recv = async move {
        while !handle_recv_session_state.closed.load(Ordering::SeqCst) {
            tokio::select! {
                recv_event = read_rmux_event(&mut rctx, ri) => {
                    match recv_event {
                        Ok(mut ev) => {
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
                            handle_recv_session_state.process_recv_state.store(1, Ordering::SeqCst);
                            let send_rc = handle_recv_event_tx.send(ev).await;
                            handle_recv_session_state.process_recv_state.store(0, Ordering::SeqCst);
                            if send_rc.is_err(){
                                break;
                            }
                        }
                        Err(err) => {
                            error!("Close remote recv since of error:{}", err);
                            break;
                        }
                    }
                },
                _ = close_rx.recv() => {
                    handle_recv_session_state.closed.store(true, Ordering::SeqCst);
                    break;
                },
            }
        }
        handle_recv_session_state
            .process_recv_state
            .store(3, Ordering::SeqCst);
        error!("[{}][{}]handle_recv done", channel, tunnel_id);
        handle_recv_session_state
            .closed
            .store(true, Ordering::SeqCst);
        //let shutdown_ev = new_shutdown_event(0, false);
        //let _ = handle_recv_event_tx.send(shutdown_ev).await;
        let _ = handle_recv_send_tx.send(Vec::new()).await;
        clear_channel(&mut close_rx);
        handle_recv_session_state
            .process_recv_state
            .store(2, Ordering::SeqCst);
    };

    // let handle_event_event_tx = event_tx.clone();
    // let mut handle_event_send_tx = send_tx.clone();
    let handle_event = process_event(
        channel,
        tunnel_id,
        wctx,
        session_state.clone(),
        event_rx,
        event_tx.clone(),
        send_tx.clone(),
        relay_buf_size,
    );

    let handle_send = async {
        let mut vbuf = VBuf::new();
        while !handle_send_session_state.closed.load(Ordering::SeqCst) {
            // if let Some(data) = send_rx.recv().await {
            //     if data.is_empty() {
            //         break;
            //     }
            //     if let Err(e) = wi.write_all(&data[..]).await {
            //         error!("Failed to write data with err:{}", e);
            //         break;
            //     }
            //     send_session_state.io_active_unix_secs.store(
            //         SystemTime::now()
            //             .duration_since(UNIX_EPOCH)
            //             .unwrap()
            //             .as_secs() as u32,
            //         Ordering::SeqCst,
            //     );
            // } else {
            //     break;
            // }

            if vbuf.vlen() == 0 {
                if let Some(data) = send_rx.recv().await {
                    if data.is_empty() {
                        break;
                    }
                    vbuf.push(data);
                } else {
                    break;
                }
            }
            let mut exit = false;
            while vbuf.vlen() < 60 {
                match send_rx.try_recv() {
                    Ok(data) => {
                        if data.is_empty() {
                            exit = true;
                            break;
                        } else {
                            vbuf.push(data);
                        }
                    }
                    Err(TryRecvError::Closed) => {
                        exit = true;
                        break;
                    }
                    Err(TryRecvError::Empty) => {
                        break;
                    }
                }
            }
            if exit {
                break;
            }
            session_state.io_active_unix_secs.store(
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_secs() as u32,
                Ordering::SeqCst,
            );
            match wi.write_buf(&mut vbuf).await {
                Ok(n) => {
                    if 0 == n {
                        break;
                    }
                }
                Err(_) => {
                    break;
                }
            }
        }
        handle_send_session_state
            .process_send_state
            .store(1, Ordering::SeqCst);
        error!("[{}][{}]handle_send done", channel, tunnel_id);
        handle_send_session_state
            .closed
            .store(true, Ordering::SeqCst);
        clear_channel(&mut send_rx);
        let close_rc = close_tx.send(()).await;
        if close_rc.is_err() {
            error!("[{}][{}]Close error:{:?}", channel, tunnel_id, close_rc);
        }
        let shutdown_ev = new_shutdown_event(0, false);
        let _ = event_tx.send(shutdown_ev).await;
        handle_send_session_state
            .process_send_state
            .store(2, Ordering::SeqCst);
    };

    join3(handle_recv, handle_event, handle_send).await;
    erase_mux_session(channel, tunnel_id);
    info!("[{}][{}]Close tunnel session", channel, tunnel_id);
    Ok(())
}

pub async fn handle_rmux_session(
    channel: &str,
    tunnel_id: u32,
    mut inbound: TcpStream,
    rctx: CryptoContext,
    wctx: CryptoContext,
    max_alive_secs: u64,
    relay_buf_size: usize,
    //cfg: &TunnelConfig,
) -> Result<(), std::io::Error> {
    let (ri, mut wi) = inbound.split();
    let mut buf_reader = tokio::io::BufReader::with_capacity(DEFAULT_RECV_BUF_SIZE, ri);
    let ctx = MuxContext::new(channel, tunnel_id, rctx, wctx, max_alive_secs);
    process_rmux_session(
        ctx, // channel,
        // tunnel_id,
        &mut buf_reader,
        &mut wi,
        // rctx,
        // wctx,
        // recv_buf,
        // max_alive_secs,
        relay_buf_size,
    )
    .await?;
    let _ = inbound.shutdown(std::net::Shutdown::Both);
    Ok(())
}
