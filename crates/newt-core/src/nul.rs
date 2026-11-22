//! Postgres refuses the NUL character in text and in jsonb, so a NUL that a model
//! or a tool produces would make every commit of its turn fail. Model output and
//! tool output are cleaned where they enter the turn: each NUL becomes U+FFFD.

use serde_json::{Map, Value};

use crate::message::{Message, Part, Reasoning, ToolCall, ToolResult};
use crate::provider::StreamEvent;
use crate::tools::ToolOutput;

const REPLACEMENT: &str = "\u{FFFD}";

fn clean(text: &mut String) {
    if text.contains('\0') {
        *text = text.replace('\0', REPLACEMENT);
    }
}

pub(crate) fn clean_text(text: String) -> String {
    let mut text = text;
    clean(&mut text);
    text
}

fn clean_value(value: &mut Value) {
    match value {
        Value::String(text) => clean(text),
        Value::Array(items) => items.iter_mut().for_each(clean_value),
        Value::Object(fields) => {
            if fields.keys().any(|key| key.contains('\0')) {
                let cleaned: Map<String, Value> = std::mem::take(fields)
                    .into_iter()
                    .map(|(key, value)| (clean_text(key), value))
                    .collect();
                *fields = cleaned;
            }
            fields.values_mut().for_each(clean_value);
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

fn clean_call_in_place(call: &mut ToolCall) {
    clean(&mut call.id);
    clean(&mut call.name);
    clean_value(&mut call.arguments);
}

pub(crate) fn clean_event(event: StreamEvent) -> StreamEvent {
    match event {
        StreamEvent::TextDelta(delta) => StreamEvent::TextDelta(clean_text(delta)),
        StreamEvent::ToolCallStarted { id, name } => StreamEvent::ToolCallStarted {
            id: clean_text(id),
            name: clean_text(name),
        },
        StreamEvent::ToolInputDelta { id, delta } => StreamEvent::ToolInputDelta {
            id: clean_text(id),
            delta: clean_text(delta),
        },
        StreamEvent::ToolCallReady(mut call) => {
            clean_call_in_place(&mut call);
            StreamEvent::ToolCallReady(call)
        }
        StreamEvent::OutputStarted => StreamEvent::OutputStarted,
    }
}

pub(crate) fn clean_message(mut message: Message) -> Message {
    for part in &mut message.parts {
        match part {
            Part::Text { text } => clean(text),
            Part::ToolCall(call) => clean_call_in_place(call),
            Part::ToolResult(ToolResult {
                call_id, content, ..
            }) => {
                clean(call_id);
                clean(content);
            }
            Part::Reasoning(Reasoning::AnthropicThinking {
                thinking,
                signature,
            }) => {
                clean(thinking);
                clean(signature);
            }
            Part::Reasoning(Reasoning::AnthropicRedacted { data }) => clean(data),
            Part::Reasoning(Reasoning::OpenaiReasoning { item }) => clean_value(item),
        }
    }
    message
}

pub(crate) fn clean_output(mut output: ToolOutput) -> ToolOutput {
    clean(&mut output.content);
    if let Some(summary) = &mut output.summary {
        clean(summary);
    }
    for page in &mut output.pages {
        clean(&mut page.path);
        clean(&mut page.version);
        clean(&mut page.content);
    }
    output
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn every_nul_in_a_message_becomes_the_replacement_character() {
        let message = clean_message(Message {
            role: crate::message::Role::Assistant,
            parts: vec![
                Part::text("a\0b"),
                Part::ToolCall(ToolCall {
                    id: "c\0".into(),
                    name: "send".into(),
                    arguments: json!({ "k\0": ["x\0", { "y": "\0" }] }),
                }),
            ],
        });
        let encoded = serde_json::to_string(&message).unwrap_or_default();
        assert!(!encoded.contains("\\u0000"), "{encoded}");
        assert!(encoded.contains("a\u{FFFD}b"));
    }

    #[test]
    fn text_without_nul_is_left_alone() {
        let output = clean_output(ToolOutput::ok("plain \\u0000 text"));
        assert_eq!(output.content, "plain \\u0000 text");
    }
}
