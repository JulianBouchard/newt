use std::sync::Arc;

use newt_core::event::{Event, EventKind};
use newt_core::sink::EventSink;
use std::time::{Duration, Instant};

use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;

use crate::live::{Channel, Notification};
use crate::store::{Store, live_topic};

const LIVE_BUFFER: usize = 256;
const LIVE_BATCH: usize = 256;
const REMOTE_NOTIFY_EVERY: Duration = Duration::from_millis(200);

#[derive(Debug, Clone)]
pub struct LiveSink {
    sender: mpsc::Sender<Event>,
}

impl LiveSink {
    pub fn spawn(
        store: Store,
        local: broadcast::Sender<Notification>,
        agent: String,
        key: String,
    ) -> (Self, JoinHandle<()>) {
        let (sender, receiver) = mpsc::channel::<Event>(LIVE_BUFFER);
        let writer = tokio::spawn(write_live(store, local, agent, key, receiver));
        (Self { sender }, writer)
    }
}

fn encode(batch: &mut Vec<Event>) -> Vec<serde_json::Value> {
    batch
        .drain(..)
        .filter_map(|event| match serde_json::to_value(&event) {
            Ok(encoded) => Some(encoded),
            Err(error) => {
                tracing::error!(%error, "could not encode a live event");
                None
            }
        })
        .collect()
}

async fn write_live(
    store: Store,
    local: broadcast::Sender<Notification>,
    agent: String,
    key: String,
    mut receiver: mpsc::Receiver<Event>,
) {
    let topic = live_topic(&agent, &key);
    let mut batch = Vec::with_capacity(LIVE_BATCH);
    let mut last_remote: Option<Instant> = None;
    let mut owed = false;
    loop {
        let drained = if owed {
            let since = last_remote.map_or(REMOTE_NOTIFY_EVERY, |at| at.elapsed());
            let wait = REMOTE_NOTIFY_EVERY.saturating_sub(since);
            tokio::time::timeout(wait, receiver.recv_many(&mut batch, LIVE_BATCH))
                .await
                .ok()
        } else {
            Some(receiver.recv_many(&mut batch, LIVE_BATCH).await)
        };
        match drained {
            None => {
                if let Err(error) = store.notify_live(&agent, &key).await {
                    tracing::error!(%error, %agent, %key, "could not announce live events");
                }
                last_remote = Some(Instant::now());
                owed = false;
            }
            Some(0) => break,
            Some(_) => {
                let remote = last_remote.is_none_or(|at| at.elapsed() >= REMOTE_NOTIFY_EVERY);
                let encoded = encode(&mut batch);
                match store.append_live_batch(&agent, &key, encoded, remote).await {
                    Ok(()) => {
                        if local
                            .send(Notification {
                                channel: Channel::Live,
                                topic: topic.clone(),
                            })
                            .is_err()
                        {
                            tracing::trace!("no local live subscribers");
                        }
                        if remote {
                            last_remote = Some(Instant::now());
                        }
                        owed = !remote;
                    }
                    Err(error) => {
                        tracing::error!(%error, %agent, %key, "could not append live events");
                    }
                }
            }
        }
    }
    if owed && let Err(error) = store.notify_live(&agent, &key).await {
        tracing::error!(%error, %agent, %key, "could not announce live events");
    }
}

fn is_live(kind: &EventKind) -> bool {
    matches!(
        kind,
        EventKind::TurnStarted { .. }
            | EventKind::TurnSteered { .. }
            | EventKind::ToolStarted { .. }
            | EventKind::ToolFinished { .. }
            | EventKind::TurnCompleted { .. }
            | EventKind::TurnFailed { .. }
    )
}

impl EventSink for LiveSink {
    fn emit(&self, event: Event) {
        if is_live(&event.kind) && self.sender.try_send(event).is_err() {
            tracing::warn!("live feed buffer full or closed, event dropped");
        }
    }
}

#[derive(Clone, Copy)]
pub struct TeeSink<'a> {
    pub live: &'a LiveSink,
    pub instrumentation: Option<&'a Arc<dyn EventSink>>,
}

impl std::fmt::Debug for TeeSink<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TeeSink")
            .field("live", self.live)
            .finish_non_exhaustive()
    }
}

impl EventSink for TeeSink<'_> {
    fn emit(&self, event: Event) {
        if let Some(instrumentation) = self.instrumentation {
            instrumentation.emit(event.clone());
        }
        self.live.emit(event);
    }
}
