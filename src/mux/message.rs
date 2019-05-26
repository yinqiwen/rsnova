use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, PartialEq, Debug)]
struct ConnectRequest {
    pub proto: String,
    pub host: String,
    pub port: u32,
}
