//! What the process knows about its own load, shared by the runner that
//! changes it and the HTTP side that reports it.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Instant;

use serde::Serialize;
use tokio_util::sync::CancellationToken;

use crate::counters::{Counter, Counters};

/// A reading of the runner's turn slots.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Load {
    /// Turns running now.
    pub running: usize,
    /// Woken sessions waiting for a slot.
    pub waiting: usize,
    pub max_sessions: usize,
    /// How long the longest-waiting session has been waiting; 0 when none is.
    pub oldest_wait_ms: u64,
}

#[derive(Debug, Default)]
pub struct Metrics {
    max_sessions: usize,
    running: AtomicUsize,
    waiters: Mutex<HashMap<u64, Instant>>,
    next_waiter: AtomicU64,
    turns: Mutex<HashMap<(String, String), (String, CancellationToken)>>,
    counters: Counters,
}

impl Metrics {
    #[must_use]
    pub fn new(max_sessions: usize) -> Self {
        Self {
            max_sessions,
            ..Self::default()
        }
    }

    fn waiters(&self) -> MutexGuard<'_, HashMap<u64, Instant>> {
        match self.waiters.lock() {
            Ok(waiters) => waiters,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    #[must_use]
    pub fn load(&self) -> Load {
        let waiters = self.waiters();
        let oldest = waiters.values().min().map_or(0, |since| {
            u64::try_from(since.elapsed().as_millis()).unwrap_or(u64::MAX)
        });
        Load {
            running: self.running.load(Ordering::Relaxed),
            waiting: waiters.len(),
            max_sessions: self.max_sessions,
            oldest_wait_ms: oldest,
        }
    }

    pub fn count(&self, counter: Counter) {
        self.counters.add(counter, 1);
    }

    pub fn count_many(&self, counter: Counter, amount: usize) {
        self.counters
            .add(counter, u64::try_from(amount).unwrap_or(u64::MAX));
    }

    pub fn counted(&self, counter: Counter) -> u64 {
        self.counters.get(counter)
    }

    /// Everything `/metrics` reports, in the Prometheus text format.
    pub fn render(&self) -> String {
        self.counters.render(self.load())
    }

    fn turns(&self) -> MutexGuard<'_, HashMap<(String, String), (String, CancellationToken)>> {
        match self.turns.lock() {
            Ok(turns) => turns,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// Whether a turn of this session is running on this instance.
    pub fn is_tracking(&self, agent: &str, key: &str) -> bool {
        self.turns()
            .contains_key(&(agent.to_owned(), key.to_owned()))
    }

    /// Cancels the turn `turn` of this session when it runs on this
    /// instance. False when another turn, or none, is running here, or it
    /// was already cancelled.
    pub fn cancel_turn(&self, agent: &str, key: &str, turn: &str) -> bool {
        self.turns()
            .get(&(agent.to_owned(), key.to_owned()))
            .filter(|(running, _)| running == turn)
            .is_some_and(|(_, token)| {
                let live = !token.is_cancelled();
                token.cancel();
                live
            })
    }

    pub(crate) fn track_turn(
        self: &Arc<Self>,
        agent: &str,
        key: &str,
        turn: &str,
        token: CancellationToken,
    ) -> TrackedTurn {
        let session = (agent.to_owned(), key.to_owned());
        self.turns()
            .insert(session.clone(), (turn.to_owned(), token));
        TrackedTurn {
            metrics: Arc::clone(self),
            session,
        }
    }

    pub(crate) fn start_waiting(self: &Arc<Self>) -> Waiting {
        let id = self.next_waiter.fetch_add(1, Ordering::Relaxed);
        let since = Instant::now();
        self.waiters().insert(id, since);
        Waiting {
            metrics: Arc::clone(self),
            id,
            since,
        }
    }
}

/// A turn that can be cancelled by session; dropping it forgets the turn.
pub(crate) struct TrackedTurn {
    metrics: Arc<Metrics>,
    session: (String, String),
}

impl Drop for TrackedTurn {
    fn drop(&mut self) {
        self.metrics.turns().remove(&self.session);
    }
}

/// A woken session waiting for a slot; dropping it stops the wait.
pub(crate) struct Waiting {
    metrics: Arc<Metrics>,
    id: u64,
    since: Instant,
}

impl Waiting {
    pub(crate) fn got_slot(self) -> Running {
        self.metrics
            .counters
            .observe_slot_wait(self.since.elapsed());
        self.metrics.running.fetch_add(1, Ordering::Relaxed);
        Running {
            metrics: Arc::clone(&self.metrics),
        }
    }
}

impl Drop for Waiting {
    fn drop(&mut self) {
        self.metrics.waiters().remove(&self.id);
    }
}

/// A turn holding a slot; dropping it frees the count.
pub(crate) struct Running {
    metrics: Arc<Metrics>,
}

impl Drop for Running {
    fn drop(&mut self) {
        self.metrics.running.fetch_sub(1, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_counts_waiting_then_running_sessions() {
        let metrics = Arc::new(Metrics::new(4));
        assert_eq!(
            metrics.load(),
            Load {
                running: 0,
                waiting: 0,
                max_sessions: 4,
                oldest_wait_ms: 0
            }
        );
        let first = metrics.start_waiting();
        let second = metrics.start_waiting();
        assert_eq!(metrics.load().waiting, 2);
        let running = first.got_slot();
        let load = metrics.load();
        assert_eq!((load.running, load.waiting), (1, 1));
        drop(running);
        drop(second);
        let load = metrics.load();
        assert_eq!((load.running, load.waiting), (0, 0));
    }

    #[test]
    fn cancelling_a_tracked_turn_cancels_its_token_once_and_only_by_its_id() {
        let metrics = Arc::new(Metrics::new(1));
        assert!(!metrics.cancel_turn("assistant", "a", "t1"));
        let token = CancellationToken::new();
        let tracked = metrics.track_turn("assistant", "a", "t1", token.clone());
        assert!(metrics.is_tracking("assistant", "a"));
        assert!(!metrics.cancel_turn("assistant", "b", "t1"));
        assert!(!metrics.cancel_turn("assistant", "a", "t0"));
        assert!(!token.is_cancelled());
        assert!(metrics.cancel_turn("assistant", "a", "t1"));
        assert!(token.is_cancelled());
        assert!(!metrics.cancel_turn("assistant", "a", "t1"));
        drop(tracked);
        assert!(!metrics.is_tracking("assistant", "a"));
        assert!(!metrics.cancel_turn("assistant", "a", "t1"));
    }

    #[test]
    fn the_oldest_wait_grows_while_a_session_waits() {
        let metrics = Arc::new(Metrics::new(1));
        let _waiting = metrics.start_waiting();
        std::thread::sleep(std::time::Duration::from_millis(30));
        assert!(metrics.load().oldest_wait_ms >= 25);
    }
}
