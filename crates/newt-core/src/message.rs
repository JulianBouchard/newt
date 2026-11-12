use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const INVALID_ARGUMENTS_KEY: &str = "_newt_invalid_arguments";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    User,
    Assistant,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub parts: Vec<Part>,
}

impl Message {
    pub fn user_text(text: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            parts: vec![Part::text(text)],
        }
    }

    pub fn tool_calls(&self) -> impl Iterator<Item = &ToolCall> {
        self.parts.iter().filter_map(|part| match part {
            Part::ToolCall(call) => Some(call),
            _ => None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Part {
    Text { text: String },
    ToolCall(ToolCall),
    ToolResult(ToolResult),
    Reasoning(Reasoning),
}

impl Part {
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text { text: text.into() }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Value,
}

impl ToolCall {
    pub fn invalid_arguments(&self) -> Option<&str> {
        self.arguments
            .get(INVALID_ARGUMENTS_KEY)
            .and_then(Value::as_str)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolResult {
    pub call_id: String,
    pub content: String,
    pub is_error: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "provider", rename_all = "snake_case")]
pub enum Reasoning {
    AnthropicThinking { thinking: String, signature: String },
    AnthropicRedacted { data: String },
    OpenaiReasoning { item: Value },
}

#[cfg(test)]
mod tests {
    #![expect(clippy::unwrap_used, reason = "tests fail loudly on setup errors")]

    use serde_json::json;

    use super::*;

    #[test]
    fn every_part_round_trips_through_json() {
        let message = Message {
            role: Role::Assistant,
            parts: vec![
                Part::text("hello"),
                Part::ToolCall(ToolCall {
                    id: "c1".into(),
                    name: "read_file".into(),
                    arguments: json!({ "paths": ["a.md"] }),
                }),
                Part::ToolResult(ToolResult {
                    call_id: "c1".into(),
                    content: "body".into(),
                    is_error: false,
                }),
                Part::Reasoning(Reasoning::AnthropicThinking {
                    thinking: "t".into(),
                    signature: "s".into(),
                }),
                Part::Reasoning(Reasoning::AnthropicRedacted { data: "d".into() }),
                Part::Reasoning(Reasoning::OpenaiReasoning {
                    item: json!({ "type": "reasoning", "id": "rs_1" }),
                }),
            ],
        };
        let encoded = serde_json::to_string(&message).unwrap();
        assert_eq!(serde_json::from_str::<Message>(&encoded).unwrap(), message);
    }
}
