#![feature(drain_filter)]
#![feature(pattern)]
#![feature(trait_alias)]

#[macro_use]
extern crate log;
extern crate bincode;
extern crate byteorder;
extern crate bytes;
extern crate clap;
extern crate crc;
#[macro_use]
extern crate futures;
extern crate httparse;
extern crate serde;

extern crate nix;
extern crate nom;
//extern crate orion;
extern crate rand;
extern crate ring;
extern crate simplelog;
extern crate skip32;

extern crate tokio;
extern crate tokio_io;
extern crate tokio_io_timeout;
extern crate tokio_sync;
extern crate tokio_timer;
extern crate tokio_udp;
//extern crate trust_dns_server;
extern crate twoway;

extern crate unicase;
extern crate url;

#[macro_use]
extern crate lazy_static;

mod common;
mod config;
mod mux;
mod proxy;
mod test;

use clap::{App, Arg};
use futures::future::{self};
use futures::prelude::*;
use std::fs::File;

use tokio::runtime::Runtime;

use simplelog::Config as LogConfig;
use simplelog::{CombinedLogger, LevelFilter, TermLogger, TerminalMode, WriteLogger};

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
            Arg::with_name("server")
                .short("s")
                .long("server")
                .value_name("ADDRESS")
                .help("Server address")
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
            Arg::with_name("proxy")
                .long("proxy")
                .help("Proxy setting")
                .default_value("")
                .multiple(false),
        )
        .arg(
            Arg::with_name("transparent")
                .long("transparent")
                .help("Serving as transparent tcp proxy.")
                .multiple(false),
        )
        .arg(
            Arg::with_name("debug")
                .short("d")
                .long("debug")
                .help("Enable debug mode")
                .multiple(false),
        )
        .arg(
            Arg::with_name("key")
                .short("k")
                .long("key")
                .help("Default cipher key")
                .default_value("a3ac5c834538c8ee421610ab78f2f4c2")
                .multiple(false),
        )
        .arg(
            Arg::with_name("method")
                .short("m")
                .long("method")
                .help("Default cipher method")
                .default_value("chacha20poly1305")
                .multiple(false),
        )
        .get_matches();

    let logs: Vec<_> = matches.values_of("log").unwrap().collect();
    let mut loggers: Vec<Box<dyn simplelog::SharedLogger>> = Vec::new();
    let mut log_level = LevelFilter::Info;
    if matches.occurrences_of("debug") == 1 {
        log_level = LevelFilter::Debug;
    }
    for log in logs.iter() {
        if log.to_lowercase() == "console" {
            loggers.push(
                TermLogger::new(log_level, LogConfig::default(), TerminalMode::Mixed).unwrap(),
            );
        } else {
            loggers.push(WriteLogger::new(
                log_level,
                LogConfig::default(),
                File::create(log).unwrap(),
            ));
        }
    }
    CombinedLogger::init(loggers).unwrap();

    config::set_local_transparent(matches.is_present("transparent"));

    let mut proxy = String::new();
    if let Some(v) = matches.value_of("proxy") {
        proxy = String::from(v);
    }

    if let Some(v) = matches.value_of("key") {
        config::set_default_cipher_key(v);
    }
    if let Some(v) = matches.value_of("method") {
        config::set_default_cipher_method(v);
    }

    // Create the runtime
    let mut rt = Runtime::new().unwrap();
    // You can see how many times a particular flag or argument occurred
    // Note, only flags can have multiple occurrences
    let listens: Vec<_> = matches.values_of("listen").unwrap().collect();

    match matches.occurrences_of("local") {
        0 => {
            info!("local mode is off");
        }
        1 => {
            info!("local mode is on");
            for l in &listens {
                let laddr = String::from(*l);
                rt.spawn(future::lazy(move || {
                    proxy::local::start_local_server(laddr.as_str());
                    Ok(())
                }));
            }
        }
        _ => info!("Don't be crazy"),
    }

    match matches.occurrences_of("remote") {
        0 => {
            info!("remote mode is off");
        }
        1 => {
            info!("remote mode is on");
            for l in &listens {
                let laddr = String::from(*l);
                rt.spawn(future::lazy(move || {
                    mux::channel::init_remote_mux_server(&laddr);
                    Ok(())
                }));
            }
        }
        _ => info!("Don't be crazy"),
    }

    match matches.values_of("server") {
        None => {
            warn!("no remote server configured.");
        }
        Some(ss) => {
            let servers: Vec<_> = ss.collect();
            for s in &servers {
                config::add_channel_config(*s, proxy.as_str());
            }
            rt.spawn(future::lazy(|| {
                mux::channel::init_local_mux_channels(
                    &config::get_config().lock().unwrap().local.channels,
                );
                Ok(())
            }));
        }
    }

    // let data = r#"
    //     {
    //         "listen": ":48100"
    //     }"#;
    // let p: Config = serde_json::from_str(data).unwrap();

    // // Do things just like with any other Rust data structure.
    // info!("Please call  {}", p.listen);

    rt.shutdown_on_idle().wait().unwrap();
}
