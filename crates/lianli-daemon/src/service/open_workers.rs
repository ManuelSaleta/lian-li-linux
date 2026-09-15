use anyhow::{ensure, Context, Result};
use std::collections::HashMap;
use std::thread::{self, JoinHandle};

const MAX_OPEN_WORKERS: usize = 32;

#[derive(Default)]
pub(super) struct OpenWorkers {
    pending: HashMap<String, JoinHandle<()>>,
}

impl OpenWorkers {
    pub fn spawn(&mut self, id: String, open: impl FnOnce() + Send + 'static) -> Result<()> {
        ensure!(
            !self.pending.contains_key(&id),
            "An earlier device open is still outstanding"
        );
        ensure!(
            self.pending.len() < MAX_OPEN_WORKERS,
            "Device open worker limit reached"
        );
        let worker = thread::Builder::new()
            .name("device-open".into())
            .spawn(open)
            .context("Starting device open worker")?;
        self.pending.insert(id, worker);
        Ok(())
    }

    pub fn reap_finished(&mut self) {
        let finished: Vec<_> = self
            .pending
            .iter()
            .filter(|(_, worker)| worker.is_finished())
            .map(|(id, _)| id.clone())
            .collect();
        for id in finished {
            if let Some(worker) = self.pending.remove(&id) {
                Self::join(&id, worker);
            }
        }
    }

    pub fn finish(&mut self) {
        // Cutting a HydroShift II transaction short can wedge its MCU until a power cycle.
        for (id, worker) in self.pending.drain() {
            Self::join(&id, worker);
        }
    }

    fn join(id: &str, worker: JoinHandle<()>) {
        if worker.join().is_err() {
            tracing::warn!("Device open worker for {id} panicked");
        }
    }
}

impl Drop for OpenWorkers {
    fn drop(&mut self) {
        self.finish();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    #[test]
    fn outstanding_opens_cannot_be_replaced_or_detached_during_shutdown() {
        let mut workers = OpenWorkers::default();
        let (release, wait) = mpsc::channel();
        workers
            .spawn("device".into(), move || {
                let _ = wait.recv();
            })
            .unwrap();
        assert!(workers.spawn("device".into(), || {}).is_err());
        workers.reap_finished();
        assert!(workers.spawn("device".into(), || {}).is_err());
        let (entered, ready) = mpsc::channel();
        let (complete, done) = mpsc::channel();
        let shutdown = thread::spawn(move || {
            entered.send(()).unwrap();
            drop(workers);
            complete.send(()).unwrap();
        });
        ready.recv_timeout(Duration::from_secs(1)).unwrap();
        let premature = done.recv_timeout(Duration::from_millis(50));
        release.send(()).unwrap();
        shutdown.join().unwrap();
        assert!(matches!(premature, Err(mpsc::RecvTimeoutError::Timeout)));
        done.recv_timeout(Duration::from_secs(1)).unwrap();
    }

    #[test]
    fn joined_workers_allow_a_later_retry() {
        let mut workers = OpenWorkers::default();
        workers.spawn("device".into(), || {}).unwrap();
        workers.finish();
        assert!(workers.spawn("device".into(), || {}).is_ok());
    }
}
