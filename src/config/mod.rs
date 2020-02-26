use regex::Regex;
use serde::{Deserialize, Serialize};

// lazy_static! {
//     static ref GLOBAL_CONFIG: Mutex<Config> = Mutex::new(Config::new());
// }

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct LogConfig {
    pub logtostderr: bool,
    pub level: String,
    pub logdir: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct PACConfig {
    pub host: String,
    pub channel: String,
    #[serde(skip)]
    pub re: Option<Regex>,
}

impl PACConfig {
    pub fn init(&mut self) {
        if self.re.is_none() {
            self.re = Some(Regex::new(self.host.as_str()).unwrap());
        }
    }
    pub fn is_match(&self, addr: &str) -> bool {
        self.re.as_ref().unwrap().is_match(addr)
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct CipherConfig {
    pub key: String,
    pub method: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ChannelConfig {
    pub name: String,
    pub url: String,
    pub cipher: CipherConfig,
    pub ping_interval_sec: u32,
    pub conns_per_host: u32,
    pub max_alive_mins: u32,
    pub proxy: Option<String>,
    pub work_time_frame: Option<[u8; 2]>,
    pub sni: Option<String>,
    pub sni_proxy: Option<String>,
}

impl ChannelConfig {
    pub fn is_valid_hour(&self, h: u8) -> bool {
        if let Some(frame) = self.work_time_frame {
            return frame[0] <= h && frame[1] > h;
        }
        true
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct TunnelConfig {
    pub listen: String,
    pub cipher: Option<CipherConfig>,
    pub pac: Vec<PACConfig>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct DebugConfig {
    pub listen: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Config {
    pub log: LogConfig,
    pub tunnel: Vec<TunnelConfig>,
    // pub server: Vec<ServerConfig>,
    pub channel: Option<Vec<ChannelConfig>>,
    pub debug: Option<DebugConfig>,
}
