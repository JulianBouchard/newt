use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::message::Message;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Effort {
    Low,
    Medium,
    High,
}

#[derive(Debug, Clone, Copy)]
pub struct Prompt<'a> {
    pub system: &'a str,
    pub tools: &'a [ToolSpec],
    pub history: &'a [Message],
    pub scratch: &'a [Message],
}

impl Prompt<'_> {
    pub fn messages(&self) -> impl Iterator<Item = &Message> {
        self.history.iter().chain(self.scratch)
    }

    pub fn last_history_index(&self) -> Option<usize> {
        self.history.len().checked_sub(1)
    }

    pub fn last_message_index(&self) -> Option<usize> {
        (self.history.len() + self.scratch.len()).checked_sub(1)
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ModelRequest<'a> {
    pub model: &'a str,
    pub prompt: Prompt<'a>,
    pub effort: Option<Effort>,
    pub max_output_tokens: u32,
    pub cache_key: &'a str,
}
