use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};

pub(super) struct StreamingWorker {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl StreamingWorker {
    pub fn spawn(run: impl FnOnce(Arc<AtomicBool>) + Send + 'static) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = stop.clone();
        Self {
            stop,
            thread: Some(thread::spawn(move || run(worker_stop))),
        }
    }

    pub fn wake(&self) {
        if let Some(worker) = &self.thread {
            worker.thread().unpark();
        }
    }

    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.wake();
        if let Some(worker) = self.thread.take() {
            if worker.join().is_err() {
                tracing::warn!("LCD streaming worker panicked during shutdown");
            }
        }
    }
}

impl Drop for StreamingWorker {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    #[test]
    fn dropping_worker_wakes_and_joins_it_before_returning() {
        let (ready_tx, ready_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let worker = StreamingWorker::spawn(move |stop| {
            let deadline = Instant::now() + Duration::from_secs(2);
            ready_tx.send(()).unwrap();
            while !stop.load(Ordering::Acquire) && Instant::now() < deadline {
                thread::park_timeout(deadline.saturating_duration_since(Instant::now()));
            }
            done_tx.send(stop.load(Ordering::Acquire)).unwrap();
        });
        ready_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        let wake = worker.thread.as_ref().unwrap().thread().clone();
        let (dropped_tx, dropped_rx) = mpsc::channel();
        let dropper = thread::spawn(move || {
            drop(worker);
            dropped_tx.send(()).unwrap();
        });
        let result = dropped_rx.recv_timeout(Duration::from_secs(1));
        wake.unpark();
        dropper.join().unwrap();
        result.unwrap();
        assert!(done_rx.try_recv().unwrap());
    }
}
