use super::{PRUNE_EVERY, Runner, Session, TURN_END_BATCH, TURN_END_MAX_ATTEMPTS, TURN_END_SWEEP};
use std::sync::Arc;

use newt_core::provider::Model;
use tokio::task::JoinHandle;

use crate::backend::Backend;

impl<B: Backend, M: Model + 'static> Runner<B, M> {
    pub fn spawn_loops(&self) -> JoinHandle<()> {
        let nudge = Arc::new(tokio::sync::Notify::new());
        let wakes = self.clone().wake_on_notify(Arc::clone(&nudge));
        let sweeps = self.clone().sweep_periodically(nudge);
        let heartbeats = self.clone().heartbeat_periodically();
        let cancels = self.clone().cancel_on_notify();
        let prunes = self.clone().prune_periodically();
        let turn_ends = self.clone().deliver_turn_ends_periodically();
        tokio::spawn(async move {
            tokio::join!(wakes, sweeps, heartbeats, cancels, prunes, turn_ends);
        })
    }

    pub(super) async fn deliver_turn_ends_periodically(self) {
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

    pub(super) async fn heartbeat_periodically(self) {
        let mut every = tokio::time::interval(self.inner.config.lease_ttl / 3);
        loop {
            tokio::select! {
                () = self.inner.stopping.cancelled() => return,
                _ = every.tick() => self.heartbeat().await,
            }
        }
    }

    pub(super) async fn prune_periodically(self) {
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

    pub(super) async fn heartbeat(&self) {
        let running: Vec<Session> = self.inner.running().iter().cloned().collect();
        if running.is_empty() {
            return;
        }
        match self
            .inner
            .store
            .heartbeat(&self.inner.owner, self.inner.config.lease_ttl, running)
            .await
        {
            Ok(requests) => {
                for (agent, key, turn) in requests {
                    if self.inner.running_turns.cancel(&agent, &key, &turn) {
                        tracing::info!(%agent, %key, %turn, "cancelling the turn on a request found at the heartbeat");
                    }
                }
            }
            Err(error) => tracing::error!(%error, "lease heartbeat failed"),
        }
    }
}
