use std::sync::{
    Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

pub const DEFAULT_MAX_CONSECUTIVE_INJECTIONS: u32 = 200;
pub const DEFAULT_MIN_INJECTION_INTERVAL_MS: u64 = 20;
pub const DEFAULT_SETTLE_MS: i64 = 150;
pub const MAX_SETTLE_MS: i64 = 10_000;

pub fn settle_duration(value: Option<i64>) -> Result<Duration, String> {
    let milliseconds = value.unwrap_or(DEFAULT_SETTLE_MS);
    if !(0..=MAX_SETTLE_MS).contains(&milliseconds) {
        return Err(format!(
            "settle_ms must be between 0 and {MAX_SETTLE_MS} milliseconds"
        ));
    }
    Ok(Duration::from_millis(milliseconds as u64))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BudgetError {
    CircuitBreakerTripped { limit: u32 },
    Stopped,
}

impl std::fmt::Display for BudgetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CircuitBreakerTripped { limit } => write!(
                f,
                "input injection circuit breaker tripped after {limit} injections without a successful screenshot; call screenshot before injecting again"
            ),
            Self::Stopped => write!(f, "input injection stopped after SIGINT"),
        }
    }
}

impl std::error::Error for BudgetError {}

#[derive(Debug, Default)]
struct State {
    consecutive_injections: u32,
    last_injection: Option<Instant>,
}

#[derive(Debug)]
pub struct Budget {
    state: Mutex<State>,
    stopped: AtomicBool,
    max_consecutive_injections: u32,
    min_interval: Duration,
}

impl Budget {
    pub fn new() -> Self {
        Self::with_config(
            DEFAULT_MAX_CONSECUTIVE_INJECTIONS,
            Duration::from_millis(DEFAULT_MIN_INJECTION_INTERVAL_MS),
        )
    }

    #[allow(dead_code)]
    pub fn with_config(max_consecutive_injections: u32, min_interval: Duration) -> Self {
        Self {
            state: Mutex::new(State::default()),
            stopped: AtomicBool::new(false),
            max_consecutive_injections,
            min_interval,
        }
    }

    pub async fn before_injection(&self) -> Result<(), BudgetError> {
        loop {
            if self.stopped.load(Ordering::SeqCst) {
                return Err(BudgetError::Stopped);
            }

            let wait = {
                let state = self
                    .state
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                if state.consecutive_injections >= self.max_consecutive_injections {
                    return Err(BudgetError::CircuitBreakerTripped {
                        limit: self.max_consecutive_injections,
                    });
                }
                state.last_injection.and_then(|last| {
                    let elapsed = last.elapsed();
                    if elapsed < self.min_interval {
                        Some(self.min_interval - elapsed)
                    } else {
                        None
                    }
                })
            };

            if let Some(wait) = wait {
                tokio::time::sleep(wait).await;
                continue;
            }

            let state = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if self.stopped.load(Ordering::SeqCst) {
                return Err(BudgetError::Stopped);
            }
            if state.consecutive_injections >= self.max_consecutive_injections {
                return Err(BudgetError::CircuitBreakerTripped {
                    limit: self.max_consecutive_injections,
                });
            }
            if state
                .last_injection
                .is_some_and(|last| last.elapsed() < self.min_interval)
            {
                continue;
            }

            return Ok(());
        }
    }

    pub fn injection_succeeded(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.consecutive_injections = state.consecutive_injections.saturating_add(1);
        state.last_injection = Some(Instant::now());
    }

    pub fn screenshot_completed(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.consecutive_injections = 0;
    }

    pub fn stop(&self) {
        self.stopped.store(true, Ordering::SeqCst);
    }

    #[allow(dead_code)]
    pub fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::SeqCst)
    }

    #[allow(dead_code)]
    pub fn consecutive_injections(&self) -> u32 {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .consecutive_injections
    }
}

#[cfg(test)]
mod tests {
    use super::{Budget, BudgetError, DEFAULT_SETTLE_MS, MAX_SETTLE_MS, settle_duration};
    use std::time::Duration;

    #[tokio::test]
    async fn circuit_breaker_trips_at_limit_and_resets_after_screenshot() {
        let budget = Budget::with_config(3, Duration::ZERO);

        for _ in 0..3 {
            budget.before_injection().await.unwrap();
            budget.injection_succeeded();
        }
        assert_eq!(budget.consecutive_injections(), 3);
        assert_eq!(
            budget.before_injection().await,
            Err(BudgetError::CircuitBreakerTripped { limit: 3 })
        );

        budget.screenshot_completed();
        assert_eq!(budget.consecutive_injections(), 0);
        budget.before_injection().await.unwrap();
    }

    #[tokio::test]
    async fn injection_is_not_counted_until_it_succeeds() {
        let budget = Budget::with_config(3, Duration::ZERO);

        budget.before_injection().await.unwrap();
        assert_eq!(budget.consecutive_injections(), 0);
        budget.injection_succeeded();
        assert_eq!(budget.consecutive_injections(), 1);
    }

    #[tokio::test]
    async fn stopped_budget_refuses_injection() {
        let budget = Budget::with_config(3, Duration::ZERO);
        budget.stop();

        assert_eq!(budget.before_injection().await, Err(BudgetError::Stopped));
        assert!(budget.is_stopped());
    }

    #[test]
    fn settle_ms_defaults_and_accepts_bounds() {
        assert_eq!(
            settle_duration(None).unwrap(),
            Duration::from_millis(DEFAULT_SETTLE_MS as u64)
        );
        assert_eq!(settle_duration(Some(0)).unwrap(), Duration::ZERO);
        assert_eq!(
            settle_duration(Some(MAX_SETTLE_MS)).unwrap(),
            Duration::from_millis(MAX_SETTLE_MS as u64)
        );
    }

    #[test]
    fn settle_ms_rejects_negative_and_too_large_values() {
        assert!(settle_duration(Some(-1)).is_err());
        assert!(settle_duration(Some(MAX_SETTLE_MS + 1)).is_err());
    }
}
