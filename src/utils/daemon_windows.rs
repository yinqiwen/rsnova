use anyhow::Result;

/// Windows has no Unix-style daemonization in rsnova; run in the foreground.
pub fn daemonize(_log_to_file: bool) -> Result<()> {
    eprintln!("warning: --daemon is not supported on Windows; continuing in foreground");
    Ok(())
}
