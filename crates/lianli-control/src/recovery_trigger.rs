use anyhow::{ensure, Context, Result};
use std::collections::HashMap;
use std::time::{Duration, Instant};

pub(crate) const UNIT: &str = "lianli-control-recovery.service";
const WAIT_LIMIT: Duration = Duration::from_secs(15 * 60);

#[derive(Clone, Debug)]
struct State {
    busy: bool,
    invocation: Option<String>,
}

fn parse(text: &str) -> Result<State> {
    let mut fields = HashMap::new();
    for line in text.lines() {
        let (key, value) = line
            .split_once('=')
            .context("Invalid recovery service observation")?;
        ensure!(
            fields.insert(key, value).is_none(),
            "Duplicate recovery service property"
        );
    }
    ensure!(
        fields.len() == 4
            && fields.get("Id") == Some(&UNIT)
            && fields.get("LoadState") == Some(&"loaded"),
        "Install and reload the native recovery unit before requesting recovery"
    );
    let busy = match fields.get("ActiveState").copied() {
        Some("inactive" | "failed") => false,
        Some("activating" | "active" | "deactivating" | "reloading") => true,
        _ => anyhow::bail!("Recovery service has an unrecognized state. Inspect its journal"),
    };
    let value = fields
        .get("InvocationID")
        .context("Recovery invocation is unavailable")?;
    let invocation = if value.is_empty() || *value == "00000000000000000000000000000000" {
        None
    } else {
        Some(lianli_shared::daemon::parse_service_invocation(value).map_err(anyhow::Error::msg)?)
    };
    ensure!(
        !busy || invocation.is_some(),
        "The running recovery service has no verifiable invocation identity"
    );
    Ok(State { busy, invocation })
}

trait Backend {
    fn inspect(&mut self) -> Result<State>;
    fn submit(&mut self) -> Result<()>;
    fn elapsed(&self) -> Duration;
    fn pause(&mut self);
}

fn run(backend: &mut impl Backend) -> Result<()> {
    let original = backend.inspect()?;
    if !original.busy {
        return backend.submit();
    }
    while backend.elapsed() < WAIT_LIMIT {
        backend.pause();
        let current = backend.inspect()?;
        if current.invocation.is_some() && current.invocation != original.invocation {
            return Ok(());
        }
        if !current.busy {
            return backend.submit();
        }
    }
    anyhow::bail!("The previous recovery is still running after fifteen minutes. Inspect service progress before requesting recovery again. No competing recovery was submitted")
}

pub(crate) fn trigger() -> Result<()> {
    run(&mut Native {
        began: Instant::now(),
    })
}

struct Native {
    began: Instant,
}

impl Backend for Native {
    fn inspect(&mut self) -> Result<State> {
        let output = crate::services::Route::Native.output(
            "/usr/bin/systemctl",
            &[
                "--system",
                "--no-ask-password",
                "--no-pager",
                "show",
                "--all",
                "--property=Id,LoadState,ActiveState,InvocationID",
                UNIT,
            ],
        )?;
        ensure!(
            output.status.success(),
            "Cannot inspect recovery before submission: {}",
            output.stderr.trim()
        );
        parse(&output.stdout)
    }

    fn submit(&mut self) -> Result<()> {
        let output = crate::services::Route::Native.output("/usr/bin/systemctl", &[
            "--system", "--no-ask-password", "--no-pager", "--no-block", "--job-mode=fail", "start", UNIT,
        ]).context("Automatic recovery submission was not confirmed. Check service progress without replaying the request")?;
        ensure!(output.status.success(), "Cannot request pending switch recovery: {}. Recheck the installed recovery unit and Polkit rule", output.stderr.trim());
        Ok(())
    }

