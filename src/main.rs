#![warn(rust_2018_idioms)]

#[macro_use]
extern crate log;

mod channel;
mod config;
mod tunnel;

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
        .arg(
            Arg::with_name("client")
                .long("client")
                .help("Launch as client mode")
                .takes_value(false)
                .multiple(false)
                .conflicts_with("server"),
        )
        .arg(
            Arg::with_name("server")
                .long("server")
                .help("Launch as server mode")
                .takes_value(false)
                .multiple(false)
                .conflicts_with("client"),
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
    let client_mode = matches.is_present("client");
    let server_mode = matches.is_present("server");
    if !client_mode && !server_mode {
        //return Err(std::io::Error::new(ErrorKind::Other, "Need specify 'client' or 'server'."))
        panic!("Need specify 'client' or 'server'.");
    }
    for c in cfg.tunnel {
        info!("Start rsnova client at {} ", c.listen);
        let handle = tunnel::start_local_server(c).map(|r| {
            if let Err(e) = r {
                error!("Failed to start server; error={}", e);
            }
        });
        tokio::spawn(handle);
    }

    // Open a TCP stream to the socket address.
    //
    // Note that this is the Tokio TcpStream, which is fully async.
    // let mut stream = TcpStream::connect("127.0.0.1:6142").await?;
    // println!("created stream");

    // let result = stream.write(b"hello world\n").await;
    // println!("wrote to stream; success={:?}", result.is_ok());
    let wait = std::time::Duration::from_secs(10_000_000);
    std::thread::sleep(wait);

    Ok(())
}
