mod maintenance;
mod wakeups;

use std::collections::HashSet;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use newt_core::provider::Model;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::backend::Backend;
use crate::cache_keys::{Buckets, CacheKeys};
use crate::cancel::RunningTurns;
use crate::events::{Instrumentation, ServerEventKind};
use crate::live::Topics;
use crate::metrics::Metrics;
use crate::retention::Retention;
use crate::store::Store;

const SWEEP_LIMIT: i64 = 100;
const FINAL_WAIT: Duration = Duration::from_secs(5);
const PRUNE_EVERY: Duration = Duration::from_hours(1);
const TURN_END_SWEEP: Duration = Duration::from_secs(2);
const TURN_END_BATCH: i64 = 20;
const TURN_END_MAX_ATTEMPTS: i32 = 30;

#[derive(Debug, Clone, Copy)]
pub struct RunnerConfig {
    pub lease_ttl: Duration,
    pub sweep_every: Duration,
    pub shutdown_grace: Duration,
    pub backoff_base: Duration,
    pub max_attempts: i32,
    pub max_sessions: usize,
    /// Slots only interactive turns may use: background turns share the
    /// other `max_sessions - reserved_interactive`, and at least one.
    pub reserved_interactive: usize,
    /// Woken sessions that may wait for a slot at once. A wake past it is
    /// dropped and the session stays in the inbox for the next sweep.
    pub max_waiting: usize,
    pub cache_key_buckets: Buckets,
    pub retention: Retention,
}

impl Default for RunnerConfig {
    fn default() -> Self {
        Self {
            lease_ttl: Duration::from_secs(30),
            sweep_every: Duration::from_secs(1),
            shutdown_grace: Duration::from_secs(8),
            backoff_base: Duration::from_secs(2),
            max_attempts: 5,
            max_sessions: 16,
            reserved_interactive: default_reserved_interactive(16),
            max_waiting: default_max_waiting(16),
            cache_key_buckets: Buckets::Fixed(1),
            retention: Retention::default(),
        }
    }
}

/// A quarter of the slots, and at least one, but never all of them.
#[must_use]
pub fn default_reserved_interactive(max_sessions: usize) -> usize {
    (max_sessions / 4)
        .max(1)
        .min(max_sessions.saturating_sub(1))
}

/// Four waiting sessions per slot.
#[must_use]
pub fn default_max_waiting(max_sessions: usize) -> usize {
    max_sessions.saturating_mul(4).max(1)
}

type Session = (String, String);

pub struct Runner<B: Backend, M: Model + 'static> {
    inner: Arc<Inner<B, M>>,
}

impl<B: Backend, M: Model + 'static> Clone for Runner<B, M> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl<B: Backend, M: Model + 'static> std::fmt::Debug for Runner<B, M> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Runner")
            .field("owner", &self.inner.owner)
            .finish_non_exhaustive()
    }
}

pub(crate) struct Inner<B, M> {
    pub(crate) store: Store,
    pub(crate) backend: B,
    pub(crate) model: M,
    pub(crate) owner: String,
    pub(crate) config: RunnerConfig,
    pub(crate) cache_keys: CacheKeys,
    pub(crate) instrumentation: Option<Arc<Instrumentation>>,
    pub(crate) notifications: Topics,
    pub(crate) live_stream: HashSet<String>,
    in_flight: Mutex<HashSet<Session>>,
    sessions: Semaphore,
    background: Semaphore,
    tasks: TaskTracker,
    pub(crate) stopping: CancellationToken,
    pub(crate) turns: CancellationToken,
    pub(crate) turn_ends: tokio::sync::Notify,
    pub(crate) metrics: Arc<Metrics>,
    pub(crate) running_turns: Arc<RunningTurns>,
}

pub(crate) fn elapsed_ms(started: std::time::Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

impl<B, M> Inner<B, M> {
    pub(crate) fn emit(&self, kind: ServerEventKind) {
        if let Some(instrumentation) = &self.instrumentation {
            instrumentation.server(kind);
        }
    }

    fn running(&self) -> MutexGuard<'_, HashSet<Session>> {
        match self.in_flight.lock() {
            Ok(running) => running,
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}

fn background_slots(config: RunnerConfig) -> usize {
    config
        .max_sessions
        .saturating_sub(config.reserved_interactive)
        .max(1)
}

impl<B: Backend, M: Model + 'static> Runner<B, M> {
    pub fn new(
        store: Store,
        backend: B,
        model: M,
        config: RunnerConfig,
        instrumentation: Option<Arc<Instrumentation>>,
        notifications: Topics,
    ) -> Self {
        let owner = format!(
            "{}-{}-{}",
            std::env::var("HOSTNAME").unwrap_or_else(|_| "newt".to_owned()),
            std::process::id(),
            uuid::Uuid::now_v7().simple()
        );
        Self {
            inner: Arc::new(Inner {
                store,
                backend,
                model,
                owner,
                config,
                cache_keys: CacheKeys::new(config.cache_key_buckets),
                instrumentation,
                notifications,
                live_stream: HashSet::new(),
                in_flight: Mutex::new(HashSet::new()),
                sessions: Semaphore::new(config.max_sessions.max(1)),
                background: Semaphore::new(background_slots(config)),
                tasks: TaskTracker::new(),
                stopping: CancellationToken::new(),
                turns: CancellationToken::new(),
                turn_ends: tokio::sync::Notify::new(),
                metrics: Arc::new(Metrics::new(config.max_sessions.max(1))),
                running_turns: Arc::default(),
            }),
        }
    }

    /// Turns on `live_stream` for these agents.
    ///
    /// # Panics
    ///
    /// If the runner was already cloned or started.
    #[must_use]
    pub fn with_live_stream(mut self, agents: HashSet<String>) -> Self {
        #[expect(
            clippy::expect_used,
            reason = "a setup mistake, caught by the first test that builds a runner"
        )]
        let inner = Arc::get_mut(&mut self.inner)
            .expect("with_live_stream is called before the runner is shared");
        inner.live_stream = agents;
        self
    }

    /// The load figures this runner keeps, for `/healthz` and `/metrics`.
    pub fn metrics(&self) -> Arc<Metrics> {
        Arc::clone(&self.inner.metrics)
    }

    pub fn owner(&self) -> &str {
        &self.inner.owner
    }

    pub fn model(&self) -> &M {
        &self.inner.model
    }

    pub fn backend(&self) -> &B {
        &self.inner.backend
    }

    pub async fn shutdown(&self) {
        self.inner.stopping.cancel();
        self.inner.tasks.close();
        if tokio::time::timeout(self.inner.config.shutdown_grace, self.inner.tasks.wait())
            .await
            .is_err()
        {
            tracing::warn!("turns still running at the end of the grace period, cancelling them");
            self.inner.turns.cancel();
            if tokio::time::timeout(FINAL_WAIT, self.inner.tasks.wait())
                .await
                .is_err()
            {
                tracing::error!("turns ignored cancellation; their leases will expire");
            }
        }
    }

    pub async fn idle(&self) {
        while !self.inner.running().is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}
