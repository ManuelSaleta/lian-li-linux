use std::sync::mpsc::{self, SyncSender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

pub(super) struct SourceRetirement<T> {
    sender: Option<SyncSender<T>>,
    worker: Option<JoinHandle<()>>,
}

impl<T: Send + 'static> SourceRetirement<T> {
    pub fn new() -> Self {
        let (sender, receiver) = mpsc::sync_channel(1);
        let worker = thread::spawn(move || {
            for source in receiver {
                drop(source);
            }
        });
        Self {
            sender: Some(sender),
            worker: Some(worker),
        }
    }

    pub fn try_retire(&self, source: T) -> Result<(), T> {
        self.sender
            .as_ref()
            .unwrap()
            .try_send(source)
            .map_err(|error| match error {
                mpsc::TrySendError::Full(source) | mpsc::TrySendError::Disconnected(source) => {
                    source
                }
            })
    }
}

impl<T> Drop for SourceRetirement<T> {
    fn drop(&mut self) {
        drop(self.sender.take());
        let Some(worker) = self.worker.take() else {
            return;
        };
        let deadline = Instant::now() + Duration::from_secs(2);
        while !worker.is_finished() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        if worker.is_finished() {
            if worker.join().is_err() {
                tracing::warn!("Retired media cleanup panicked");
            }
        } else {
            // Only stopped, completed sources enter this worker. File cleanup may outlive shutdown.
            tracing::warn!("Retired media is still finishing file cleanup");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slow_disposal_stays_on_one_worker_and_full_queue_returns_ownership() {
        struct Source {
            id: usize,
            dropped: mpsc::Sender<(usize, thread::ThreadId)>,
            release: Option<mpsc::Receiver<()>>,
        }
        impl Drop for Source {
            fn drop(&mut self) {
                self.dropped
                    .send((self.id, thread::current().id()))
                    .unwrap();
                if let Some(release) = self.release.take() {
                    release.recv_timeout(Duration::from_secs(2)).unwrap();
                }
            }
        }
        let retirement = SourceRetirement::new();
        let worker = retirement.worker.as_ref().unwrap().thread().id();
        let (dropped, drops) = mpsc::channel();
        let (release, released) = mpsc::channel();
        assert!(retirement
            .try_retire(Source {
                id: 1,
                dropped: dropped.clone(),
                release: Some(released)
            })
            .is_ok());
        assert_eq!(
            drops.recv_timeout(Duration::from_secs(1)).unwrap(),
            (1, worker)
        );
        assert!(retirement
            .try_retire(Source {
                id: 2,
                dropped: dropped.clone(),
                release: None
            })
            .is_ok());
        let returned = retirement
            .try_retire(Source {
                id: 3,
                dropped,
                release: None,
            })
            .err()
            .unwrap();
        assert_eq!(returned.id, 3);
        assert!(drops.try_recv().is_err());
        release.send(()).unwrap();
        drop(retirement);
        assert_eq!(drops.try_recv().unwrap(), (2, worker));
        drop(returned);
        assert_eq!(drops.try_recv().unwrap(), (3, thread::current().id()));
    }
}
