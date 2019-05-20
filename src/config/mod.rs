extern crate serde;

use self::serde::{Serialize, Deserialize};

#[derive(Serialize, Deserialize,Debug)]
pub struct Config{
    pub listen: String,
}