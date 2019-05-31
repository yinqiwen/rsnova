use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, PartialEq, Debug)]
pub struct ConnectRequest {
    pub proto: String,
    pub addr: String,
}
