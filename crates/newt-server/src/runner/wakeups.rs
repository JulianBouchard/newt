use super::{Inner, Runner, SWEEP_LIMIT, Session, elapsed_ms};
use std::panic::AssertUnwindSafe;
use std::sync::Arc;

use futures_util::FutureExt;
use newt_core::event::{EventKind, WakeSource};
use newt_core::prompt::Lane;
use newt_core::provider::Model;
use tokio::sync::broadcast;

use crate::backend::Backend;
use crate::counters::Counter;

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
    pub fn wake(&self, agent: &str, key: &str) {
        self.wake_from(agent, key, WakeSource::Direct);
    }

    pub(super) fn wake_from(&self, agent: &str, key: &str, source: WakeSource) -> bool {
        if self.inner.stopping.is_cancelled() {
            return false;
        }
        let session = (agent.to_owned(), key.to_owned());
        if !self.inner.running().insert(session.clone()) {
            return false;
        }
        if self.inner.metrics.load().waiting >= self.inner.config.max_waiting {
            self.inner.running().remove(&session);
            self.inner.metrics.count(Counter::WakesDropped);
            return false;
        }
        let waiting = self.inner.metrics.start_waiting();
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
            let lane = match inner.store.pending_lane(&session.0, &session.1).await {
                Ok(lane) => lane,
                Err(error) => {
                    tracing::error!(%error, agent = %session.0, key = %session.1, "could not read the session's lane");
                    return;
                }
            };
            let _background = match lane {
                Lane::Background => match inner.background.acquire().await {
                    Ok(permit) => Some(permit),
                    Err(_) => return,
                },
                Lane::Interactive => None,
            };
            let Ok(_permit) = inner.sessions.acquire().await else {
                return;
            };
            let _running = waiting.got_slot();
            let run = crate::turn_runner::run_session(&inner, &session.0, &session.1);
            match AssertUnwindSafe(run).catch_unwind().await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    tracing::error!(%error, agent = %session.0, key = %session.1, "session run failed");
                }
                Err(_) => {
                    tracing::error!(agent = %session.0, key = %session.1, "session task panicked outside a turn; its lease will expire");
                }
            }
        });
        true
    }

    pub(super) async fn wake_on_notify(self, nudge: Arc<tokio::sync::Notify>) {
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

    pub(super) async fn cancel_on_notify(self) {
        let mut topics = self.inner.notifications.cancels();
        loop {
            tokio::select! {
                () = self.inner.stopping.cancelled() => return,
                topic = topics.recv() => match topic {
                    Ok(topic) => {
                        if let Some((agent, key)) = topic.split_once('/') {
                            self.cancel_requested(agent, key).await;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        tracing::warn!("cancel notifications lagged; the next heartbeat finds the requests");
                    }
                    Err(broadcast::error::RecvError::Closed) => return,
                },
            }
        }
    }

    pub(super) async fn cancel_requested(&self, agent: &str, key: &str) {
        if !self.inner.running_turns.is_running(agent, key) {
            return;
        }
        match self
            .inner
            .store
            .requested_cancel(&self.inner.owner, agent, key)
            .await
        {
            Ok(Some(turn)) => {
                if self.inner.running_turns.cancel(agent, key, &turn) {
                    tracing::info!(agent, key, %turn, "cancelling the turn on request");
                }
            }
            Ok(None) => {}
            Err(error) => tracing::error!(%error, agent, key, "could not read a cancel request"),
        }
    }

    pub(super) async fn sweep_periodically(self, nudge: Arc<tokio::sync::Notify>) {
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
}
