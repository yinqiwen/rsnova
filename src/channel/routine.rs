use super::rmux::init_rmux_client;
use crate::config::ChannelConfig;
use crate::rmux::{get_channel_session_size, routine_all_sessions};
use chrono::{Local, Timelike};
use futures::FutureExt;
use rand::Rng;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, SystemTime};
use tokio::time;

pub async fn routine_channels(cfgs: Option<Vec<ChannelConfig>>) {
    let mut interval = time::interval(Duration::from_secs(5));
    let session_id_seed = AtomicU32::new(0);
    let mut ping_time: u64 = 0;
    loop {
        interval.tick().await;
        let now = Local::now();
        if let Some(ccfgs) = &cfgs {
            for channel_cfg in ccfgs.iter() {
                if !channel_cfg.is_valid_hour(now.hour() as u8) {
                    continue;
                }
                let count = get_channel_session_size(channel_cfg.name.as_str());
                if count < channel_cfg.conns_per_host as usize {
                    let n = channel_cfg.conns_per_host as usize - count;
                    for _ in 0..n {
                        let init_cfg = channel_cfg.clone();
                        let f = init_rmux_client(
                            init_cfg,
                            session_id_seed.fetch_add(1, Ordering::SeqCst),
                        )
                        .map(|r| {
                            if let Err(e) = r {
                                error!("Failed to init_rmux_client; error={}", e);
                            }
                        });
                        tokio::spawn(f);
                    }
                }
            }
        }
        match SystemTime::now().duration_since(SystemTime::UNIX_EPOCH) {
            Ok(n) => {
                let rand_inc: i64 = {
                    let mut rng = rand::thread_rng();
                    rng.gen_range(-10, 10)
                };
                if n.as_secs() - ping_time > (30 + rand_inc) as u64 {
                    routine_all_sessions().await;
                    ping_time = n.as_secs();
                }
            }
            Err(_) => panic!("SystemTime before UNIX EPOCH!"),
        }
    }
}
