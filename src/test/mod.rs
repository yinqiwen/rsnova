use futures::future::lazy;
use futures::try_ready;
use std::str;

use tokio::io::Error as TokioIOError;
use tokio::net::TcpListener;

use std::io::ErrorKind;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::prelude::*;
use tokio::timer::Interval;

use tokio_sync::semaphore::{Permit, Semaphore};

pub struct TestFuture {
    counter: u32,
    interval: Interval,
}

impl TestFuture {
    pub fn new() -> Self {
        Self {
            counter: 0,
            interval: Interval::new_interval(Duration::from_secs(3)),
        }
    }
    pub fn teset(self) -> String {
        return String::from("a");
    }
}

impl Stream for TestFuture {
    type Item = u32;
    type Error = ();

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        info!("poll invoked.");
        try_ready!(self
            .interval
            .poll()
            // The interval can fail if the Tokio runtime is unavailable.
            // In this example, the error is ignored.
            .map_err(|_| ()));
        self.counter = self.counter + 1;
        Ok(Async::Ready(Some(self.counter)))
    }
}

fn t2() {
    let f = TestFuture::new();
    let s1 = f.teset();
    //let s2 = f.teset();
}

fn t1() {
    let s = Semaphore::new(2);

    let acuire = |sem| {
        let mut p = Permit::new();
        let r = p.poll_acquire(sem);
        match r {
            Err(e) => error!("failed to acuire:{}", e),
            Ok(Async::NotReady) => {
                error!("failed to acuire");
            }
            _ => {
                info!("acuire success:{}", sem.available_permits());
            }
        }
    };

    acuire(&s);
    acuire(&s);
    //s.add_permits(1);
    acuire(&s);
}

pub fn start() {
    let f = TestFuture::new();
    // let r = f.into_stream();
    let x = f
        .for_each(|v| {
            info!("Got counter:{}", v);
            t1();
            Ok(())
        })
        .map_err(|e| {});
    tokio::run(x);
    info!("after run");
}
