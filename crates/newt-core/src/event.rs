use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::message::Message;
use crate::prompt::{Effort, LayerFingerprint};
use crate::provider::{CallTiming, StopReason, Usage};
use crate::tools::Retention;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    #[serde(with = "time::serde::rfc3339")]
    pub at: OffsetDateTime,
    #[serde(flatten)]
    pub kind: EventKind,
}

impl Event {
    pub fn now(kind: EventKind) -> Self {
        Self {
            at: OffsetDateTime::now_utc(),
            kind,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EventKind {
    ServerReady {
        boot_ms: u64,
    },
    TurnStarted {
        session: String,
        turn: String,
        history_messages: usize,
        events: usize,
        #[serde(default)]
        compaction: bool,
    },
    ModelCallStarted {
        session: String,
        turn: String,
        call: u32,
        model: String,
        effort: Option<Effort>,
        prefix: LayerFingerprint,
        history: LayerFingerprint,
        scratch: LayerFingerprint,
        input: Vec<Message>,
    },
    ModelCallFinished {
        session: String,
        turn: String,
        call: u32,
        usage: Usage,
        timing: CallTiming,
        stop: StopReason,
        response_id: Option<String>,
        request_ids: BTreeMap<String, String>,
        output: Message,
    },
    ModelCallReplayed {
        session: String,
        turn: String,
        call: u32,
    },
    ModelCallInterrupted {
        session: String,
        turn: String,
        call: u32,
    },
    TurnSteered {
        session: String,
        turn: String,
        events: usize,
    },
    ModelCallFailed {
        session: String,
        turn: String,
        call: u32,
        error: String,
    },
    ToolStarted {
        session: String,
        turn: String,
        call_id: String,
        name: String,
    },
    ToolFinished {
        session: String,
        turn: String,
        call_id: String,
        name: String,
        retention: Retention,
        is_error: bool,
        ms: u64,
    },
    TurnCompleted {
        session: String,
        turn: String,
        model_calls: u32,
        committed_messages: usize,
    },
    TurnFailed {
        session: String,
        turn: String,
        error: String,
        committed_messages: usize,
    },
    CompactionCompleted {
        session: String,
        turn: String,
        tokens_before: i64,
        tokens_after: i64,
    },
}
