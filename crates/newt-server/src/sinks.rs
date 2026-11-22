use std::sync::Arc;

use newt_core::event::{Event, EventKind};
use newt_core::sink::EventSink;
use std::time::{Duration, Instant};

use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::live::{Channel, Notification, Topics};
use crate::store::{Store, live_topic};

const LIVE_BUFFER: usize = 256;
const LIVE_BATCH: usize = 256;
const REMOTE_NOTIFY_EVERY: Duration = Duration::from_millis(200);

#[derive(Debug, Clone)]
pub struct LiveSink {
    sender: mpsc::Sender<Event>,
    stream: bool,
}

impl LiveSink {
    pub fn spawn(
        store: Store,
        local: Topics,
        agent: String,
        key: String,
        stream: bool,
    ) -> (Self, JoinHandle<()>) {
        let (sender, receiver) = mpsc::channel::<Event>(LIVE_BUFFER);
        let writer = tokio::spawn(write_live(store, local, agent, key, receiver));
        (Self { sender, stream }, writer)
    }
}

const MODEL_CALL_STARTED_FIELDS: [&str; 6] = ["type", "at", "session", "turn", "call", "model"];
const MODEL_CALL_FINISHED_FIELDS: [&str; 6] = ["type", "at", "session", "turn", "call", "stop"];

fn slim_model_call(mut frame: serde_json::Value) -> serde_json::Value {
    let keep: &[&str] = match frame["type"].as_str() {
        Some("model_call_started") => &MODEL_CALL_STARTED_FIELDS,
        Some("model_call_finished") => &MODEL_CALL_FINISHED_FIELDS,
        _ => return frame,
    };
    if let Some(fields) = frame.as_object_mut() {
        fields.retain(|name, _| keep.contains(&name.as_str()));
    }
    frame
}

fn encode(batch: &mut Vec<Event>) -> Vec<serde_json::Value> {
    batch
        .drain(..)
        .filter_map(|event| match serde_json::to_value(&event) {
            Ok(encoded) => Some(slim_model_call(encoded)),
            Err(error) => {
                tracing::error!(%error, "could not encode a live event");
                None
            }
        })
        .collect()
}

async fn write_live(
    store: Store,
    local: Topics,
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
                        local.publish(Notification {
                            channel: Channel::Live,
                            topic: topic.clone(),
                        });
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

fn is_live(kind: &EventKind, stream: bool) -> bool {
    if kind.is_live_only()
        || matches!(
            kind,
            EventKind::ModelCallStarted { .. } | EventKind::ModelCallFinished { .. }
        )
    {
        return stream;
    }
    matches!(
        kind,
        EventKind::TurnStarted { .. }
            | EventKind::TurnSteered { .. }
            | EventKind::ToolStarted { .. }
            | EventKind::ToolProgress { .. }
            | EventKind::ToolFinished { .. }
            | EventKind::TurnCompleted { .. }
            | EventKind::TurnFailed { .. }
    )
}

impl EventSink for LiveSink {
    fn emit(&self, event: Event) {
        if is_live(&event.kind, self.stream) && self.sender.try_send(event).is_err() {
            tracing::warn!("live feed buffer full or closed, event dropped");
        }
    }
}

#[derive(Clone, Copy)]
pub struct TeeSink<'a> {
    pub live: &'a LiveSink,
    pub instrumentation: Option<&'a Arc<dyn EventSink>>,
    /// Called on every model call the turn starts.
    pub on_model_call: Option<&'a (dyn Fn() + Sync)>,
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
        if let (Some(on_model_call), EventKind::ModelCallStarted { .. }) =
            (self.on_model_call, &event.kind)
        {
            on_model_call();
        }
        if let Some(instrumentation) = self.instrumentation
            && !event.kind.is_live_only()
        {
            instrumentation.emit(event.clone());
        }
        self.live.emit(event);
    }
}
