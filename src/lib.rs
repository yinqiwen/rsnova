#![crate_type = "lib"]
#![crate_name = "rsnova"]

#[macro_use]
extern crate log;

#[macro_use]
extern crate lazy_static;

#[macro_use]
extern crate futures;

pub use self::config::Config;

mod channel;
pub mod config;
mod rmux;
mod tunnel;
mod utils;

use futures::FutureExt;
use std::error::Error;

pub async fn start_rsnova(cfg: config::Config) -> Result<(), Box<dyn Error>> {
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
