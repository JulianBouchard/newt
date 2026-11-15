use std::collections::HashSet;
use std::panic::AssertUnwindSafe;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use futures_util::FutureExt;
use newt_core::event::{Event, EventKind, WakeSource};
use newt_core::provider::Model;
use newt_core::sink::EventSink;
use tokio::sync::{Semaphore, broadcast};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::backend::Backend;
use crate::cache_keys::{Buckets, CacheKeys};
use crate::live::Topics;
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
            cache_key_buckets: Buckets::Fixed(1),
            retention: Retention::default(),
        }
    }
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
    pub(crate) instrumentation: Option<Arc<dyn EventSink>>,
    pub(crate) notifications: Topics,
    in_flight: Mutex<HashSet<Session>>,
    sessions: Semaphore,
    tasks: TaskTracker,
    pub(crate) stopping: CancellationToken,
    pub(crate) turns: CancellationToken,
    pub(crate) turn_ends: tokio::sync::Notify,
}

pub(crate) fn elapsed_ms(started: std::time::Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

impl<B, M> Inner<B, M> {
    pub(crate) fn emit(&self, kind: EventKind) {
        if let Some(sink) = &self.instrumentation {
            sink.emit(Event::now(kind));
        }
    }

    fn running(&self) -> MutexGuard<'_, HashSet<Session>> {
        match self.in_flight.lock() {
            Ok(running) => running,
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}

struct InFlight<'a, B, M> {
    inner: &'a Inner<B, M>,
    session: Session,
}

impl<B, M> Drop for InFlight<'_, B, M> {
    fn drop(&mut self) {
        self.inner.running().remove(&self.session);
    }
}

