use serde::{Deserialize, Serialize};
use std::sync::Mutex;

lazy_static! {
    static ref GLOBAL_CONFIG: Mutex<Config> = Mutex::new(Config::new());
}

#[derive(Serialize, Deserialize, Debug, Default, Clone)]
pub struct ChannelConfig {
    pub name: String,
    pub urls: Vec<String>,
    pub ping_interval_sec: u32,
    pub conns_per_host: u32,
    pub max_alive_mins: u32,
    pub proxy: String,
}

#[derive(Serialize, Deserialize, Debug, Default)]
pub struct CipherConfig {
    pub key: String,
    pub method: String,
}

#[derive(Serialize, Deserialize, Debug, Default)]
pub struct LocalConfig {
    pub channels: Vec<ChannelConfig>,
}

#[derive(Serialize, Deserialize, Debug, Default)]
pub struct RemoteConfig {}

#[derive(Serialize, Deserialize, Debug)]
pub struct Config {
    pub cipher: CipherConfig,
    pub read_timeout_sec: u64,
    pub local: LocalConfig,
    pub remote: RemoteConfig,
}

impl Config {
    pub fn new() -> Self {
        Self {
            cipher: CipherConfig {
                key: String::from("44eb1a8421c5f9c96e405a77f70646c8"),
                method: String::from("aes128gcm"),
            },
            read_timeout_sec: 30,
            local: Default::default(),
            remote: Default::default(),
        }
    }
}

pub fn get_config() -> &'static Mutex<Config> {
    &GLOBAL_CONFIG
}

pub fn add_channel_config(url: &str, proxy: &str) {
    let mut ch: ChannelConfig = Default::default();
    ch.urls.push(String::from(url));
    ch.conns_per_host = 3;
    ch.max_alive_mins = 10;
    ch.name = String::from("default");
    ch.proxy = String::from(proxy);
    get_config().lock().unwrap().local.channels.push(ch);
}

pub fn set_default_cipher_key(key: &str) {
    get_config().lock().unwrap().cipher.key = String::from(key);
}

pub fn set_default_cipher_method(method: &str) {
    get_config().lock().unwrap().cipher.method = String::from(method);
}
