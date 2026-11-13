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

fn empty_message() -> Message {
    Message {
        role: crate::message::Role::Assistant,
        parts: Vec::new(),
    }
}

impl Event {
    pub fn now(kind: EventKind) -> Self {
        Self {
            at: OffsetDateTime::now_utc(),
            kind,
        }
    }

    #[must_use]
    pub fn without_content(mut self) -> Self {
        match &mut self.kind {
            EventKind::ModelCallStarted { input, .. } => input.clear(),
            EventKind::ModelCallFinished { output, .. } => output.parts.clear(),
            _ => {}
        }
        self
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
        #[serde(default)]
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
        #[serde(default = "empty_message")]
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
    ToolProgress {
        session: String,
        turn: String,
        call_id: String,
        name: String,
        message: String,
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
    EventAccepted {
        session: String,
        inbox_id: i64,
        accept_ms: u64,
    },
    SessionWoken {
        session: String,
        source: WakeSource,
    },
    SessionClaimed {
        session: String,
        claimed: bool,
        claim_ms: u64,
    },
    TurnPrepared {
        session: String,
        turn: String,
        prepare_ms: u64,
        history_ms: u64,
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
