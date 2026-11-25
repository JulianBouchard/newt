//! What the process knows about its own load, shared by the runner that
//! changes it and the HTTP side that reports it.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Instant;

use serde::Serialize;

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
        self.waiters.lock().unwrap_or_else(PoisonError::into_inner)
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
    fn the_oldest_wait_grows_while_a_session_waits() {
        let metrics = Arc::new(Metrics::new(1));
        let _waiting = metrics.start_waiting();
        std::thread::sleep(std::time::Duration::from_millis(30));
        assert!(metrics.load().oldest_wait_ms >= 25);
    }
}
