use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InboundKind {
    #[default]
    Message,
    Signal,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Policy {
    #[default]
    Steer,
    Queue,
    Collect,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Priority {
    #[default]
    Interactive,
    Background,
}

impl InboundEvent {
    /// A sha256 over the event's content, `text` and `meta`, with object keys
    /// sorted so the same content always hashes the same.
    #[must_use]
    pub fn content_hash(&self) -> String {
        use sha2::{Digest, Sha256};
        use std::fmt::Write as _;
        let mut canonical = String::new();
        push_canonical(
            &json!({ "text": self.text, "meta": self.meta }),
            &mut canonical,
        );
        Sha256::digest(canonical.as_bytes())
            .iter()
            .fold(String::new(), |mut hex, byte| {
                let _ = write!(hex, "{byte:02x}");
                hex
            })
    }
}

fn push_canonical(value: &Value, out: &mut String) {
    match value {
        Value::Object(fields) => {
            let mut keys: Vec<&String> = fields.keys().collect();
            keys.sort();
            out.push('{');
            for (index, key) in keys.into_iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                out.push_str(&Value::String(key.clone()).to_string());
                out.push(':');
                push_canonical(&fields[key], out);
            }
            out.push('}');
        }
        Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                push_canonical(item, out);
            }
            out.push(']');
        }
        other => out.push_str(&other.to_string()),
    }
}

impl InboundKind {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Message => "message",
            Self::Signal => "signal",
        }
    }
}

impl Policy {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Steer => "steer",
            Self::Queue => "queue",
            Self::Collect => "collect",
        }
    }
}

impl Priority {
    pub(super) fn rank(self) -> i16 {
        match self {
            Self::Interactive => 0,
            Self::Background => 1,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InboundEvent {
    pub id: String,
    #[serde(default)]
    pub kind: InboundKind,
    #[serde(default)]
    pub policy: Policy,
    #[serde(default)]
    pub priority: Priority,
    pub text: String,
    #[serde(default)]
    pub meta: Option<Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Accepted {
    pub inbox_id: i64,
    pub duplicate: bool,
    /// Milliseconds spent waiting for a database connection.
    #[serde(skip)]
    pub pool_wait_ms: u64,
}

/// How many unconsumed events a session may hold when its agent sets no cap.
pub const DEFAULT_MAX_PENDING_EVENTS: usize = 100;

#[derive(Debug, thiserror::Error)]
pub enum AcceptError {
    #[error("the session already holds {max_pending} unconsumed events")]
    Full { max_pending: i64 },
    #[error("an event with this id and different content was already accepted")]
    Conflict,
    #[error(transparent)]
    Database(#[from] sqlx::Error),
}

#[cfg(test)]
mod regressions;

#[cfg(test)]
mod tests {
    #![expect(clippy::unwrap_used, reason = "tests fail loudly on setup errors")]

    use super::*;

    fn event(body: Value) -> InboundEvent {
        serde_json::from_value(body).unwrap()
    }

    #[test]
    fn the_hash_ignores_key_order_and_the_id_but_not_content() {
        let base = event(
            json!({ "id": "a", "text": "hi", "meta": { "x": 1, "y": [1, { "b": 2, "a": 3 }] } }),
        );
        let reordered = event(
            json!({ "id": "b", "meta": { "y": [1, { "a": 3, "b": 2 }], "x": 1 }, "text": "hi" }),
        );
        assert_eq!(base.content_hash(), reordered.content_hash());
        assert_eq!(base.content_hash().len(), 64);

        let other_text = event(
            json!({ "id": "a", "text": "ho", "meta": { "x": 1, "y": [1, { "b": 2, "a": 3 }] } }),
        );
        let other_meta = event(
            json!({ "id": "a", "text": "hi", "meta": { "x": 2, "y": [1, { "b": 2, "a": 3 }] } }),
        );
        let no_meta = event(json!({ "id": "a", "text": "hi" }));
        let hashes = [
            base.content_hash(),
            other_text.content_hash(),
            other_meta.content_hash(),
            no_meta.content_hash(),
        ];
        for (index, hash) in hashes.iter().enumerate() {
            assert_eq!(
                hashes.iter().filter(|other| *other == hash).count(),
                1,
                "{index}"
            );
        }
    }

    #[test]
    fn a_missing_meta_and_a_null_meta_hash_alike() {
        let missing = event(json!({ "id": "a", "text": "hi" }));
        let null = event(json!({ "id": "a", "text": "hi", "meta": null }));
        assert_eq!(missing.content_hash(), null.content_hash());
    }
}
