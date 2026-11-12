use std::collections::HashSet;
use std::future::Future;
use std::sync::{Arc, Mutex};

use newt_core::message::Message;
use newt_core::steering::{Steered, Steering};
use tokio::sync::broadcast;

use crate::live::{Channel, Notification};
use crate::sessions::{Lease, PendingEvent};
use crate::store::{Store, live_topic};

#[derive(Debug)]
struct State {
    store: Store,
    lease: Lease,
    topic: String,
    seen: Mutex<HashSet<i64>>,
    receiver: tokio::sync::Mutex<broadcast::Receiver<Notification>>,
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
        notifications: broadcast::Receiver<Notification>,
    ) -> Self {
        Self {
            state: Arc::new(State {
                topic: live_topic(&lease.agent, &lease.key),
                store,
                lease,
                seen: Mutex::new(already_in_turn.into_iter().collect()),
                receiver: tokio::sync::Mutex::new(notifications),
            }),
        }
    }
}

impl State {
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
            let pending = match state.store.pending_events(&state.lease).await {
                Ok(pending) => pending,
                Err(error) => {
                    tracing::error!(%error, "steering could not read the inbox");
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
            let mut receiver = state.receiver.lock().await;
            loop {
                match receiver.recv().await {
                    Ok(notification)
                        if notification.channel == Channel::Inbox
                            && notification.topic == state.topic =>
                    {
                        if state.has_unseen_steer().await {
                            return;
                        }
                    }
                    Ok(_) => {}
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        if state.has_unseen_steer().await {
                            return;
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => std::future::pending::<()>().await,
                }
            }
        }
    }
}
