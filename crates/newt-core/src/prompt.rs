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

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Lane {
    #[default]
    Interactive,
    Background,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LayerFingerprint {
    pub hash: String,
    pub bytes: usize,
    pub items: usize,
    /// The layer piece by piece, in prompt order: the system prompt and each
    /// tool for the prefix, each message for history. Lets a report name the
    /// piece that changed when the cache missed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub parts: Vec<PartFingerprint>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PartFingerprint {
    /// Empty for a history message, which its position names.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
    pub hash: String,
    pub bytes: usize,
}

impl PartFingerprint {
    fn of<T: Serialize + ?Sized>(name: impl Into<String>, value: &T) -> Self {
        let LayerFingerprint { hash, bytes, .. } = LayerFingerprint::of(value, 1);
        Self {
            name: name.into(),
            hash,
            bytes,
        }
    }
}

impl LayerFingerprint {
    pub fn of<T: Serialize + ?Sized>(value: &T, items: usize) -> Self {
        match serde_json::to_vec(value) {
            Ok(bytes) => Self {
                hash: format!("{:016x}", fnv1a(&bytes)),
                bytes: bytes.len(),
                items,
                parts: Vec::new(),
            },
            Err(error) => {
                tracing::warn!(%error, "could not fingerprint a prompt layer");
                Self {
                    hash: "unserializable".to_owned(),
                    bytes: 0,
                    items,
                    parts: Vec::new(),
                }
            }
        }
    }
}

pub(crate) fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x0100_0000_01b3)
    })
}

#[derive(Debug, Clone, Copy)]
pub struct Prompt<'a> {
    pub system: &'a str,
    pub tools: &'a [ToolSpec],
    pub history: &'a [Message],
    pub scratch: &'a [Message],
}

impl Prompt<'_> {
    pub fn prefix_fingerprint(&self) -> LayerFingerprint {
        let mut layer = LayerFingerprint::of(&(self.system, self.tools), self.tools.len());
        layer.parts = std::iter::once(PartFingerprint::of("system", self.system))
            .chain(
                self.tools
                    .iter()
                    .map(|tool| PartFingerprint::of(format!("tool `{}`", tool.name), tool)),
            )
            .collect();
        layer
    }

    pub fn history_fingerprint(&self) -> LayerFingerprint {
        let mut layer = LayerFingerprint::of(self.history, self.history.len());
        layer.parts = self
            .history
            .iter()
            .map(|message| PartFingerprint::of(String::new(), message))
            .collect();
        layer
    }

    pub fn scratch_fingerprint(&self) -> LayerFingerprint {
        LayerFingerprint::of(self.scratch, self.scratch.len())
    }

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
    pub lane: Lane,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::Message;

    #[test]
    fn fingerprints_are_stable_and_change_with_content() {
        let first = [Message::user_text("hi")];
        let same = [Message::user_text("hi")];
        let other = [Message::user_text("hi!")];
        assert_eq!(
            LayerFingerprint::of(&first[..], 1),
            LayerFingerprint::of(&same[..], 1)
        );
        assert_ne!(
            LayerFingerprint::of(&first[..], 1).hash,
            LayerFingerprint::of(&other[..], 1).hash
        );
    }
}
