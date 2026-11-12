use std::sync::Arc;

use newt_core::event::{Event, EventKind};
use newt_core::sink::EventSink;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::store::Store;

const LIVE_BUFFER: usize = 256;

#[derive(Debug, Clone)]
pub struct LiveSink {
    sender: mpsc::Sender<Event>,
}

impl LiveSink {
    pub fn spawn(store: Store, agent: String, key: String) -> (Self, JoinHandle<()>) {
        let (sender, mut receiver) = mpsc::channel::<Event>(LIVE_BUFFER);
        let writer = tokio::spawn(async move {
            while let Some(event) = receiver.recv().await {
                let encoded = match serde_json::to_value(&event) {
                    Ok(encoded) => encoded,
                    Err(error) => {
                        tracing::error!(%error, "could not encode a live event");
                        continue;
                    }
                };
                if let Err(error) = store.append_live(&agent, &key, &encoded).await {
                    tracing::error!(%error, %agent, %key, "could not append a live event");
                }
            }
        });
        (Self { sender }, writer)
    }
}

fn is_live(kind: &EventKind) -> bool {
    matches!(
        kind,
        EventKind::TurnStarted { .. }
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
