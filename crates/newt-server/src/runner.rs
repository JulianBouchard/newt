use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use newt_core::message::Message;
use newt_core::prompt::Lane;
use newt_core::provider::{Model, ProviderError};
use newt_core::sink::EventSink;
use newt_core::turn::{TurnError, TurnInput, TurnSettings, run_turn};
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::backend::Backend;
use crate::sessions::{CommitError, Lease, PendingEvent};
use crate::sinks::{LiveSink, TeeSink};
use crate::store::Store;

const SWEEP_LIMIT: i64 = 100;

#[derive(Debug, Clone, Copy)]
pub struct RunnerConfig {
    pub lease_ttl: Duration,
    pub sweep_every: Duration,
    pub shutdown_grace: Duration,
}

impl Default for RunnerConfig {
    fn default() -> Self {
        Self {
            lease_ttl: Duration::from_secs(30),
            sweep_every: Duration::from_secs(1),
            shutdown_grace: Duration::from_secs(8),
        }
    }
}

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

struct Inner<B, M> {
    store: Store,
    backend: B,
    model: M,
    owner: String,
    config: RunnerConfig,
    instrumentation: Option<Arc<dyn EventSink>>,
    in_flight: Mutex<HashSet<(String, String)>>,
    tasks: TaskTracker,
    stopping: CancellationToken,
    turns: CancellationToken,
}

impl<B: Backend, M: Model + 'static> Runner<B, M> {
    pub fn new(
        store: Store,
        backend: B,
        model: M,
        config: RunnerConfig,
        instrumentation: Option<Arc<dyn EventSink>>,
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
                in_flight: Mutex::new(HashSet::new()),
                tasks: TaskTracker::new(),
                stopping: CancellationToken::new(),
                turns: CancellationToken::new(),
            }),
        }
    }

    pub fn owner(&self) -> &str {
        &self.inner.owner
    }

    pub fn wake(&self, agent: &str, key: &str) {
        if self.inner.stopping.is_cancelled() {
            return;
        }
        let session = (agent.to_owned(), key.to_owned());
        let newly_running = match self.inner.in_flight.lock() {
            Ok(mut running) => running.insert(session.clone()),
            Err(poisoned) => poisoned.into_inner().insert(session.clone()),
        };
        if !newly_running {
            return;
        }
        let inner = Arc::clone(&self.inner);
        self.inner.tasks.spawn(async move {
            if let Err(error) = run_session(&inner, &session.0, &session.1).await {
                tracing::error!(%error, agent = %session.0, key = %session.1, "session run failed");
            }
            match inner.in_flight.lock() {
                Ok(mut running) => running.remove(&session),
                Err(poisoned) => poisoned.into_inner().remove(&session),
            };
        });
    }

    pub fn spawn_loops(&self, mut topics: broadcast::Receiver<String>) -> JoinHandle<()> {
        let runner = self.clone();
        tokio::spawn(async move {
            let mut sweep = tokio::time::interval(runner.inner.config.sweep_every);
            let mut heartbeat = tokio::time::interval(runner.inner.config.lease_ttl / 3);
            loop {
                tokio::select! {
                    () = runner.inner.stopping.cancelled() => return,
                    _ = sweep.tick() => runner.sweep().await,
                    _ = heartbeat.tick() => runner.heartbeat().await,
                    topic = topics.recv() => match topic {
                        Ok(topic) => {
                            if let Some((agent, key)) = topic.split_once('/') {
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
        if let Err(error) = self
            .inner
            .store
            .heartbeat(&self.inner.owner, self.inner.config.lease_ttl)
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
            self.inner.tasks.wait().await;
        }
    }

    pub async fn idle(&self) {
        loop {
            let busy = match self.inner.in_flight.lock() {
                Ok(running) => !running.is_empty(),
                Err(poisoned) => !poisoned.into_inner().is_empty(),
            };
            if !busy {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}

async fn run_session<B: Backend, M: Model>(
    inner: &Inner<B, M>,
    agent: &str,
    key: &str,
) -> anyhow::Result<()> {
    let ttl = inner.config.lease_ttl;
    let Some(lease) = inner.store.claim(agent, key, &inner.owner, ttl).await? else {
        return Ok(());
    };
    loop {
        if inner.stopping.is_cancelled() {
            inner.store.release(&lease).await?;
            return Ok(());
        }
        let pending = inner.store.pending_events(&lease).await?;
        if pending.is_empty() {
            if inner.store.release_if_idle(&lease).await? {
                return Ok(());
            }
            continue;
        }
        if !run_one_turn(inner, &lease, &pending).await? {
            return Ok(());
        }
    }
}

async fn run_one_turn<B: Backend, M: Model>(
    inner: &Inner<B, M>,
    lease: &Lease,
    pending: &[PendingEvent],
) -> anyhow::Result<bool> {
    let prepared = match inner.backend.prepare(&lease.agent, &lease.key).await {
        Ok(prepared) => prepared,
        Err(error) => {
            tracing::error!(%error, agent = %lease.agent, key = %lease.key, "turn preparation failed");
            inner.store.release_with_backoff(lease).await?;
            return Ok(false);
        }
    };
    let history = inner.store.history(lease).await?;
    let events: Vec<Message> = pending
        .iter()
        .map(|event| Message::user_text(event.text.clone()))
        .collect();
    let consumed: Vec<i64> = pending.iter().map(|event| event.inbox_id).collect();
    let lane = if pending.iter().any(|event| event.interactive) {
        Lane::Interactive
    } else {
        Lane::Background
    };
    let turn_id = uuid::Uuid::now_v7().to_string();
    let cache_key = format!("{}/{}", lease.agent, lease.key);
    let settings = TurnSettings {
        model: &prepared.model,
        first_call_effort: prepared.first_call_effort,
        effort: prepared.effort,
        max_output_tokens: prepared.max_output_tokens,
        max_model_calls: prepared.max_model_calls,
        lane,
    };

    let (live, live_writer) =
        LiveSink::spawn(inner.store.clone(), lease.agent.clone(), lease.key.clone());
    let result = run_turn(
        &inner.model,
        &prepared.host,
        TurnInput {
            session: &cache_key,
            turn: &turn_id,
            system: &prepared.system,
            external_tools: &prepared.tools,
            history: &history,
            events,
            workspace: &prepared.workspace,
        },
        &settings,
        &TeeSink {
            live: &live,
            instrumentation: inner.instrumentation.as_ref(),
        },
        &inner.turns,
    )
    .await;
    drop(live);
    live_writer.await?;

    let (committed, failure) = match result {
        Ok(outcome) => (outcome.committed, None),
        Err(failure) => (failure.committed, Some(failure.error)),
    };
    let acted = committed.len() > pending.len();
    if failure.is_none() || acted {
        match inner
            .store
            .commit_turn(lease, turn_id, committed, consumed, inner.config.lease_ttl)
            .await
        {
            Ok(()) => {}
            Err(CommitError::Fenced) => {
                tracing::warn!(agent = %lease.agent, key = %lease.key, "lease lost before commit, turn discarded");
                return Ok(false);
            }
            Err(CommitError::Database(error)) => return Err(error.into()),
        }
    }
    match failure {
        None => Ok(true),
        Some(TurnError::Provider(ProviderError::Cancelled)) => {
            inner.store.release(lease).await?;
            Ok(false)
        }
        Some(error) => {
            tracing::error!(%error, agent = %lease.agent, key = %lease.key, "turn failed");
            inner.store.release_with_backoff(lease).await?;
            Ok(false)
        }
    }
}
