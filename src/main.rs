#[macro_use]
extern crate log;
extern crate byteorder;
extern crate bytes;
extern crate clap;
extern crate futures;
extern crate httparse;

extern crate nom;
extern crate simplelog;

extern crate tokio;
extern crate tokio_io;

mod common;
mod config;
mod mux;
mod proxy;

use bytes::BufMut;
use clap::{App, Arg};
use config::Config;
use std::fs::File;

use simplelog::Config as LogConfig;
use simplelog::{CombinedLogger, LevelFilter, TermLogger, WriteLogger};

struct T {
    a: String,
    b: bytes::BytesMut,
}
type PeekAddress1 = (String, bytes::BytesMut);
fn test() {
    let mut t = T {
        a: String::from("a"),
        b: bytes::BytesMut::with_capacity(10),
    };
    t.a = String::from("a");
    t.b.put_u8(1);

    let t1 = T {
        a: String::from("a"),
        b: t.b,
    };
    println!("{}  {}", t.a, t1.b[0]);

    let t2 = (String::from("a"), bytes::BytesMut::with_capacity(10));
}

fn main() {
    let matches = App::new("rsnova")
        .version("0.1.0")
        .author("yinqiwen<yinqiwen@gmail.com>")
        .about("Private proxy solution & network troubleshooting tool.")
        .arg(
            Arg::with_name("config")
                .short("c")
                .long("config")
                .value_name("FILE")
                .help("Sets a custom config file")
                .takes_value(true),
        )
        .arg(
            Arg::with_name("listen")
                .short("l")
                .long("listen")
                .value_name("ADDRESS")
                .help("Listen address")
                .multiple(true)
                .takes_value(true),
        )
        .arg(
            Arg::with_name("local")
                .short("L")
                .long("local")
                .help("Launch as local mode")
                .multiple(false)
                .conflicts_with("remote"),
        )
        .arg(
            Arg::with_name("remote")
                .short("R")
                .long("remote")
                .help("Launch as remote mode")
                .multiple(false)
                .conflicts_with("local"),
        )
        .arg(
            Arg::with_name("log")
                .long("log")
                .help("Log destination")
                .default_value("console")
                .multiple(true),
        )
        .arg(
            Arg::with_name("debug")
                .short("d")
                .long("debug")
                .help("Enable debug mode")
                .multiple(false)
                .multiple(false),
        )
        .get_matches();

    let logs: Vec<_> = matches.values_of("log").unwrap().collect();
    let mut loggers: Vec<Box<simplelog::SharedLogger>> = Vec::new();
    let mut log_level = LevelFilter::Info;
    if matches.occurrences_of("debug") == 1 {
        log_level = LevelFilter::Debug;
    }
    for log in logs.iter() {
        if log.to_lowercase() == "console" {
            loggers.push(TermLogger::new(log_level, LogConfig::default()).unwrap());
        } else {
            loggers.push(WriteLogger::new(
                log_level,
                LogConfig::default(),
                File::create(log).unwrap(),
            ));
        }
    }
    //let mut loggers:Vec<Box<SharedLogger>;

    CombinedLogger::init(loggers).unwrap();

    // test();

    // error!("Bright red error");
    // info!("This only appears in the log file");
    // debug!("This level is currently not enabled for any logger");

    // if let Some(c) = matches.value_of("local") {
    //     println!("Value for local: {}", matches.value_of("local"));
    // }

    // You can see how many times a particular flag or argument occurred
    // Note, only flags can have multiple occurrences
    let listens: Vec<_> = matches.values_of("listen").unwrap().collect();

    match matches.occurrences_of("local") {
        0 => info!("local mode is off"),
        1 => {
            info!("local mode is on");
            proxy::local::start_local_server(listens[0]);
        }
        _ => info!("Don't be crazy"),
    }

    let data = r#"
        {
            "listen": ":48100"
        }"#;
    let p: Config = serde_json::from_str(data).unwrap();

    // Do things just like with any other Rust data structure.
    info!("Please call  {}", p.listen);
}
