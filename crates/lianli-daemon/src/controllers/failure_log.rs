use std::collections::HashMap;
use std::fmt::Display;
use std::time::{Duration, Instant};

const REPORT_INTERVAL: Duration = Duration::from_secs(30);
const MAX_DEVICES: usize = 128;

struct Failure {
    reported_at: Instant,
    seen_at: Instant,
    suppressed: u64,
}

#[derive(Default)]
pub(super) struct FailureLog {
    devices: HashMap<String, HashMap<&'static str, Failure>>,
    overflow_reported: Option<Instant>,
    cleaned_at: Option<Instant>,
}

impl FailureLog {
    pub(super) fn record<E: Display>(
        &mut self,
        device: &str,
        operation: &'static str,
        result: Result<(), E>,
    ) {
        let now = Instant::now();
        if self
            .cleaned_at
            .is_none_or(|at| now.duration_since(at) >= REPORT_INTERVAL)
        {
            self.devices.retain(|_, operations| {
                operations.retain(|_, state| {
                    now.duration_since(state.seen_at) < Duration::from_secs(120)
                });
                !operations.is_empty()
            });
            self.cleaned_at = Some(now);
        }
        match result {
            Ok(()) => {
                if let Some(operations) = self.devices.get_mut(device) {
                    if operations.remove(operation).is_some() {
                        tracing::info!(device, operation, "Cooling operation recovered");
                    }
                    if operations.is_empty() {
                        self.devices.remove(device);
                    }
                }
            }
            Err(error) => {
                if !self.devices.contains_key(device) && self.devices.len() >= MAX_DEVICES {
                    if self
                        .overflow_reported
                        .is_none_or(|at| now.duration_since(at) >= REPORT_INTERVAL)
                    {
                        tracing::warn!(device, operation, error = %format_args!("{error:#}"), "Additional cooling failures; tracking capacity reached");
                        self.overflow_reported = Some(now);
                    }
                    return;
                }
                if !self.devices.contains_key(device) {
                    self.devices.insert(device.to_owned(), HashMap::new());
                }
                let operations = self.devices.get_mut(device).unwrap();
                match operations.get_mut(operation) {
                    Some(state) => {
                        if let Some(suppressed) = state.repeat(now) {
                            tracing::warn!(device, operation, error = %format_args!("{error:#}"), suppressed, "Cooling operation still failing");
                        }
                    }
                    None => {
                        tracing::warn!(device, operation, error = %format_args!("{error:#}"), "Cooling operation failed");
                        operations.insert(
                            operation,
                            Failure {
                                reported_at: now,
                                seen_at: now,
                                suppressed: 0,
                            },
                        );
                    }
                }
            }
        }
    }
}

impl Failure {
    fn repeat(&mut self, now: Instant) -> Option<u64> {
        self.seen_at = now;
        if now.duration_since(self.reported_at) < REPORT_INTERVAL {
            self.suppressed = self.suppressed.saturating_add(1);
            return None;
        }
        self.reported_at = now;
        Some(std::mem::take(&mut self.suppressed))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repeat_failures_are_summarized_at_a_bounded_cadence() {
        let now = Instant::now();
        let mut state = Failure {
            reported_at: now,
            seen_at: now,
            suppressed: 0,
        };
        for second in 1..30 {
            assert_eq!(state.repeat(now + Duration::from_secs(second)), None);
        }
        assert_eq!(state.repeat(now + REPORT_INTERVAL), Some(29));
        assert_eq!(state.repeat(now + REPORT_INTERVAL), None);
    }

    #[test]
    fn recovery_clears_only_the_failed_operation_and_storage_is_bounded() {
        let mut log = FailureLog::default();
        log.record("pump", "set speed", Err("disconnected"));
        log.record("pump", "read temperature", Err("timeout"));
        log.record::<&str>("pump", "set speed", Ok(()));
        assert_eq!(log.devices["pump"].len(), 1);
        log.record::<&str>("pump", "read temperature", Ok(()));
        assert!(log.devices.is_empty());
        for index in 0..MAX_DEVICES + 10 {
            log.record(&index.to_string(), "set speed", Err("unavailable"));
        }
        assert_eq!(log.devices.len(), MAX_DEVICES);
    }
}
