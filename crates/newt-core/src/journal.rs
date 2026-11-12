use std::collections::{HashMap, HashSet, VecDeque};
use std::future::Future;

use serde::{Deserialize, Serialize};

use crate::message::Message;
use crate::provider::{StopReason, Usage};
use crate::steering::Steered;
use crate::tools::ToolOutput;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "entry", rename_all = "snake_case")]
pub enum JournalEntry {
    Steered {
        id: String,
        message: Message,
    },
    ModelResponse {
        call: u32,
        message: Message,
        usage: Usage,
        stop: StopReason,
    },
    OutwardIntent {
        call_id: String,
    },
    ToolOutput {
        call_id: String,
        output: ToolOutput,
    },
}

#[derive(Debug, thiserror::Error)]
#[error("could not write the turn journal: {0}")]
pub struct JournalError(pub String);

pub trait Journal: Send + Sync {
    fn append(&self, entry: JournalEntry);
    fn append_and_wait(
        &self,
        entry: JournalEntry,
    ) -> impl Future<Output = Result<(), JournalError>> + Send;
}

#[derive(Debug, Clone, Copy, Default)]
pub struct NoJournal;

impl Journal for NoJournal {
    fn append(&self, _entry: JournalEntry) {}

    fn append_and_wait(
        &self,
        _entry: JournalEntry,
    ) -> impl Future<Output = Result<(), JournalError>> + Send {
        std::future::ready(Ok(()))
    }
}

#[derive(Debug, Default)]
pub(crate) struct Replay {
    sequence: VecDeque<JournalEntry>,
    outputs: HashMap<String, ToolOutput>,
    intents: HashSet<String>,
}

impl Replay {
    pub(crate) fn new(entries: Vec<JournalEntry>) -> Self {
        let mut replay = Self::default();
        for entry in entries {
            match entry {
                JournalEntry::ToolOutput { call_id, output } => {
                    replay.outputs.insert(call_id, output);
                }
                JournalEntry::OutwardIntent { call_id } => {
                    replay.intents.insert(call_id);
                }
                ordered @ (JournalEntry::Steered { .. } | JournalEntry::ModelResponse { .. }) => {
                    replay.sequence.push_back(ordered);
                }
            }
        }
        replay
    }

    pub(crate) fn exhausted(&self) -> bool {
        self.sequence.is_empty()
    }

    pub(crate) fn take_steered(&mut self) -> Vec<Steered> {
        let mut steered = Vec::new();
        while let Some(JournalEntry::Steered { .. }) = self.sequence.front() {
            if let Some(JournalEntry::Steered { id, message }) = self.sequence.pop_front() {
                steered.push(Steered { id, message });
            }
        }
        steered
    }

    pub(crate) fn take_response(&mut self) -> Option<(Message, Usage, StopReason)> {
        if !matches!(
            self.sequence.front(),
            Some(JournalEntry::ModelResponse { .. })
        ) {
            return None;
        }
        match self.sequence.pop_front() {
            Some(JournalEntry::ModelResponse {
                message,
                usage,
                stop,
                ..
            }) => Some((message, usage, stop)),
            _ => None,
        }
    }

    pub(crate) fn output(&self, call_id: &str) -> Option<&ToolOutput> {
        self.outputs.get(call_id)
    }

    pub(crate) fn has_intent(&self, call_id: &str) -> bool {
        self.intents.contains(call_id)
    }
}
