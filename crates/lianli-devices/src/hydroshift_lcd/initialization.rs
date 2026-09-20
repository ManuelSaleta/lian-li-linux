use anyhow::{bail, Result};
use parking_lot::{Condvar, Mutex};
use std::time::Duration;

#[derive(Default)]
enum State {
    #[default]
    Pending,
    Running,
    Finished(Result<(), String>),
}

#[derive(Default)]
pub(super) struct Initialization {
    state: Mutex<State>,
    changed: Condvar,
}

impl Initialization {
    pub(super) fn ready(&self) -> Result<bool> {
        match &*self.state.lock() {
            State::Finished(Ok(())) => Ok(true),
            State::Finished(Err(error)) => Err(anyhow::Error::msg(error.clone())),
            _ => Ok(false),
        }
    }

    pub(super) fn run(
        &self,
        cancelled: impl Fn() -> bool,
        initialize: impl FnOnce() -> Result<()>,
    ) -> Result<()> {
        let mut state = self.state.lock();
        loop {
            if cancelled() {
                bail!("AIO initialization cancelled");
            }
            match &*state {
                State::Pending => {
                    *state = State::Running;
                    break;
                }
                State::Running => {
                    self.changed
                        .wait_for(&mut state, Duration::from_millis(100));
                }
                State::Finished(result) => return result.clone().map_err(anyhow::Error::msg),
            }
        }
        drop(state);
        let completion = Completion(self);
        let result = initialize();
        *self.state.lock() = State::Finished(
            result
                .as_ref()
                .map(|_| ())
                .map_err(|error| format!("{error:#}")),
        );
        drop(completion);
        result
    }
}

struct Completion<'a>(&'a Initialization);

impl Drop for Completion<'_> {
    fn drop(&mut self) {
        let mut state = self.0.state.lock();
        if matches!(*state, State::Running) {
            *state = State::Finished(Err("AIO initialization worker panicked".into()));
        }
        self.0.changed.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{mpsc, Arc};
    use std::thread;

    #[test]
    fn concurrent_callers_wait_for_the_same_result() {
        for fail in [false, true] {
            let initialization = Arc::new(Initialization::default());
            let (entered, started) = mpsc::channel();
            let (release, wait) = mpsc::channel();
            let owner = initialization.clone();
            let worker = thread::spawn(move || {
                owner.run(
                    || false,
                    || {
                        entered.send(()).unwrap();
                        wait.recv().unwrap();
                        anyhow::ensure!(!fail, "firmware failure");
                        Ok(())
                    },
                )
            });
            started.recv_timeout(Duration::from_secs(1)).unwrap();
            let (finished, result) = mpsc::channel();
            let waiter = initialization.clone();
            let second = thread::spawn(move || {
                finished
                    .send(waiter.run(|| false, || panic!("must initialize only once")))
                    .unwrap();
            });
            let premature = result.recv_timeout(Duration::from_millis(150));
            release.send(()).unwrap();
            assert!(matches!(premature, Err(mpsc::RecvTimeoutError::Timeout)));
            assert_eq!(worker.join().unwrap().is_err(), fail);
            assert_eq!(
                result
                    .recv_timeout(Duration::from_secs(1))
                    .unwrap()
                    .is_err(),
                fail
            );
            second.join().unwrap();
            assert_eq!(
                initialization
                    .run(|| false, || panic!("must retain result"))
                    .is_err(),
                fail
            );
        }
    }

    #[test]
    fn cancelling_a_waiter_does_not_cancel_the_owner_or_other_devices() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let initialization = Arc::new(Initialization::default());
        let owner = initialization.clone();
        let (entered, started) = mpsc::channel();
        let (release, wait) = mpsc::channel();
        let worker = thread::spawn(move || {
            owner.run(
                || false,
                || {
                    entered.send(()).unwrap();
                    wait.recv().unwrap();
                    Ok(())
                },
            )
        });
        started.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(!initialization.ready().unwrap());
        let other_device = Initialization::default();
        other_device.run(|| false, || Ok(())).unwrap();
        assert!(other_device.ready().unwrap());
        let cancelled = Arc::new(AtomicBool::new(false));
        let stop = cancelled.clone();
        let waiter = initialization.clone();
        let (checking, checked) = mpsc::channel();
        let (finished, result) = mpsc::channel();
        let second = thread::spawn(move || {
            finished
                .send(waiter.run(
                    || {
                        checking.send(()).unwrap();
                        stop.load(Ordering::Relaxed)
                    },
                    || panic!("another caller owns initialization"),
                ))
                .unwrap();
        });
        checked.recv_timeout(Duration::from_secs(1)).unwrap();
        cancelled.store(true, Ordering::Relaxed);
        let stopped = result.recv_timeout(Duration::from_secs(1));
        release.send(()).unwrap();
        worker.join().unwrap().unwrap();
        second.join().unwrap();
        assert!(stopped.unwrap().is_err());
        assert!(initialization.ready().unwrap());
    }

    #[test]
    fn cancellation_and_panic_do_not_leave_waiters_stuck() {
        let initialization = Initialization::default();
        assert!(initialization.run(|| true, || panic!("cancelled")).is_err());
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = initialization.run(|| false, || panic!("init failure"));
        }));
        assert!(panic.is_err());
        assert!(initialization
            .run(|| false, || panic!("must retain failure"))
            .unwrap_err()
            .to_string()
            .contains("panicked"));
    }
}
