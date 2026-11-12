use std::collections::HashSet;
use std::panic::AssertUnwindSafe;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use futures_util::FutureExt;
use newt_core::provider::Model;
use newt_core::sink::EventSink;
use tokio::sync::{Semaphore, broadcast};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::backend::Backend;
use crate::live::{Channel, Notification};
use crate::store::Store;

const SWEEP_LIMIT: i64 = 100;
const FINAL_WAIT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy)]
pub struct RunnerConfig {
    pub lease_ttl: Duration,
    pub sweep_every: Duration,
    pub shutdown_grace: Duration,
    pub backoff_base: Duration,
    pub max_attempts: i32,
    pub max_sessions: usize,
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
    pub(crate) instrumentation: Option<Arc<dyn EventSink>>,
    pub(crate) notifications: broadcast::Sender<Notification>,
    in_flight: Mutex<HashSet<Session>>,
    sessions: Semaphore,
    tasks: TaskTracker,
    pub(crate) stopping: CancellationToken,
    pub(crate) turns: CancellationToken,
}

impl<B, M> Inner<B, M> {
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
        notifications: broadcast::Sender<Notification>,
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
                instrumentation,
                notifications,
                in_flight: Mutex::new(HashSet::new()),
                sessions: Semaphore::new(config.max_sessions.max(1)),
                tasks: TaskTracker::new(),
                stopping: CancellationToken::new(),
                turns: CancellationToken::new(),
            }),
        }
    }

    pub fn owner(&self) -> &str {
        &self.inner.owner
    }

    pub fn model(&self) -> &M {
        &self.inner.model
    }

    pub fn wake(&self, agent: &str, key: &str) {
        if self.inner.stopping.is_cancelled() {
            return;
        }
        let session = (agent.to_owned(), key.to_owned());
        if !self.inner.running().insert(session.clone()) {
            return;
        }
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
    }

    pub fn spawn_loops(&self) -> JoinHandle<()> {
        let runner = self.clone();
        let mut topics = self.inner.notifications.subscribe();
        tokio::spawn(async move {
            let mut sweep = tokio::time::interval(runner.inner.config.sweep_every);
            let mut heartbeat = tokio::time::interval(runner.inner.config.lease_ttl / 3);
            loop {
                tokio::select! {
                    () = runner.inner.stopping.cancelled() => return,
                    _ = sweep.tick() => runner.sweep().await,
                    _ = heartbeat.tick() => runner.heartbeat().await,
                    topic = topics.recv() => match topic {
                        Ok(notification) => {
                            if notification.channel == Channel::Inbox
                                && let Some((agent, key)) = notification.topic.split_once('/')
                            {
                                runner.wake(agent, key);
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(_)) => runner.sweep().await,
                        Err(broadcast::error::RecvError::Closed) => return,
                    },
                }
            }
        })
    }

    pub async fn sweep(&self) {
        match self.inner.store.sessions_needing_work(SWEEP_LIMIT).await {
            Ok(sessions) => {
                for (agent, key) in sessions {
                    self.wake(&agent, &key);
                }
            }
            Err(error) => tracing::error!(%error, "sweep could not list sessions"),
        }
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
