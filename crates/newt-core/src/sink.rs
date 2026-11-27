use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::Serialize;
use tokio::fs::OpenOptions;
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::event::Event;

pub trait EventSink: Send + Sync {
    fn emit(&self, event: Event);
}

impl<T: EventSink + ?Sized> EventSink for Arc<T> {
    fn emit(&self, event: Event) {
        (**self).emit(event);
    }
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

/// A line of a JSONL event log.
pub trait LogLine: Serialize + Send + Sized + 'static {
    /// The line as written, or `None` to leave it out of the log. Without
    /// `capture_content` it carries no prompt or reply content.
    fn logged(self, capture_content: bool) -> Option<Self>;
}

impl LogLine for Event {
    fn logged(self, capture_content: bool) -> Option<Self> {
        if self.kind.is_live_only() {
            return None;
        }
        Some(if capture_content {
            self
        } else {
            self.without_content()
        })
    }
}

/// Appends lines to a JSONL file from a background task. Lines that find
/// the buffer full are dropped and counted.
#[derive(Debug)]
pub struct JsonlSink<L = Event> {
    sender: mpsc::Sender<L>,
    dropped: Arc<AtomicU64>,
    capture_content: bool,
}

impl<L> Clone for JsonlSink<L> {
    fn clone(&self) -> Self {
        Self {
            sender: self.sender.clone(),
            dropped: Arc::clone(&self.dropped),
            capture_content: self.capture_content,
        }
    }
}

impl<L: LogLine> JsonlSink<L> {
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

    /// Writes `line`, unless [`LogLine::logged`] leaves it out.
    pub fn write(&self, line: L) {
        let Some(line) = line.logged(self.capture_content) else {
            return;
        };
        if self.sender.try_send(line).is_err() {
            let total = self.dropped.fetch_add(1, Ordering::Relaxed) + 1;
            tracing::warn!(dropped = total, "jsonl sink full or closed, event dropped");
        }
    }
}

impl<L: LogLine + From<Event>> EventSink for JsonlSink<L> {
    fn emit(&self, event: Event) {
        self.write(L::from(event));
    }
}

async fn write_lines<L: Serialize>(
    path: PathBuf,
    mut receiver: mpsc::Receiver<L>,
) -> Result<(), SinkError> {
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

    fn steered(events: usize) -> Event {
        Event::now(EventKind::TurnSteered {
            session: "assistant/a".to_owned(),
            turn: "t1".to_owned(),
            events,
        })
    }

    #[tokio::test]
    async fn writes_one_json_line_per_event_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let (sink, writer) = JsonlSink::<Event>::spawn(path.clone(), 8);

        sink.emit(steered(7));
        sink.emit(steered(9));
        drop(sink);
        writer.await.unwrap().unwrap();

        let contents = std::fs::read_to_string(&path).unwrap();
        let counts: Vec<usize> = contents
            .lines()
            .filter_map(
                |line| match serde_json::from_str::<Event>(line).unwrap().kind {
                    EventKind::TurnSteered { events, .. } => Some(events),
                    _ => None,
                },
            )
            .collect();
        assert_eq!(counts, vec![7, 9]);
    }

    #[tokio::test]
    async fn counts_events_dropped_when_the_buffer_is_full() {
        let (sender, _receiver) = mpsc::channel(1);
        let sink: JsonlSink = JsonlSink {
            sender,
            dropped: Arc::new(AtomicU64::new(0)),
            capture_content: false,
        };

        sink.emit(steered(1));
        sink.emit(steered(2));
        sink.emit(steered(3));

        assert_eq!(sink.dropped(), 2);
    }
}
