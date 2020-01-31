use clap::{App, Arg};
use std::fs::File;
use std::io::Read;

use std::error::Error;

#[tokio::main]
pub async fn main() -> Result<(), Box<dyn Error>> {
    let matches = App::new("rsnova")
        .version("0.1.0")
        .author("yinqiwen<yinqiwen@gmail.com>")
        .about("Private proxy solution & network troubleshooting tool.")
        .arg(
            Arg::with_name("config")
                .short("c")
                .long("config")
                .default_value("./rsnova.toml")
                .value_name("FILE")
                .help("Sets a custom config file")
                .takes_value(true),
        )
        .get_matches();
    let confile_name = matches.value_of("config").unwrap();
    let mut confile = match File::open(matches.value_of("config").unwrap()) {
        Ok(f) => f,
        Err(e) => panic!("Error occurred opening file: {} - Err: {}", confile_name, e),
    };
    let mut confstr = String::new();
    match confile.read_to_string(&mut confstr) {
        Ok(s) => s,
        Err(e) => panic!("Error Reading file: {}", e),
    };
    let cfg: rsnova::Config = toml::from_str(confstr.as_str()).unwrap();
    rsnova::start_rsnova(cfg).await?;
    Ok(())
}
