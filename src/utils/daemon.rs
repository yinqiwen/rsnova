use anyhow::{Context, Result, anyhow};
use std::fs::OpenOptions;
use std::io;
use std::os::unix::io::AsRawFd;

/// Detach from the controlling terminal (classic double-fork daemon).
pub fn daemonize(log_to_file: bool) -> Result<()> {
    if !log_to_file {
        eprintln!("warning: daemon mode without --log sends tracing to /dev/null");
    }

    unsafe {
        match libc::fork() {
            -1 => return Err(anyhow!("first fork failed: {}", io::Error::last_os_error())),
            pid if pid > 0 => libc::_exit(0),
            _ => {}
        }
    }

    if unsafe { libc::setsid() } == -1 {
        return Err(anyhow!("setsid failed: {}", io::Error::last_os_error()));
    }

    unsafe {
        match libc::fork() {
            -1 => {
                return Err(anyhow!(
                    "second fork failed: {}",
                    io::Error::last_os_error()
                ));
            }
            pid if pid > 0 => libc::_exit(0),
            _ => {}
        }
    }

    redirect_stdio_to_dev_null()
}

fn redirect_stdio_to_dev_null() -> Result<()> {
    let dev_null = OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/null")
        .context("open /dev/null")?;
    let fd = dev_null.as_raw_fd();
    for std_fd in [0, 1, 2] {
        if unsafe { libc::dup2(fd, std_fd) } == -1 {
            return Err(anyhow!(
                "dup2 to fd {} failed: {}",
                std_fd,
                io::Error::last_os_error()
            ));
        }
    }
    Ok(())
}