impl<B: Backend, M: Model + 'static> Runner<B, M> {
    pub fn new(
        store: Store,
        backend: B,
        model: M,
        config: RunnerConfig,
        instrumentation: Option<Arc<dyn EventSink>>,
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
                in_flight: Mutex::new(HashSet::new()),
                sessions: Semaphore::new(config.max_sessions.max(1)),
                tasks: TaskTracker::new(),
                stopping: CancellationToken::new(),
                turns: CancellationToken::new(),
                turn_ends: tokio::sync::Notify::new(),
            }),
        }
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

    pub fn wake(&self, agent: &str, key: &str) {
        self.wake_from(agent, key, WakeSource::Direct);
    }

    fn wake_from(&self, agent: &str, key: &str, source: WakeSource) -> bool {
        if self.inner.stopping.is_cancelled() {
            return false;
        }
        let session = (agent.to_owned(), key.to_owned());
        if !self.inner.running().insert(session.clone()) {
            return false;
        }
        self.inner.emit(EventKind::SessionWoken {
            session: format!("{agent}/{key}"),
            source,
        });
        let inner = Arc::clone(&self.inner);
        self.inner.tasks.spawn(async move {
            let _in_flight = InFlight {
                inner: &inner,
                session: session.clone(),
            };
            let Ok(_permit) = inner.sessions.acquire().await else {
                return;
            };
            let run = crate::turn_runner::run_session(&inner, &session.0, &session.1);
            match AssertUnwindSafe(run).catch_unwind().await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    tracing::error!(%error, agent = %session.0, key = %session.1, "session run failed");
                }
                Err(_) => {
                    tracing::error!(agent = %session.0, key = %session.1, "session task panicked; its lease will expire");
                }
            }
        });
        true
    }

    pub fn spawn_loops(&self) -> JoinHandle<()> {
        let nudge = Arc::new(tokio::sync::Notify::new());
        let wakes = self.clone().wake_on_notify(Arc::clone(&nudge));
        let sweeps = self.clone().sweep_periodically(nudge);
        let heartbeats = self.clone().heartbeat_periodically();
        let prunes = self.clone().prune_periodically();
        let turn_ends = self.clone().deliver_turn_ends_periodically();
        tokio::spawn(async move {
            tokio::join!(wakes, sweeps, heartbeats, prunes, turn_ends);
        })
    }

    async fn wake_on_notify(self, nudge: Arc<tokio::sync::Notify>) {
        let mut topics = self.inner.notifications.inbox();
        loop {
            tokio::select! {
                () = self.inner.stopping.cancelled() => return,
                topic = topics.recv() => match topic {
                    Ok(topic) => {
                        if let Some((agent, key)) = topic.split_once('/') {
                            self.wake_from(agent, key, WakeSource::Notify);
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => nudge.notify_one(),
                    Err(broadcast::error::RecvError::Closed) => return,
                },
            }
        }
    }

    async fn sweep_periodically(self, nudge: Arc<tokio::sync::Notify>) {
        let mut every = tokio::time::interval(self.inner.config.sweep_every);
        loop {
            tokio::select! {
                () = self.inner.stopping.cancelled() => return,
                _ = every.tick() => {}
                () = nudge.notified() => {}
            }
            self.sweep().await;
        }
    }

    async fn deliver_turn_ends_periodically(self) {
        let mut every = tokio::time::interval(TURN_END_SWEEP);
        loop {
            tokio::select! {
                () = self.inner.stopping.cancelled() => return,
                _ = every.tick() => {}
                () = self.inner.turn_ends.notified() => {}
            }
            tokio::select! {
                () = self.inner.stopping.cancelled() => return,
                _delivered = self.deliver_turn_ends() => {}
            }
        }
    }

    /// Delivers the turn-end callbacks that are due, and returns how many
    /// were delivered. A failed one is retried later with backoff.
    pub async fn deliver_turn_ends(&self) -> usize {
        let due = match self.inner.store.claim_turn_ends(TURN_END_BATCH).await {
            Ok(due) => due,
            Err(error) => {
                tracing::error!(%error, "could not claim turn-end callbacks");
                return 0;
            }
        };
        let deliveries = due.into_iter().map(|turn_end| async move {
            if turn_end.attempts > TURN_END_MAX_ATTEMPTS {
                tracing::error!(id = turn_end.id, agent = %turn_end.agent, attempts = turn_end.attempts, "giving up on a turn-end callback");
                if let Err(error) = self.inner.store.delete_turn_end(turn_end.id).await {
                    tracing::error!(%error, id = turn_end.id, "could not delete an abandoned turn end");
                }
                return false;
            }
            match self.inner.backend.turn_end(&turn_end.agent, &turn_end.payload).await {
                Ok(()) => match self.inner.store.delete_turn_end(turn_end.id).await {
                    Ok(()) => true,
                    Err(error) => {
                        tracing::error!(%error, id = turn_end.id, "delivered a turn end but could not delete it; it will be sent again");
                        false
                    }
                },
                Err(error) => {
                    tracing::warn!(%error, id = turn_end.id, agent = %turn_end.agent, attempts = turn_end.attempts, "turn-end callback failed, retrying later");
                    false
                }
            }
        });
        futures_util::future::join_all(deliveries)
            .await
            .into_iter()
            .filter(|delivered| *delivered)
            .count()
    }

    async fn heartbeat_periodically(self) {
        let mut every = tokio::time::interval(self.inner.config.lease_ttl / 3);
        loop {
            tokio::select! {
                () = self.inner.stopping.cancelled() => return,
                _ = every.tick() => self.heartbeat().await,
            }
        }
    }

    async fn prune_periodically(self) {
        let mut every = tokio::time::interval(PRUNE_EVERY);
        loop {
            tokio::select! {
                () = self.inner.stopping.cancelled() => return,
                _ = every.tick() => {}
            }
            match self.inner.store.prune(self.inner.config.retention).await {
                Ok(pruned) => tracing::info!(
                    inbox = pruned.inbox,
                    live = pruned.live,
                    "pruned consumed events and live feed rows"
                ),
                Err(error) => tracing::error!(%error, "pruning failed"),
            }
        }
    }

    pub async fn sweep(&self) {
        let started = std::time::Instant::now();
        let mut woken = 0;
        match self.inner.store.sessions_needing_work(SWEEP_LIMIT).await {
            Ok(sessions) => {
                for (agent, key) in sessions {
                    woken += usize::from(self.wake_from(&agent, &key, WakeSource::Sweep));
                }
            }
            Err(error) => tracing::error!(%error, "sweep could not list sessions"),
        }
        let pool = self.inner.store.pool();
        self.inner.emit(EventKind::RunnerSample {
            running: self.inner.running().len(),
            pool_size: pool.size(),
            pool_idle: pool.num_idle(),
            sweep_ms: elapsed_ms(started),
            woken,
        });
    }

    async fn heartbeat(&self) {
        let running: Vec<Session> = self.inner.running().iter().cloned().collect();
        if running.is_empty() {
            return;
        }
        if let Err(error) = self
            .inner
            .store
            .heartbeat(&self.inner.owner, self.inner.config.lease_ttl, running)
            .await
        {
            tracing::error!(%error, "lease heartbeat failed");
        }
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