    fn elapsed(&self) -> Duration {
        self.began.elapsed()
    }
    fn pause(&mut self) {
        std::thread::sleep(Duration::from_secs(5));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    #[test]
    fn observes_only_known_recovery_states_and_real_invocations() {
        let state = |active, invocation| {
            format!(
                "Id={UNIT}\nLoadState=loaded\nActiveState={active}\nInvocationID={invocation}\n"
            )
        };
        let id = "123456789abcdef0123456789abcdef0";
        for active in ["active", "activating", "deactivating", "reloading"] {
            assert!(parse(&state(active, id)).unwrap().busy);
            assert!(parse(&state(active, "")).is_err());
        }
        for active in ["inactive", "failed"] {
            assert!(!parse(&state(active, "")).unwrap().busy);
            assert!(parse(&state(active, "00000000000000000000000000000000"))
                .unwrap()
                .invocation
                .is_none());
            assert!(!parse(&state(active, id)).unwrap().busy);
        }
        for invalid in [
            state("active", "00000000000000000000000000000000"),
            state("unknown", id),
            state("active", "invalid"),
            state("active", id).replace(UNIT, "unrelated.service"),
            state("active", id).replace("loaded", "not-found"),
            format!("{}ActiveState=inactive\n", state("active", id)),
        ] {
            assert!(parse(&invalid).is_err());
        }
    }

    struct Fixture {
        states: VecDeque<State>,
        submitted: usize,
        elapsed: Duration,
        fail_inspection: bool,
        fail_submission: bool,
    }

    impl Backend for Fixture {
        fn inspect(&mut self) -> Result<State> {
            ensure!(!self.fail_inspection, "Fixture observation failed");
            if self.states.len() > 1 {
                Ok(self.states.pop_front().unwrap())
            } else {
                Ok(self.states.front().unwrap().clone())
            }
        }
        fn submit(&mut self) -> Result<()> {
            self.submitted += 1;
            ensure!(!self.fail_submission, "Fixture acknowledgement lost");
            Ok(())
        }
        fn elapsed(&self) -> Duration {
            self.elapsed
        }
        fn pause(&mut self) {
            self.elapsed += Duration::from_secs(60);
        }
    }

    fn fixture(states: &[(bool, Option<&str>)]) -> Fixture {
        Fixture {
            states: states
                .iter()
                .map(|(busy, id)| State {
                    busy: *busy,
                    invocation: id.map(str::to_string),
                })
                .collect(),
            submitted: 0,
            elapsed: Duration::ZERO,
            fail_inspection: false,
            fail_submission: false,
        }
    }

    #[test]
    fn login_waits_for_the_existing_attempt_then_submits_once() {
        let mut waiting = fixture(&[
            (true, Some("old")),
            (true, Some("old")),
            (false, Some("old")),
        ]);
        run(&mut waiting).unwrap();
        assert_eq!(waiting.submitted, 1);
        assert_eq!(waiting.elapsed, Duration::from_secs(120));
        for terminal in [Some("old"), None] {
            let mut collected = fixture(&[(true, Some("old")), (false, terminal)]);
            run(&mut collected).unwrap();
            assert_eq!(collected.submitted, 1);
        }
        for busy in [false, true] {
            let mut newer = fixture(&[(true, Some("old")), (busy, Some("new"))]);
            run(&mut newer).unwrap();
            assert_eq!(newer.submitted, 0);
        }
    }

    #[test]
    fn bounded_observation_and_failed_submission_never_replay_a_request() {
        let mut waiting = fixture(&[(true, Some("old"))]);
        assert!(run(&mut waiting).is_err());
        assert_eq!(waiting.elapsed, WAIT_LIMIT);
        assert_eq!(waiting.submitted, 0);
        let mut failed = fixture(&[(false, None)]);
        failed.fail_inspection = true;
        assert!(run(&mut failed).is_err());
        assert_eq!(failed.submitted, 0);
        failed.fail_inspection = false;
        failed.fail_submission = true;
        assert!(run(&mut failed).is_err());
        assert_eq!(failed.submitted, 1);
    }
}
