#![feature(drain_filter)]
#![feature(pattern)]
#![feature(trait_alias)]

#[macro_use]
extern crate log;
extern crate bincode;
extern crate byteorder;
extern crate bytes;
#[macro_use]
extern crate clap;
extern crate crc;
#[macro_use]
extern crate futures;
extern crate flexi_logger;
extern crate httparse;
extern crate serde;

extern crate nix;
extern crate nom;
//extern crate orion;
extern crate rand;
extern crate ring;

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
mod stat;

use clap::{App, Arg};
use futures::future::{self};
use futures::prelude::*;
use std::fs::File;
use std::time::Duration;

use tokio::runtime::Runtime;
use tokio::timer::Interval;

use flexi_logger::{opt_format, LogTarget, Logger};
// use simplelog::Config as LogConfig;
// use simplelog::{CombinedLogger, LevelFilter, TermLogger, TerminalMode, WriteLogger};

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
                .takes_value(false)
                .help("Launch as local mode")
                .multiple(false)
                .conflicts_with("remote"),
        )
        .arg(
            Arg::with_name("remote")
                .short("R")
                .long("remote")
                .takes_value(false)
                .help("Launch as remote mode")
                .multiple(false)
                .conflicts_with("local"),
        )
        .arg(
            Arg::with_name("logdir")
                .long("logdir")
                .takes_value(true)
                .help("log dir destination")
                .multiple(false),
        )
        .arg(
            Arg::with_name("logtostderr")
                .long("logtostderr")
                .help("log to stderr")
                .takes_value(false)
                .multiple(false),
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
        .arg(
            Arg::with_name("period_dump")
                .long("period_dump")
                .help("Period dump log secs")
                .default_value("30")
                .multiple(false),
        )
        .get_matches();

    //let logs: Vec<_> = matches.values_of("log").unwrap().collect();

    //let mut loggers: Vec<Box<dyn simplelog::SharedLogger>> = Vec::new();
    let mut log_level = String::from("info");
    if matches.occurrences_of("debug") == 1 {
        log_level = String::from("debug");
    }
    let mut logger = flexi_logger::Logger::with_str(log_level.as_str());
    match matches.value_of("logdir") {
        None => {
            logger = logger.log_target(LogTarget::DevNull);
        }
        Some(dir) => {
            logger = logger
                .log_to_file()
                .rotate(
                    flexi_logger::Criterion::Size(1024 * 1024),
                    flexi_logger::Naming::Numbers,
                    flexi_logger::Cleanup::KeepLogFiles(10),
                )
                .directory(dir)
                .format(opt_format);
        }
    }
    if matches.occurrences_of("logtostderr") == 1 {
        logger = logger
            .duplicate_to_stderr(flexi_logger::Duplicate::Info)
            .format_for_stderr(flexi_logger::colored_opt_format);
    }

    logger.start().unwrap();
    //CombinedLogger::init(loggers).unwrap();
    config::set_local_transparent(matches.is_present("transparent"));

    let period_dump_secs =
        value_t!(matches.value_of("period_dump"), u64).unwrap_or_else(|e| e.exit());

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
                    mux::init_remote_mux_server(&laddr);
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
                mux::init_local_mux_channels(&config::get_config().lock().unwrap().local.channels);
                Ok(())
            }));
        }
    }

    let interval = Interval::new_interval(Duration::from_secs(period_dump_secs));
    let routine = interval
        .for_each(|_| {
            stat::dump_stat();
            Ok(())
        })
        .map_err(|e| {
            error!("routine task error:{}", e);
        });
    rt.spawn(routine);

    // let data = r#"
    //     {
    //         "listen": ":48100"
    //     }"#;
    // let p: Config = serde_json::from_str(data).unwrap();

    // // Do things just like with any other Rust data structure.
    // info!("Please call  {}", p.listen);

    rt.shutdown_on_idle().wait().unwrap();
}
