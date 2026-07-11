use std::time::Duration;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CoordinatorState {
    Starting,
    Healthy,
    Degraded,
    Draining,
    Stopped,
}

#[derive(Debug)]
pub struct CoordinatorHealth {
    state: CoordinatorState,
    consecutive_failures: u32,
}

impl CoordinatorHealth {
    pub fn starting() -> Self {
        Self {
            state: CoordinatorState::Starting,
            consecutive_failures: 0,
        }
    }

    pub fn state(&self) -> CoordinatorState {
        self.state
    }

    pub fn record_success(&mut self) {
        if !matches!(
            self.state,
            CoordinatorState::Draining | CoordinatorState::Stopped
        ) {
            self.state = CoordinatorState::Healthy;
        }
        self.consecutive_failures = 0;
    }

    pub fn record_transient_failure(&mut self) -> Duration {
        if !matches!(
            self.state,
            CoordinatorState::Draining | CoordinatorState::Stopped
        ) {
            self.state = CoordinatorState::Degraded;
        }
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        let exponent = self.consecutive_failures.saturating_sub(1).min(6);
        Duration::from_millis(250 * (1u64 << exponent))
    }

    pub fn start_draining(&mut self) {
        if self.state != CoordinatorState::Stopped {
            self.state = CoordinatorState::Draining;
        }
    }

    pub fn stop(&mut self) {
        self.state = CoordinatorState::Stopped;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transient_failures_degrade_and_back_off() {
        let mut health = CoordinatorHealth::starting();
        health.record_success();
        assert_eq!(health.state(), CoordinatorState::Healthy);
        assert_eq!(
            health.record_transient_failure(),
            Duration::from_millis(250)
        );
        assert_eq!(health.state(), CoordinatorState::Degraded);
        assert_eq!(
            health.record_transient_failure(),
            Duration::from_millis(500)
        );
        health.record_success();
        assert_eq!(health.state(), CoordinatorState::Healthy);
    }

    #[test]
    fn draining_is_not_reversed_by_late_success() {
        let mut health = CoordinatorHealth::starting();
        health.start_draining();
        health.record_success();
        assert_eq!(health.state(), CoordinatorState::Draining);
        health.stop();
        assert_eq!(health.state(), CoordinatorState::Stopped);
    }
}
