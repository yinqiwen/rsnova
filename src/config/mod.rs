use serde::{Deserialize, Serialize};

// lazy_static! {
//     static ref GLOBAL_CONFIG: Mutex<Config> = Mutex::new(Config::new());
// }

#[derive(Serialize, Deserialize, Debug, Default, Clone)]
pub struct LogConfig {
    pub logtostderr: bool,
    pub level: String,
    pub logdir: String,
}

#[derive(Serialize, Deserialize, Debug, Default, Clone)]
pub struct PACConfig {
    pub host: String,
    pub channel: String,
}

#[derive(Serialize, Deserialize, Debug, Default, Clone)]
pub struct CipherConfig {
    pub key: String,
    pub method: String,
}

#[derive(Serialize, Deserialize, Debug, Default, Clone)]
pub struct ChannelConfig {
    pub name: String,
    pub url: String,
    pub cipher: CipherConfig,
    pub ping_interval_sec: u32,
    pub conns_per_host: u32,
    pub max_alive_mins: u32,
    pub proxy: String,
}

#[derive(Serialize, Deserialize, Debug, Default, Clone)]
pub struct TunnelConfig {
    pub listen: String,
    pub pac: Vec<PACConfig>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Config {
    pub log: LogConfig,
    pub tunnel: Vec<TunnelConfig>,
    // pub server: Vec<ServerConfig>,
    // pub channel: Vec<ChannelConfig>,
}
