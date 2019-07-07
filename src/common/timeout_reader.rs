use bytes::{Buf, BufMut};
use futures::{Async, Future, Poll};
use std::io::{self, Read, Write};
use std::sync::Arc;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tokio_io::{AsyncRead, AsyncWrite};
use tokio_timer::Delay;

#[derive(Debug)]
struct TimeoutState {
    timeout: Option<Duration>,
    cur: Arc<Mutex<Delay>>,
    active: bool,
}

impl TimeoutState {
    #[inline]
    fn new(d: &Arc<Mutex<Delay>>) -> TimeoutState {
        TimeoutState {
            timeout: None,
            //cur: Delay::new(Instant::now()),
            cur: d.clone(),
            active: false,
        }
    }

    #[inline]
    fn timeout(&self) -> Option<Duration> {
        self.timeout
    }

    #[inline]
    fn set_timeout(&mut self, timeout: Option<Duration>) {
        self.timeout = timeout;
        self.reset();
    }

    #[inline]
    fn reset(&mut self) {
        if self.active {
            self.active = false;
            self.cur.lock().unwrap().reset(Instant::now());
        }
    }

    #[inline]
    fn check(&mut self) -> io::Result<()> {
        let timeout = match self.timeout {
            Some(timeout) => timeout,
            None => return Ok(()),
        };

        if !self.active {
            self.cur.lock().unwrap().reset(Instant::now() + timeout);
            self.active = true;
        }

        if self
            .cur
            .lock()
            .unwrap()
            .poll()
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?
            .is_ready()
        {
            Err(io::Error::from(io::ErrorKind::TimedOut))
        } else {
            Ok(())
        }
    }
}

/// An `AsyncRead`er which applies a timeout to read operations.
#[derive(Debug)]
pub struct RelayTimeoutReader<R> {
    reader: R,
    state: TimeoutState,
}

impl<R> RelayTimeoutReader<R>
where
    R: AsyncRead,
{
    /// Returns a new `TimeoutReader` wrapping the specified reader.
    ///
    /// There is initially no timeout.
    pub fn new(reader: R, d: &Arc<Mutex<Delay>>) -> RelayTimeoutReader<R> {
        RelayTimeoutReader {
            reader,
            state: TimeoutState::new(d),
        }
    }

    /// Returns the current read timeout.
    pub fn timeout(&self) -> Option<Duration> {
        self.state.timeout()
    }

    /// Sets the read timeout.
    ///
    /// This will reset any pending timeout.
    pub fn set_timeout(&mut self, timeout: Option<Duration>) {
        self.state.set_timeout(timeout);
    }

    /// Returns a shared reference to the inner reader.
    pub fn get_ref(&self) -> &R {
        &self.reader
    }

    /// Returns a mutable reference to the inner reader.
    pub fn get_mut(&mut self) -> &mut R {
        &mut self.reader
    }

    /// Consumes the `TimeoutReader`, returning the inner reader.
    pub fn into_inner(self) -> R {
        self.reader
    }
}

impl<R> Read for RelayTimeoutReader<R>
where
    R: AsyncRead,
{
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let r = self.reader.read(buf);
        match r {
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => self.state.check()?,
            _ => self.state.reset(),
        }
        r
    }
}

impl<R> AsyncRead for RelayTimeoutReader<R>
where
    R: AsyncRead,
{
    unsafe fn prepare_uninitialized_buffer(&self, buf: &mut [u8]) -> bool {
        self.reader.prepare_uninitialized_buffer(buf)
    }

    fn read_buf<B: BufMut>(&mut self, buf: &mut B) -> Poll<usize, io::Error> {
        let r = self.reader.read_buf(buf);
        match r {
            Ok(Async::NotReady) => self.state.check()?,
            _ => self.state.reset(),
        }
        r
    }
}
