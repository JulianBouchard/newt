use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::fs::OpenOptions;
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::event::{Event, EventKind};

pub trait EventSink: Send + Sync {
    fn emit(&self, event: Event);
}

#[derive(Clone, Default)]
pub struct MultiSink(pub Vec<Arc<dyn EventSink>>);

impl std::fmt::Debug for MultiSink {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_tuple("MultiSink")
            .field(&self.0.len())
            .finish()
    }
}

impl EventSink for MultiSink {
    fn emit(&self, event: Event) {
        if let Some((last, rest)) = self.0.split_last() {
            for sink in rest {
                sink.emit(event.clone());
            }
            last.emit(event);
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct NoSink;

impl EventSink for NoSink {
    fn emit(&self, _event: Event) {}
}

#[derive(Debug, thiserror::Error)]
pub enum SinkError {
    #[error("event sink io failed")]
    Io(#[from] std::io::Error),
    #[error("event could not be serialized")]
    Serialize(#[from] serde_json::Error),
}

#[derive(Debug, Clone)]
pub struct JsonlSink {
    sender: mpsc::Sender<Event>,
    dropped: Arc<AtomicU64>,
    capture_content: bool,
}

impl JsonlSink {
    pub fn spawn(path: PathBuf, capacity: usize) -> (Self, JoinHandle<Result<(), SinkError>>) {
        let (sender, receiver) = mpsc::channel(capacity);
        let writer = tokio::spawn(write_lines(path, receiver));
        let sink = Self {
            sender,
            dropped: Arc::new(AtomicU64::new(0)),
            capture_content: false,
        };
        (sink, writer)
    }

    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    #[must_use]
    pub fn capturing_content(mut self, capture_content: bool) -> Self {
        self.capture_content = capture_content;
        self
    }
}

impl EventSink for JsonlSink {
    fn emit(&self, event: Event) {
        if matches!(event.kind, EventKind::TextDelta { .. }) {
            return;
        }
        let event = if self.capture_content {
            event
        } else {
            event.without_content()
        };
        if self.sender.try_send(event).is_err() {
            let total = self.dropped.fetch_add(1, Ordering::Relaxed) + 1;
            tracing::warn!(dropped = total, "jsonl sink full or closed, event dropped");
        }
    }
}

async fn write_lines(path: PathBuf, mut receiver: mpsc::Receiver<Event>) -> Result<(), SinkError> {
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .await?;
    while let Some(event) = receiver.recv().await {
        let mut line = serde_json::to_vec(&event)?;
        line.push(b'\n');
        file.write_all(&line).await?;
        file.flush().await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![expect(clippy::unwrap_used, reason = "tests fail loudly on setup errors")]

    use super::*;
    use crate::event::EventKind;

    #[tokio::test]
    async fn writes_one_json_line_per_event_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let (sink, writer) = JsonlSink::spawn(path.clone(), 8);

        sink.emit(Event::now(EventKind::ServerReady { boot_ms: 7 }));
        sink.emit(Event::now(EventKind::ServerReady { boot_ms: 9 }));
        drop(sink);
        writer.await.unwrap().unwrap();

        let contents = std::fs::read_to_string(&path).unwrap();
        let boot_times: Vec<u64> = contents
            .lines()
            .filter_map(
                |line| match serde_json::from_str::<Event>(line).unwrap().kind {
                    EventKind::ServerReady { boot_ms } => Some(boot_ms),
                    _ => None,
                },
            )
            .collect();
        assert_eq!(boot_times, vec![7, 9]);
    }

    #[tokio::test]
    async fn counts_events_dropped_when_the_buffer_is_full() {
        let (sender, _receiver) = mpsc::channel(1);
        let sink = JsonlSink {
            sender,
            dropped: Arc::new(AtomicU64::new(0)),
            capture_content: false,
        };

        sink.emit(Event::now(EventKind::ServerReady { boot_ms: 1 }));
        sink.emit(Event::now(EventKind::ServerReady { boot_ms: 2 }));
        sink.emit(Event::now(EventKind::ServerReady { boot_ms: 3 }));

        assert_eq!(sink.dropped(), 2);
    }
}
