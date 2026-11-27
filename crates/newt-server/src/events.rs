//! The server's own events and where they go. A turn's events come from
//! the loop in newt-core; around them the server records what happens to a
//! session: an event accepted, a wake, a claim, a turn prepared and
//! committed, a compaction, and the runner's samples. Both kinds go to the
//! event log, in the order they happen; only a turn's go to the trace
//! exporter.

use std::sync::Arc;

use newt_core::event::Event;
use newt_core::sink::{EventSink, JsonlSink, LogLine};
use newt_otlp::OtlpSink;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ServerEvent {
    #[serde(with = "time::serde::rfc3339")]
    pub at: OffsetDateTime,
    #[serde(flatten)]
    pub kind: ServerEventKind,
}

impl ServerEvent {
    pub fn now(kind: ServerEventKind) -> Self {
        Self {
            at: OffsetDateTime::now_utc(),
            kind,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerEventKind {
    ServerReady {
        boot_ms: u64,
    },
    CompactionCompleted {
        session: String,
        turn: String,
        tokens_before: i64,
        tokens_after: i64,
    },
    EventAccepted {
        session: String,
        inbox_id: i64,
        accept_ms: u64,
        /// Of the time above, how long it waited for a database connection.
        #[serde(default)]
        pool_wait_ms: u64,
    },
    SessionWoken {
        session: String,
        source: WakeSource,
    },
    SessionClaimed {
        session: String,
        claimed: bool,
        claim_ms: u64,
        /// Of the time above, how long it waited for a database connection.
        #[serde(default)]
        pool_wait_ms: u64,
    },
    TurnPrepared {
        session: String,
        turn: String,
        prepare_ms: u64,
        history_ms: u64,
        /// Of the history time, how long it waited for a database connection.
        #[serde(default)]
        pool_wait_ms: u64,
    },
    TurnCommitted {
        session: String,
        turn: String,
        commit_ms: u64,
    },
    RunnerSample {
        running: usize,
        pool_size: u32,
        pool_idle: usize,
        sweep_ms: u64,
        woken: usize,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WakeSource {
    Notify,
    Sweep,
    Direct,
}

/// A line of the event log: one of a turn's events or one of the server's.
#[expect(
    clippy::large_enum_variant,
    reason = "a record only passes through the log's channel; boxing would allocate per event"
)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Record {
    Turn(Event),
    Server(ServerEvent),
}

impl From<Event> for Record {
    fn from(event: Event) -> Self {
        Self::Turn(event)
    }
}

impl LogLine for Record {
    fn logged(self, capture_content: bool) -> Option<Self> {
        match self {
            Self::Turn(event) => event.logged(capture_content).map(Self::Turn),
            Self::Server(event) => Some(Self::Server(event)),
        }
    }
}

/// Watches what the server does: a turn's events as the loop in newt-core
/// emits them, and the server's own events around them. An observer runs
/// on the session's task, so it must not block: one with slow work queues
/// it, as the event log does.
pub trait Observer: Send + Sync {
    /// A turn's event: a model call, a tool call, the turn's end.
    fn turn(&self, _event: &Event) {}
    /// A server event: an event accepted, a claim, a commit, a compaction.
    fn server(&self, _event: &ServerEvent) {}
}

/// The observers a server was built with, each told every event.
#[derive(Clone, Default)]
pub struct Observers(Vec<Arc<dyn Observer>>);

impl std::fmt::Debug for Observers {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_tuple("Observers")
            .field(&self.0.len())
            .finish()
    }
}

impl Observers {
    pub fn push(&mut self, observer: Arc<dyn Observer>) {
        self.0.push(observer);
    }

    pub const fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Tells every observer about one of the server's events.
    pub fn server(&self, kind: ServerEventKind) {
        if self.0.is_empty() {
            return;
        }
        let event = ServerEvent::now(kind);
        for observer in &self.0 {
            observer.server(&event);
        }
    }
}

impl EventSink for Observers {
    fn emit(&self, event: Event) {
        for observer in &self.0 {
            observer.turn(&event);
        }
    }
}

/// The event log: every turn event and every server event, in order.
impl Observer for JsonlSink<Record> {
    fn turn(&self, event: &Event) {
        self.emit(event.clone());
    }

    fn server(&self, event: &ServerEvent) {
        self.write(Record::Server(event.clone()));
    }
}

/// The trace exporter: a turn's events only.
impl Observer for OtlpSink {
    fn turn(&self, event: &Event) {
        self.emit(event.clone());
    }
}

#[cfg(test)]
mod tests {
    #![expect(clippy::unwrap_used, reason = "tests fail loudly on setup errors")]

    use newt_core::event::EventKind;

    use super::*;

    #[test]
    fn a_log_line_reads_back_as_the_event_it_was() {
        let turn = Record::Turn(Event::now(EventKind::TurnSteered {
            session: "assistant/a".to_owned(),
            turn: "t1".to_owned(),
            events: 2,
        }));
        let server = Record::Server(ServerEvent::now(ServerEventKind::SessionWoken {
            session: "assistant/a".to_owned(),
            source: WakeSource::Sweep,
        }));
        for record in [turn, server] {
            let line = serde_json::to_string(&record).unwrap();
            assert!(line.starts_with("{\"at\":"), "{line}");
            assert_eq!(serde_json::from_str::<Record>(&line).unwrap(), record);
        }
    }
}
