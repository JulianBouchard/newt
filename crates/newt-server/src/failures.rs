//! What happens to a turn that failed before it had any effect outside:
//! run it again later, or give up and dead-letter its events. A turn whose
//! tool calls already reached the outside world is always run again, so a
//! policy is never asked about one.

use newt_core::provider::ProviderError;
use newt_core::turn::TurnError;

/// Decides whether a failed turn runs again or its events are dead-lettered.
pub trait FailurePolicy: Send + Sync {
    fn on_failure(&self, failure: &FailedTurn<'_>) -> AfterFailure;
}

/// A turn that failed with no effect outside.
#[derive(Debug)]
pub struct FailedTurn<'a> {
    pub agent: &'a str,
    pub session: &'a str,
    pub error: &'a TurnError,
    /// Failed attempts at this turn, this one included; 0 when cancelled.
    pub attempts: i32,
    pub cancelled: bool,
}

impl FailedTurn<'_> {
    pub const fn is_cancelled(error: &TurnError) -> bool {
        matches!(error, TurnError::Provider(ProviderError::Cancelled))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AfterFailure {
    /// Release the lease with backoff; the turn runs again.
    Retry,
    /// Dead-letter the turn's events with this reason.
    DeadLetter { reason: String },
}

/// The default: a cancelled turn is dead-lettered at once, any other after
/// `attempts` failed attempts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GiveUpAfter {
    pub attempts: i32,
}

impl Default for GiveUpAfter {
    fn default() -> Self {
        Self { attempts: 5 }
    }
}

impl FailurePolicy for GiveUpAfter {
    fn on_failure(&self, failure: &FailedTurn<'_>) -> AfterFailure {
        if failure.cancelled {
            AfterFailure::DeadLetter {
                reason: "cancelled".to_owned(),
            }
        } else if failure.attempts >= self.attempts {
            AfterFailure::DeadLetter {
                reason: failure.error.to_string(),
            }
        } else {
            AfterFailure::Retry
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn failed(error: &TurnError, attempts: i32) -> FailedTurn<'_> {
        FailedTurn {
            agent: "assistant",
            session: "ana",
            error,
            attempts,
            cancelled: FailedTurn::is_cancelled(error),
        }
    }

    #[test]
    fn the_default_retries_until_the_fifth_attempt_and_gives_up_on_a_cancel_at_once() {
        let policy = GiveUpAfter::default();
        let error = TurnError::NoEndTurn;
        assert_eq!(policy.on_failure(&failed(&error, 4)), AfterFailure::Retry);
        assert_eq!(
            policy.on_failure(&failed(&error, 5)),
            AfterFailure::DeadLetter {
                reason: error.to_string()
            }
        );
        let cancelled = TurnError::Provider(ProviderError::Cancelled);
        assert_eq!(
            policy.on_failure(&failed(&cancelled, 0)),
            AfterFailure::DeadLetter {
                reason: "cancelled".to_owned()
            }
        );
    }
}
