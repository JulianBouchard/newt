use std::collections::HashSet;
use std::future::Future;
use std::sync::{Arc, Mutex};

use crate::live::Watch;
use crate::sessions::{Lease, PendingEvent};
use crate::store::Store;
use newt_core::message::Message;
use newt_core::steering::{Steered, Steering};

#[derive(Debug)]
struct State {
    store: Store,
    lease: Lease,
    seen: Mutex<HashSet<i64>>,
    inbox: tokio::sync::Mutex<Watch>,
    changes: Mutex<InboxChanges>,
}

/// Whether the inbox may hold something steering has not read: a
/// notification arrived, or the last read failed.
#[derive(Debug)]
struct InboxChanges {
    watch: Watch,
    retry: bool,
}

#[derive(Debug, Clone)]
pub struct SessionSteering {
    state: Arc<State>,
}

impl SessionSteering {
    pub fn new(
        store: Store,
        lease: Lease,
        already_in_turn: impl IntoIterator<Item = i64>,
        inbox: Watch,
        changes: Watch,
    ) -> Self {
        Self {
            state: Arc::new(State {
                store,
                lease,
                seen: Mutex::new(already_in_turn.into_iter().collect()),
                inbox: tokio::sync::Mutex::new(inbox),
                changes: Mutex::new(InboxChanges {
                    watch: changes,
                    retry: false,
                }),
            }),
        }
    }
}

impl State {
    fn changes(&self) -> std::sync::MutexGuard<'_, InboxChanges> {
        match self.changes.lock() {
            Ok(changes) => changes,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    fn inbox_changed(&self) -> bool {
        let mut changes = self.changes();
        let notified = changes.watch.take_change();
        let retry = std::mem::take(&mut changes.retry);
        notified || retry
    }

    fn unseen_steers(&self, pending: Vec<PendingEvent>) -> Vec<PendingEvent> {
        let seen = match self.seen.lock() {
            Ok(seen) => seen,
            Err(poisoned) => poisoned.into_inner(),
        };
        pending
            .into_iter()
            .filter(|event| event.steer && !seen.contains(&event.inbox_id))
            .collect()
    }

    async fn has_unseen_steer(&self) -> bool {
        match self.store.pending_events(&self.lease).await {
            Ok(pending) => !self.unseen_steers(pending).is_empty(),
            Err(error) => {
                tracing::error!(%error, "steering could not read the inbox");
                false
            }
        }
    }
}

impl Steering for SessionSteering {
    fn take_new(&self) -> impl Future<Output = Vec<Steered>> + Send {
        let state = Arc::clone(&self.state);
        async move {
            if !state.inbox_changed() {
                return Vec::new();
            }
            let pending = match state.store.pending_events(&state.lease).await {
                Ok(pending) => pending,
                Err(error) => {
                    tracing::error!(%error, "steering could not read the inbox");
                    state.changes().retry = true;
                    return Vec::new();
                }
            };
            let fresh = state.unseen_steers(pending);
            let mut seen = match state.seen.lock() {
                Ok(seen) => seen,
                Err(poisoned) => poisoned.into_inner(),
            };
            fresh
                .into_iter()
                .filter(|event| seen.insert(event.inbox_id))
                .map(|event| Steered {
                    id: event.inbox_id.to_string(),
                    message: Message::user_text(event.text),
                })
                .collect()
        }
    }

    fn arrived(&self) -> impl Future<Output = ()> + Send {
        let state = Arc::clone(&self.state);
        async move {
            let mut inbox = state.inbox.lock().await;
            loop {
                inbox.changed().await;
                if state.has_unseen_steer().await {
                    return;
                }
            }
        }
    }
}
