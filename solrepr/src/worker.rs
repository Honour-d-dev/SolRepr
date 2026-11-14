use std::{thread, time::Duration};

use crossbeam_channel::{Receiver, Sender, unbounded};


/// Spawns new threads with a designated Job.
pub(crate) struct Worker<J: Job> {
    tx: Sender<J::Req>,
    pub(crate) rx: Receiver<J::Res>
}

impl<J: Job> Worker<J> {
    pub(crate) fn spawn(name: &str, size_mb: usize, job: J) -> Self {
        let (tx_request, rx_request) = unbounded();
        let (tx_result, rx_result) = unbounded();
        thread::Builder::new()
            .name(format!("solrepr-{name}"))
            .stack_size(size_mb * 1024 * 1024)
            .spawn(move || job.run(rx_request, tx_result))
            .expect("failed to spawn worker");
        Self {
            tx: tx_request,
            rx: rx_result,
        }
    }

    pub(crate) fn request(&self, request: J::Req) {
        let _ = self.tx.send(request);
    }
 
    /// Stop listening: the thread is gone, so selecting on its channel would
    /// otherwise return immediately on every loop turn.
    pub(crate) fn stop(&mut self) {
        self.rx = crossbeam_channel::never();
    }
}


/// A job that runs on its own thread. Reads requests from `rx` and answers on `tx`.
pub(crate) trait Job: Send + 'static {
    type Req: Send + 'static;
    type Res: Send + 'static;
    fn run(self, rx: Receiver<Self::Req>, tx: Sender<Self::Res>);
}

/// Collapses a burst of requests until the channel goes quiet for `timeout`,
/// combining each arrival into the running value. `combine` decides what
/// collapsing means
pub(crate) fn debounce<T>(rx: &Receiver<T>, mut latest: T, timeout: Duration, combine: impl Fn(T, T) -> T) -> T {
    while let Ok(newer) = rx.recv_timeout(timeout) {
        latest = combine(latest, newer);
    }
    latest
}

/// Collapses the queued requests, combining each into the running value.
pub(crate) fn coalesce<T>(rx: &Receiver<T>, combine: impl Fn(T, T) -> T) -> Option<T> {
    let mut latest = None;
    while let Ok(newer) = rx.try_recv() {
        latest = Some(match latest {
            Some(latest) => combine(latest, newer),
            None => newer,
        });
    }
    latest
}



