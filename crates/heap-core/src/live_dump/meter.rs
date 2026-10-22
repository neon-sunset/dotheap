use super::Status;
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

#[derive(Clone)]
struct Counts {
    phase: String,
    total: u64,
    done: u64,
    written: u64,
    started: Instant,
}

pub(super) struct Meter {
    counts: Mutex<Counts>,
    stopped: Mutex<bool>,
    changed: Condvar,
}

impl Meter {
    pub fn new() -> Self {
        Self {
            counts: Mutex::new(Counts {
                phase: String::new(),
                total: 0,
                done: 0,
                written: 0,
                started: Instant::now(),
            }),
            stopped: Mutex::new(false),
            changed: Condvar::new(),
        }
    }

    pub fn start(&self, phase: &str, total: u64) {
        let mut counts = self.counts.lock().unwrap();
        counts.phase = phase.into();
        counts.total = total;
        counts.done = 0;
        counts.started = Instant::now();
    }

    pub fn done(&self, bytes: u64) {
        self.counts.lock().unwrap().done += bytes;
    }
    pub fn written(&self, bytes: u64) {
        self.counts.lock().unwrap().written += bytes;
    }

    pub fn stop(&self) {
        *self.stopped.lock().unwrap() = true;
        self.changed.notify_all();
    }

    pub fn report(&self, status: &(dyn Fn(Status) + Sync)) {
        let mut previous = self.counts.lock().unwrap().clone();
        let mut at = Instant::now();
        loop {
            let stopped = self.stopped.lock().unwrap();
            let (stopped, _) = self
                .changed
                .wait_timeout_while(stopped, Duration::from_millis(500), |s| !*s)
                .unwrap();
            if *stopped {
                return;
            }
            drop(stopped);
            let current = self.counts.lock().unwrap().clone();
            let now = Instant::now();
            let interval = now.duration_since(at).as_secs_f64().max(0.001);
            let same_phase = previous.started == current.started;
            status(Status {
                phase: current.phase.clone(),
                done: current.done,
                total: current.total,
                done_per_second: (current.done - if same_phase { previous.done } else { 0 }) as f64
                    / interval,
                written_per_second: (current.written - previous.written) as f64 / interval,
                seconds: now.duration_since(current.started).as_secs_f64(),
            });
            previous = current;
            at = now;
        }
    }
}
