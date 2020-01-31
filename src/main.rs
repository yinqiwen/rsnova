#![warn(rust_2018_idioms)]

#[macro_use]
extern crate log;

#[macro_use]
extern crate lazy_static;

#[macro_use]
extern crate futures;

mod channel;
mod config;
mod rmux;
mod tunnel;
mod utils;

use clap::{App, Arg};
use config::Config;
use futures::FutureExt;
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
    let cfg: Config = toml::from_str(confstr.as_str()).unwrap();
    let mut logger = flexi_logger::Logger::with_str(cfg.log.level.as_str());
    if !cfg.log.logdir.is_empty() {
        logger = logger
            .log_to_file()
            .rotate(
                flexi_logger::Criterion::Size(1024 * 1024),
                flexi_logger::Naming::Numbers,
                flexi_logger::Cleanup::KeepLogFiles(10),
            )
            .directory(cfg.log.logdir)
            .format(flexi_logger::colored_opt_format);
    }
    if cfg.log.logtostderr {
        logger = logger.duplicate_to_stderr(flexi_logger::Duplicate::Info);
    }
    logger.start().unwrap();

    for c in cfg.tunnel {
        info!("Start rsnova client at {} ", c.listen);
        let handle = tunnel::start_tunnel_server(c).map(|r| {
            if let Err(e) = r {
                error!("Failed to start server; error={}", e);
            }
        });
        tokio::spawn(handle);
    }

    channel::routine_channels(cfg.channel).await;

    Ok(())
}
